//! Collection-aware HTTP transport. Credentials are accepted only in headers.
use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{
        rejection::{JsonRejection, PathRejection, QueryRejection},
        ConnectInfo, DefaultBodyLimit, Path, Query, Request, State,
    },
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{AuthError, RequestContext},
    config::ServerMode,
    policy::PolicyError,
    service::{is_record_id, DeleteOptions, ReportOptions, Service, ServiceError, WriteOptions},
    store::{Collection, Metadata, StoreError},
};

mod transfer;

pub struct ApiError(StatusCode, &'static str, &'static str);
impl ApiError {
    fn invalid() -> Self {
        Self(StatusCode::BAD_REQUEST, "invalid_input", "invalid request")
    }
    fn validation() -> Self {
        Self(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "request does not match the schema",
        )
    }
    fn unauthenticated() -> Self {
        Self(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "a valid credential is required",
        )
    }
    fn not_found() -> Self {
        Self(
            StatusCode::NOT_FOUND,
            "not_found",
            "requested resource is unavailable",
        )
    }
    fn unavailable() -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "service is unavailable",
        )
    }
}
impl From<ServiceError> for ApiError {
    fn from(error: ServiceError) -> Self {
        match error {
            ServiceError::Store(StoreError::ExportChanged) => Self(
                StatusCode::CONFLICT,
                "export_changed",
                "collection changed during export",
            ),
            ServiceError::Busy => Self(
                StatusCode::TOO_MANY_REQUESTS,
                "resource_limit",
                "collection execution capacity is busy",
            ),
            ServiceError::Store(StoreError::IdempotencyConflict) => Self(
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "idempotency key was used with different input",
            ),
            ServiceError::Policy(PolicyError::Authentication(AuthError::Unauthenticated)) => {
                Self::unauthenticated()
            }
            ServiceError::Policy(PolicyError::Unavailable)
            | ServiceError::Store(
                StoreError::RecordNotFound { .. }
                | StoreError::SearchNotFound { .. }
                | StoreError::CollectionNotFound(_),
            ) => Self::not_found(),
            ServiceError::InvalidInput(_) => Self::invalid(),
            ServiceError::Store(StoreError::MutationPurged) => Self(
                StatusCode::GONE,
                "mutation_purged",
                "mutation content was purged",
            ),
            ServiceError::Store(StoreError::RecordDeleted { .. }) => {
                Self(StatusCode::GONE, "record_deleted", "record was deleted")
            }
            ServiceError::Store(StoreError::RevisionRequired) => Self(
                StatusCode::PRECONDITION_REQUIRED,
                "revision_required",
                "a revision precondition is required",
            ),
            ServiceError::Store(StoreError::RevisionConflict) => Self(
                StatusCode::CONFLICT,
                "revision_conflict",
                "revision precondition did not match",
            ),
            _ => Self::unavailable(),
        }
    }
}
impl From<JsonRejection> for ApiError {
    fn from(error: JsonRejection) -> Self {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Self(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "request body is too large",
            )
        } else {
            Self::validation()
        }
    }
}
impl From<QueryRejection> for ApiError {
    fn from(_: QueryRejection) -> Self {
        Self::validation()
    }
}
impl From<PathRejection> for ApiError {
    fn from(_: PathRejection) -> Self {
        Self::invalid()
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({"error": {"code": self.1, "message": self.2}})),
        )
            .into_response()
    }
}
type ApiResult<T> = Result<T, ApiError>;

async fn blocking<T: Send + 'static>(
    service: &Arc<Service>,
    call: impl FnOnce(&Service) -> Result<T, ServiceError> + Send + 'static,
) -> ApiResult<T> {
    let service = service.clone();
    tokio::task::spawn_blocking(move || call(&service))
        .await
        .map_err(|_| ApiError::unavailable())?
        .map_err(Into::into)
}

