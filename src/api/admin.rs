//! Operator endpoints under `/v1/admin`, enabled by `PRIORART_ADMIN_TOKEN`.
//! Operator authority comes only from that token, never from a credential.
use super::*;
use crate::{
    auth::{Grant, IssuedCredential},
    config::AdminToken,
    store::{Store, Visibility},
};
use axum::{
    http::{header::CACHE_CONTROL, HeaderMap},
    routing::{delete, put},
};

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Unauthenticated
            | AuthError::Forbidden
            | AuthError::Storage(StoreError::PrincipalNotFound(_)) => Self::not_found(),
            AuthError::InvalidInput(_) => Self::invalid(),
            _ => Self::unavailable(),
        }
    }
}

pub(super) fn routes() -> Router<Arc<Service>> {
    Router::new()
        .route("/v1/admin/principals", post(create_principal))
        .route("/v1/admin/collections", post(create_collection))
        .route(
            "/v1/admin/principals/{principal}/credentials",
            get(list_credentials).post(issue),
        )
        .route("/v1/admin/credentials/{credential}", delete(revoke))
        .route("/v1/admin/credentials/{credential}/rotate", post(rotate))
        .route(
            "/v1/admin/credentials/{credential}/grants",
            put(replace_grants),
        )
}

/// Unconfigured, the endpoints do not exist; configured, only the token opens them.
pub(super) fn authorize(token: Option<&AdminToken>, headers: &HeaderMap) -> ApiResult<()> {
    let token = token.ok_or_else(ApiError::not_found)?;
    let supplied: Vec<_> = headers.get_all(AUTHORIZATION).iter().collect();
    let [header] = supplied.as_slice() else {
        return Err(ApiError::unauthenticated());
    };
    let bearer = header
        .to_str()
        .ok()
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"));
    match bearer {
        Some((_, candidate)) if token.matches(candidate) => Ok(()),
        _ => Err(ApiError::unauthenticated()),
    }
}

async fn operate<T: Send + 'static>(
    service: &Arc<Service>,
    call: impl FnOnce(&Store) -> Result<T, AuthError> + Send + 'static,
) -> ApiResult<T> {
    let service = service.clone();
    tokio::task::spawn_blocking(move || call(&service.connect()?))
        .await
        .map_err(|_| ApiError::unavailable())?
        .map_err(Into::into)
}

/// Secrets are returned once and must not be cached.
fn issued(credential: IssuedCredential) -> Response {
    let info = credential.info.clone();
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, "no-store")],
        Json(json!({"credential": info, "secret": credential.into_secret()})),
    )
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionRequest {
    owner: String,
    #[serde(default = "restricted")]
    visibility: Visibility,
}
fn restricted() -> Visibility {
    Visibility::Restricted
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantsRequest {
    grants: Vec<Grant>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueRequest {
    grants: Vec<Grant>,
    expires_at: Option<i64>,
}

async fn create_principal(
    State(service): State<Arc<Service>>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    query?;
    let id = operate(&service, |store| Ok(store.create_principal()?)).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))))
}

async fn create_collection(
    State(service): State<Arc<Service>>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
    body: Result<Json<CollectionRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    query?;
    let Json(body) = body?;
    identifier(&body.owner)?;
    let collection = operate(&service, move |store| {
        let id = store.create_collection(&body.owner, body.visibility)?;
        Ok(store.get_collection(&id)?)
    })
    .await?;
    let mut row = collection_json(collection.clone());
    row["owner_principal_id"] = json!(collection.owner_principal_id);
    Ok((StatusCode::CREATED, Json(row)))
}

async fn list_credentials(
    State(service): State<Arc<Service>>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    query?;
    let Path(principal) = path?;
    identifier(&principal)?;
    let credentials = operate(&service, move |store| store.credentials(&principal)).await?;
    Ok(Json(json!({"credentials": credentials})))
}

async fn issue(
    State(service): State<Arc<Service>>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
    body: Result<Json<IssueRequest>, JsonRejection>,
) -> ApiResult<Response> {
    query?;
    let Path(principal) = path?;
    identifier(&principal)?;
    let Json(body) = body?;
    let credential = operate(&service, move |store| {
        store.issue_credential(&principal, &body.grants, body.expires_at)
    })
    .await?;
    Ok(issued(credential))
}

async fn rotate(
    State(service): State<Arc<Service>>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<Response> {
    query?;
    let Path(credential) = path?;
    identifier(&credential)?;
    let credential = operate(&service, move |store| store.rotate_credential(&credential)).await?;
    Ok(issued(credential))
}

async fn revoke(
    State(service): State<Arc<Service>>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<StatusCode> {
    query?;
    let Path(credential) = path?;
    identifier(&credential)?;
    operate(&service, move |store| store.revoke_credential(&credential)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Replaces the full grant set; an empty list removes every permission.
async fn replace_grants(
    State(service): State<Arc<Service>>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
    body: Result<Json<GrantsRequest>, JsonRejection>,
) -> ApiResult<StatusCode> {
    query?;
    let Path(credential) = path?;
    identifier(&credential)?;
    let Json(body) = body?;
    operate(&service, move |store| {
        store.replace_credential_grants(&credential, &body.grants)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
