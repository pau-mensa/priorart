use super::*;
use crate::store::{FeedbackKind, RetentionKind};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RetentionRequest {
    id: String,
    kind: RetentionKind,
    before_unix: i64,
}

pub(super) async fn create(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
    body: Result<Json<RetentionRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    query?;
    let Path(collection) = path?;
    identifier(&collection)?;
    let Json(body) = body?;
    let job = blocking(&service, move |s| {
        s.create_retention_job(&context, &collection, &body.id, body.kind, body.before_unix)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(json!(job))))
}

pub(super) async fn inspect(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    query?;
    let Path((collection, id)) = path?;
    identifier(&collection)?;
    identifier(&id)?;
    let job = blocking(&service, move |s| {
        s.retention_job(&context, &collection, &id)
    })
    .await?;
    Ok(Json(json!(job)))
}

pub(super) async fn run(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    query?;
    let Path((collection, id)) = path?;
    identifier(&collection)?;
    identifier(&id)?;
    let job = blocking(&service, move |s| {
        s.run_retention_batch(&context, &collection, &id)
    })
    .await?;
    Ok(Json(json!(job)))
}

macro_rules! feedback_delete {
    ($name:ident, $kind:ident) => {
        pub(super) async fn $name(
            State(service): State<Arc<Service>>,
            Extension(context): Extension<RequestContext>,
            path: Result<Path<(String, String)>, PathRejection>,
            query: Result<Query<EmptyQuery>, QueryRejection>,
        ) -> ApiResult<StatusCode> {
            query?;
            let Path((collection, id)) = path?;
            identifier(&collection)?;
            identifier(&id)?;
            blocking(&service, move |s| {
                s.delete_feedback(&context, &collection, &id, FeedbackKind::$kind)
            })
            .await?;
            Ok(StatusCode::NO_CONTENT)
        }
    };
}
feedback_delete!(delete_receipt, Receipt);
feedback_delete!(delete_report, Report);
feedback_delete!(delete_publication, Publication);