/// Missing credentials have meaning only in the explicitly selected local mode.
/// Supplied invalid credentials never fall back to local or anonymous authority.
async fn authenticate(
    State(service): State<Arc<Service>>,
    mut request: Request,
    next: Next,
) -> ApiResult<Response> {
    if service.settings().mode == ServerMode::Hosted
        || !request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|peer| peer.0.ip().is_loopback())
    {
        return Err(ApiError::unavailable());
    }
    if request
        .headers()
        .keys()
        .any(|name| name == "forwarded" || name.as_str().starts_with("x-forwarded-"))
    {
        return Err(ApiError::invalid());
    }
    let headers: Vec<_> = request.headers().get_all(AUTHORIZATION).iter().collect();
    let context = match headers.as_slice() {
        [] => match service.settings().mode {
            ServerMode::Local => RequestContext::local(),
            ServerMode::Authenticated => RequestContext::anonymous(),
            ServerMode::Hosted => return Err(ApiError::unavailable()),
        },
        [header] => {
            let value = header.to_str().map_err(|_| ApiError::unauthenticated())?;
            let (scheme, token) = value
                .split_once(' ')
                .ok_or_else(ApiError::unauthenticated)?;
            if !scheme.eq_ignore_ascii_case("bearer")
                || token.len() > 256
                || token.is_empty()
                || token.bytes().any(|b| b.is_ascii_whitespace())
            {
                return Err(ApiError::unauthenticated());
            }
            let token = token.to_owned();
            blocking(&service, move |service| {
                service
                    .authenticate(&token)
                    .map_err(|e| ServiceError::Policy(PolicyError::Authentication(e)))
            })
            .await?
        }
        _ => return Err(ApiError::unauthenticated()),
    };
    request.headers_mut().remove(AUTHORIZATION);
    if !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) && context.principal_id().is_none()
    {
        // Public search is the only anonymous POST operation.
        if request.uri().path() != "/v1/search" {
            return Err(ApiError::unauthenticated());
        }
    }
    let mut keys = request.headers().get_all("idempotency-key").iter();
    let key = keys
        .next()
        .map(|h| h.to_str().map(str::to_owned))
        .transpose()
        .map_err(|_| ApiError::invalid())?;
    if keys.next().is_some()
        || key.as_ref().is_some_and(|k| {
            k.is_empty()
                || k.len() > 128
                || !k
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
        })
    {
        return Err(ApiError::invalid());
    }
    request.extensions_mut().insert(IdempotencyKey(key));
    request.extensions_mut().insert(context);
    Ok(next.run(request).await)
}

