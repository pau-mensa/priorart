//! HTTP transport for the protocol. See `docs/protocol.md`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::service::{Health, Service, ServiceError};
use crate::store::{Metadata, StoreError};

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn validation(message: String) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            message,
        )
    }
}

impl From<ServiceError> for ApiError {
    fn from(error: ServiceError) -> Self {
        let message = error.to_string();
        match error {
            ServiceError::InvalidInput(_) => {
                Self::new(StatusCode::BAD_REQUEST, "invalid_input", message)
            }
            ServiceError::Store(StoreError::RecordNotFound { .. }) => {
                Self::new(StatusCode::NOT_FOUND, "record_not_found", message)
            }
            ServiceError::Store(StoreError::SearchNotFound { .. }) => {
                Self::new(StatusCode::NOT_FOUND, "search_not_found", message)
            }
            ServiceError::Store(StoreError::RecordDeleted { .. }) => {
                Self::new(StatusCode::GONE, "record_deleted", message)
            }
            _ => {
                eprintln!("priorart: internal error: {message}");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "the server failed to complete the request",
                )
            }
        }
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        Self::validation(rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::validation(rejection.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"error": {"code": self.code, "message": self.message}});
        (self.status, Json(body)).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Runs a service call on the blocking pool: calls hold the service lock and
/// may encode or rewrite the vector store.
async fn blocking<T, F>(service: &Arc<Service>, call: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Service) -> Result<T, ServiceError> + Send + 'static,
{
    let service = service.clone();
    tokio::task::spawn_blocking(move || call(&service))
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the server failed to complete the request",
            )
        })?
        .map_err(ApiError::from)
}

#[derive(Deserialize)]
struct PutRequest {
    text: String,
    metadata: Option<Metadata>,
    id: Option<String>,
}

#[derive(Serialize)]
struct PutResponse {
    id: String,
    revision: i64,
}

#[derive(Deserialize)]
struct RevisionQuery {
    revision: Option<i64>,
}

#[derive(Serialize)]
struct RecordResponse {
    id: String,
    revision: i64,
    text: String,
    metadata: Option<Metadata>,
    created_at: String,
}

fn default_limit() -> i64 {
    10
}

#[derive(Deserialize)]
struct SearchRequest {
    text: String,
    filters: Option<Metadata>,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Serialize)]
struct HitResponse {
    id: String,
    revision: i64,
    score: f64,
    score_semantics: String,
    excerpt: String,
    metadata: Option<Metadata>,
}

#[derive(Serialize)]
struct SearchResponse {
    search_id: String,
    hits: Vec<HitResponse>,
    timings: BTreeMap<String, f64>,
    gatherer: String,
}

#[derive(Deserialize)]
struct ReportRequest {
    record_id: String,
    text: String,
    revision: Option<i64>,
    search_id: Option<String>,
}

#[derive(Serialize)]
struct ReportCreated {
    id: String,
}

#[derive(Serialize)]
struct ReportResponse {
    id: String,
    record_id: String,
    revision: Option<i64>,
    search_id: Option<String>,
    text: String,
    created_at: String,
}

#[derive(Serialize)]
struct ReportsResponse {
    reports: Vec<ReportResponse>,
}

pub fn router(service: Arc<Service>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/records", post(put_record))
        .route("/v1/records/{id}", get(get_record).delete(delete_record))
        .route("/v1/records/{id}/reports", get(list_reports))
        .route("/v1/search", post(search))
        .route("/v1/reports", post(report))
        .with_state(service)
}

async fn healthz(State(service): State<Arc<Service>>) -> ApiResult<Json<Health>> {
    blocking(&service, |service| Ok(service.health()))
        .await
        .map(Json)
}

async fn put_record(
    State(service): State<Arc<Service>>,
    body: Result<Json<PutRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<PutResponse>)> {
    let Json(body) = body?;
    let (id, revision) = blocking(&service, move |service| {
        service.put(&body.text, body.metadata.as_ref(), body.id.as_deref())
    })
    .await?;
    Ok((StatusCode::CREATED, Json(PutResponse { id, revision })))
}

async fn get_record(
    State(service): State<Arc<Service>>,
    Path(id): Path<String>,
    query: Result<Query<RevisionQuery>, QueryRejection>,
) -> ApiResult<Json<RecordResponse>> {
    let Query(query) = query?;
    let revision = blocking(&service, move |service| service.get(&id, query.revision)).await?;
    Ok(Json(RecordResponse {
        id: revision.record_id,
        revision: revision.revision,
        text: revision.text.unwrap_or_default(),
        metadata: revision.metadata,
        created_at: revision.created_at,
    }))
}

async fn delete_record(
    State(service): State<Arc<Service>>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    blocking(&service, move |service| service.delete(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn search(
    State(service): State<Arc<Service>>,
    body: Result<Json<SearchRequest>, JsonRejection>,
) -> ApiResult<Json<SearchResponse>> {
    let Json(body) = body?;
    let outcome = blocking(&service, move |service| {
        service.search(&body.text, body.filters.as_ref(), body.limit)
    })
    .await?;
    Ok(Json(SearchResponse {
        search_id: outcome.search_id,
        hits: outcome
            .hits
            .into_iter()
            .map(|hit| HitResponse {
                id: hit.id,
                revision: hit.revision,
                score: hit.score,
                score_semantics: hit.score_semantics,
                excerpt: hit.excerpt,
                metadata: hit.metadata,
            })
            .collect(),
        timings: outcome.timings,
        gatherer: outcome.gatherer,
    }))
}

async fn report(
    State(service): State<Arc<Service>>,
    body: Result<Json<ReportRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<ReportCreated>)> {
    let Json(body) = body?;
    let id = blocking(&service, move |service| {
        service.report(
            &body.record_id,
            &body.text,
            body.revision,
            body.search_id.as_deref(),
        )
    })
    .await?;
    Ok((StatusCode::CREATED, Json(ReportCreated { id })))
}

async fn list_reports(
    State(service): State<Arc<Service>>,
    Path(id): Path<String>,
) -> ApiResult<Json<ReportsResponse>> {
    let reports = blocking(&service, move |service| service.reports(&id)).await?;
    Ok(Json(ReportsResponse {
        reports: reports
            .into_iter()
            .map(|report| ReportResponse {
                id: report.id,
                record_id: report.record_id,
                revision: report.revision,
                search_id: report.search_id,
                text: report.text,
                created_at: report.created_at,
            })
            .collect(),
    }))
}