#[derive(Clone)]
struct IdempotencyKey(Option<String>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutRequest {
    text: String,
    metadata: Option<Metadata>,
    id: Option<String>,
    expected_revision: Option<i64>,
    #[serde(default)]
    publish: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionQuery {
    revision: Option<i64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuery {
    expected_revision: Option<i64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyQuery {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(default)]
    after: String,
    #[serde(default = "default_page_size")]
    limit: i64,
}
fn default_page_size() -> i64 {
    50
}
fn default_limit() -> i64 {
    10
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchRequest {
    collections: Vec<String>,
    text: String,
    filters: Option<Metadata>,
    #[serde(default = "default_limit")]
    limit: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportRequest {
    record_id: String,
    text: String,
    revision: i64,
    search_id: Option<String>,
}

pub fn router(service: Arc<Service>) -> Router {
    let limit = service
        .settings()
        .max_text_bytes
        .saturating_mul(6)
        .saturating_add(65_536);
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/collections", get(collections))
        .route(
            "/v1/collections/{collection}",
            get(collection).delete(delete_collection),
        )
        .route("/v1/collections/{collection}/diagnostics", get(diagnostics))
        .route("/v1/collections/{collection}/records", post(put_record))
        .route(
            "/v1/collections/{collection}/records/{id}",
            get(get_record).delete(delete_record),
        )
        .route(
            "/v1/collections/{collection}/records/{id}/reports",
            get(list_reports),
        )
        .route("/v1/collections/{collection}/reports", post(report))
        .route(
            "/v1/collections/{collection}/reports/{id}/publish",
            post(publish_report),
        )
        .route(
            "/v1/collections/{collection}/records/{id}/published-reports",
            get(published_reports),
        )
        .route(
            "/v1/collections/{collection}/searches/{id}",
            get(search_receipt),
        )
        .route("/v1/collections/{collection}/export", get(transfer::export))
        .route(
            "/v1/collections/{collection}/import",
            post(transfer::import),
        )
        .route("/v1/search", post(search))
        .fallback(|| async { ApiError::not_found() })
        .method_not_allowed_fallback(|| async {
            ApiError(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "method is not supported",
            )
        })
        .layer(DefaultBodyLimit::max(limit))
        .layer(middleware::from_fn_with_state(
            service.clone(),
            authenticate,
        ))
        .with_state(service)
}
fn identifier(id: &str) -> ApiResult<()> {
    if is_record_id(id) {
        Ok(())
    } else {
        Err(ApiError::invalid())
    }
}
fn positive(revision: Option<i64>) -> ApiResult<()> {
    if revision.is_some_and(|r| r <= 0) {
        Err(ApiError::invalid())
    } else {
        Ok(())
    }
}
fn collection_json(c: Collection) -> Value {
    json!({"id": c.id, "visibility": c.visibility.as_str(), "created_at": c.created_at})
}
async fn healthz(query: Result<Query<EmptyQuery>, QueryRejection>) -> ApiResult<Json<Value>> {
    query?;
    Ok(Json(json!({"status": "ok"})))
}
async fn collections(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    let Query(query) = query?;
    let rows = blocking(&service, move |s| {
        s.collections(&context, &query.after, query.limit)
    })
    .await?;
    Ok(Json(
        json!({"collections": rows.into_iter().map(collection_json).collect::<Vec<_>>()}),
    ))
}
async fn collection(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    query?;
    let Path(id) = path?;
    identifier(&id)?;
    let row = blocking(&service, move |s| s.collection(&context, &id)).await?;
    Ok(Json(collection_json(row)))
}
async fn diagnostics(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    query?;
    let Path(id) = path?;
    identifier(&id)?;
    let row = blocking(&service, move |s| s.health(&context, &id)).await?;
    Ok(Json(
        json!({"status": row.status, "document_count": row.document_count}),
    ))
}
async fn put_record(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    Extension(key): Extension<IdempotencyKey>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
    body: Result<Json<PutRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, [(&'static str, String); 1], Json<Value>)> {
    query?;
    let Path(collection) = path?;
    identifier(&collection)?;
    let Json(body) = body?;
    let response_collection = collection.clone();
    let result = blocking(&service, move |s| {
        s.put(
            &context,
            &collection,
            &body.text,
            body.metadata.as_ref(),
            body.id.as_deref(),
            WriteOptions {
                idempotency_key: key.0.as_deref(),
                publish: body.publish,
                expected_revision: body.expected_revision,
            },
        )
    })
    .await?;
    let (id, revision) = result.value;
    Ok((
        StatusCode::CREATED,
        [("mutation-id", result.mutation_id)],
        Json(json!({"collection_id": response_collection, "id": id, "revision": revision})),
    ))
}
async fn get_record(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<RevisionQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    let Path((collection, id)) = path?;
    identifier(&collection)?;
    identifier(&id)?;
    let Query(query) = query?;
    positive(query.revision)?;
    let row = blocking(&service, move |s| {
        s.get(&context, &collection, &id, query.revision)
    })
    .await?;
    Ok(Json(
        json!({"collection_id": row.collection_id, "id": row.record_id, "revision": row.revision, "text": row.text, "metadata": row.metadata, "created_at": row.created_at}),
    ))
}
async fn delete_record(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    Extension(key): Extension<IdempotencyKey>,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<DeleteQuery>, QueryRejection>,
) -> ApiResult<(StatusCode, [(&'static str, String); 1])> {
    let Path((collection, id)) = path?;
    identifier(&collection)?;
    identifier(&id)?;
    let Query(query) = query?;
    positive(query.expected_revision)?;
    let result = blocking(&service, move |s| {
        s.delete(
            &context,
            &collection,
            &id,
            DeleteOptions {
                expected_revision: query.expected_revision,
                idempotency_key: key.0.as_deref(),
            },
        )
    })
    .await?;
    Ok((
        StatusCode::NO_CONTENT,
        [("mutation-id", result.mutation_id)],
    ))
}
async fn search(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
    body: Result<Json<SearchRequest>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    query?;
    let Json(body) = body?;
    if body.collections.len() != 1 {
        return Err(ApiError::invalid());
    }
    let collection = body.collections[0].clone();
    identifier(&collection)?;
    let outcome = blocking(&service, move |s| {
        s.search(
            &context,
            &collection,
            &body.text,
            body.filters.as_ref(),
            body.limit,
        )
    })
    .await?;
    Ok(Json(
        json!({"collections": body.collections, "search_id": outcome.search_id, "hits": outcome.hits}),
    ))
}
async fn report(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    Extension(key): Extension<IdempotencyKey>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
    body: Result<Json<ReportRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, [(&'static str, String); 1], Json<Value>)> {
    query?;
    let Path(collection) = path?;
    identifier(&collection)?;
    let Json(body) = body?;
    identifier(&body.record_id)?;
    positive(Some(body.revision))?;
    if let Some(id) = &body.search_id {
        identifier(id)?;
    }
    let response_collection = collection.clone();
    let result = blocking(&service, move |s| {
        s.report(
            &context,
            &collection,
            &body.record_id,
            &body.text,
            ReportOptions {
                revision: Some(body.revision),
                search_id: body.search_id.as_deref(),
                idempotency_key: key.0.as_deref(),
            },
        )
    })
    .await?;
    let id = result.value;
    Ok((
        StatusCode::CREATED,
        [("mutation-id", result.mutation_id)],
        Json(json!({"collection_id": response_collection, "id": id})),
    ))
}
async fn list_reports(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    query?;
    let Path((collection, id)) = path?;
    identifier(&collection)?;
    identifier(&id)?;
    let rows = blocking(&service, move |s| s.reports(&context, &collection, &id)).await?;
    let rows: Vec<_> = rows.into_iter().map(|r| json!({"collection_id": r.collection_id, "id": r.id, "record_id": r.record_id, "revision": r.revision, "search_id": r.search_id, "text": r.text, "created_at": r.created_at})).collect();
    Ok(Json(json!({"reports": rows})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationRequest {
    text: String,
}

async fn publish_report(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    Extension(key): Extension<IdempotencyKey>,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
    body: Result<Json<PublicationRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, [(&'static str, String); 1], Json<Value>)> {
    query?;
    let Path((collection, id)) = path?;
    identifier(&collection)?;
    identifier(&id)?;
    let Json(body) = body?;
    let response_collection = collection.clone();
    let result = blocking(&service, move |s| {
        s.publish_report(&context, &collection, &id, &body.text, key.0.as_deref())
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        [("mutation-id", result.mutation_id)],
        Json(json!({"collection_id": response_collection, "id": result.value})),
    ))
}

async fn published_reports(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    query?;
    let Path((collection, id)) = path?;
    identifier(&collection)?;
    identifier(&id)?;
    let rows = blocking(&service, move |s| {
        s.published_reports(&context, &collection, &id)
    })
    .await?;
    Ok(Json(json!({"reports": rows})))
}

async fn search_receipt(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    query?;
    let Path((collection, id)) = path?;
    identifier(&collection)?;
    identifier(&id)?;
    let receipt = blocking(&service, move |s| {
        s.search_receipt(&context, &collection, &id)
    })
    .await?;
    Ok(Json(json!(receipt)))
}

async fn delete_collection(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<StatusCode> {
    query?;
    let Path(collection) = path?;
    identifier(&collection)?;
    blocking(&service, move |s| {
        s.delete_collection(&context, &collection)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
