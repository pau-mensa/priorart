use super::*;
use crate::{
    service::ImportOptions,
    store::{ExportCursor, TransferRecord, Visibility},
};
use axum::{
    body::{Body, BodyDataStream, Bytes},
    http::{header::CONTENT_TYPE, HeaderMap},
};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};

const MAX_ROWS: usize = 10_000;
const MAX_BYTES: usize = 128 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Frame {
    Revision {
        record: TransferRecord,
    },
    End {
        count: usize,
        generation: i64,
        next_cursor: Option<ExportCursor>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExportQuery {
    #[serde(default)]
    after_record: String,
    #[serde(default)]
    after_revision: i64,
    generation: Option<i64>,
    #[serde(default = "page_size")]
    limit: usize,
}

fn page_size() -> usize {
    50
}

fn line(value: impl Serialize) -> Bytes {
    let mut bytes = serde_json::to_vec(&value).expect("serializable transfer frame");
    bytes.push(b'\n');
    bytes.into()
}
fn failure(error: ApiError) -> Bytes {
    line(json!({"type": "error", "error": {"code": error.1, "message": error.2}}))
}
fn response(body: Body) -> Response {
    (
        [
            (CONTENT_TYPE, "application/x-ndjson"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

struct ExportState {
    service: Arc<Service>,
    context: RequestContext,
    collection: String,
    cursor: ExportCursor,
    count: usize,
    bytes: usize,
    finished: bool,
}

pub(super) async fn export(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<ExportQuery>, QueryRejection>,
) -> ApiResult<Response> {
    let Path(collection) = path?;
    identifier(&collection)?;
    let Query(query) = query?;
    if !(1..=MAX_ROWS).contains(&query.limit)
        || query.after_revision < 0
        || query.after_record.is_empty() != (query.after_revision == 0)
        || (!query.after_record.is_empty() && query.generation.is_none())
    {
        return Err(ApiError::invalid());
    }
    if !query.after_record.is_empty() {
        identifier(&query.after_record)?;
    }
    let c = collection.clone();
    let ctx = context.clone();
    let generation = blocking(&service, move |s| s.export_generation(&ctx, &c)).await?;
    if query.generation.is_some_and(|g| g != generation) {
        return Err(ServiceError::Store(StoreError::ExportChanged).into());
    }
    let state = ExportState {
        service,
        context,
        collection,
        cursor: ExportCursor {
            record_id: query.after_record,
            revision: query.after_revision,
        },
        count: 0,
        bytes: 0,
        finished: false,
    };
    let output = stream::unfold(state, move |mut state| async move {
        if state.finished {
            return None;
        }
        let c = state.collection.clone();
        let ctx = state.context.clone();
        let after = state.cursor.clone();
        let result = blocking(&state.service, move |s| {
            s.export_next(&ctx, &c, generation, &after)
        })
        .await;
        let bytes = match result {
            Ok(Some(record)) if state.count < query.limit => {
                let cursor = ExportCursor {
                    record_id: record.record_id.clone(),
                    revision: record.revision,
                };
                let bytes = line(Frame::Revision { record });
                if state.bytes.saturating_add(bytes.len()) > MAX_BYTES - 1024 {
                    state.finished = true;
                    if state.count == 0 {
                        failure(ApiError::invalid())
                    } else {
                        line(Frame::End {
                            count: state.count,
                            generation,
                            next_cursor: Some(state.cursor.clone()),
                        })
                    }
                } else {
                    state.cursor = cursor;
                    state.count += 1;
                    bytes
                }
            }
            Ok(next) => {
                state.finished = true;
                line(Frame::End {
                    count: state.count,
                    generation,
                    next_cursor: next.map(|_| state.cursor.clone()),
                })
            }
            Err(error) => {
                state.finished = true;
                failure(error)
            }
        };
        state.bytes += bytes.len();
        Some((Ok::<_, std::convert::Infallible>(bytes), state))
    });
    Ok(response(Body::from_stream(output)))
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ImportQuery {
    visibility: Visibility,
    #[serde(default)]
    publish: bool,
}

struct Lines {
    body: BodyDataStream,
    pending: Bytes,
    partial: Vec<u8>,
    bytes: usize,
    max_line: usize,
}
impl Lines {
    async fn next(&mut self) -> ApiResult<Option<Vec<u8>>> {
        loop {
            if !self.pending.is_empty() {
                let newline = self.pending.iter().position(|b| *b == b'\n');
                let take = newline.map_or(self.pending.len(), |p| p + 1);
                if self.partial.len().saturating_add(take) > self.max_line {
                    return Err(ApiError::invalid());
                }
                self.partial.extend_from_slice(&self.pending.split_to(take));
                if newline.is_some() {
                    return Ok(Some(std::mem::take(&mut self.partial)));
                }
            }
            match self.body.next().await {
                Some(Ok(bytes)) => {
                    self.bytes = self.bytes.saturating_add(bytes.len());
                    if self.bytes > MAX_BYTES {
                        return Err(ApiError::invalid());
                    }
                    self.pending = bytes;
                }
                Some(Err(_)) => return Err(ApiError::invalid()),
                None if self.partial.is_empty() => return Ok(None),
                None => return Ok(Some(std::mem::take(&mut self.partial))),
            }
        }
    }
}

pub(super) async fn import(
    State(service): State<Arc<Service>>,
    Extension(context): Extension<RequestContext>,
    Extension(key): Extension<IdempotencyKey>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<ImportQuery>, QueryRejection>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let Path(collection) = path?;
    identifier(&collection)?;
    let Query(query) = query?;
    let key = key.0.ok_or_else(ApiError::invalid)?;
    if headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) != Some("application/x-ndjson") {
        return Err(ApiError::invalid());
    }
    let c = collection.clone();
    let ctx = context.clone();
    let k = key.clone();
    blocking(&service, move |s| {
        s.authorize_import(
            &ctx,
            &c,
            ImportOptions {
                batch_key: &k,
                visibility: query.visibility,
                publish: query.publish,
            },
        )
    })
    .await?;
    let lines = Lines {
        body: body.into_data_stream(),
        pending: Bytes::new(),
        partial: Vec::new(),
        bytes: 0,
        max_line: service.settings().max_body_bytes(),
    };
    let state = (service, context, collection, key, lines, 0usize, false);
    let output = stream::unfold(
        state,
        move |(service, context, collection, key, mut lines, count, done)| async move {
            if done {
                return None;
            }
            let result: ApiResult<(Bytes, bool)> = async {
            let bytes = lines.next().await?.ok_or_else(ApiError::invalid)?;
            let frame: Frame = serde_json::from_slice(&bytes).map_err(|_| ApiError::validation())?;
            match frame {
                Frame::Revision {record} => {
                    if count >= MAX_ROWS { return Err(ApiError::invalid()); }
                    let source = json!({"collection_id": record.collection_id, "record_id": record.record_id, "revision": record.revision});
                    let c = collection.clone();
                    let ctx = context.clone();
                    let k = key.clone();
                    let imported = blocking(&service, move |s| s.import_revision(&ctx, &c, &record, ImportOptions {batch_key: &k, visibility: query.visibility, publish: query.publish})).await?;
                    Ok((line(json!({"type":"imported", "source":source, "collection_id":collection,
                        "record_id":imported.value.0, "revision":imported.value.1, "mutation_id":imported.mutation_id})), false))
                },
                Frame::End {count: expected, ..} => {
                    if expected != count || lines.next().await?.is_some() { return Err(ApiError::invalid()); }
                    let c = collection.clone();
                    let ctx = context.clone();
                    let k = key.clone();
                    blocking(&service, move |s| s.authorize_import(&ctx, &c, ImportOptions {batch_key:&k, visibility:query.visibility, publish:query.publish})).await?;
                    Ok((line(json!({"type":"end", "count":count})), true))
                },
            }
        }.await;
            let (bytes, finished) = match result {
                Ok(result) => result,
                Err(error) => (failure(error), true),
            };
            let count = if finished { count } else { count + 1 };
            Some((
                Ok::<_, std::convert::Infallible>(bytes),
                (service, context, collection, key, lines, count, finished),
            ))
        },
    );
    Ok(response(Body::from_stream(output)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::{Grant, Operation},
        config::Settings,
        service::{WriteOptions, DATABASE_FILE},
        store::{Store, LOCAL_COLLECTION_ID as LOCAL, LOCAL_PRINCIPAL_ID},
    };

    #[tokio::test]
    async fn export_rechecks_changes_and_revocation_between_stream_polls() {
        for revoke in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path().join(DATABASE_FILE)).unwrap();
            let key = store
                .issue_local_credential(
                    LOCAL_PRINCIPAL_ID,
                    &[
                        Grant::new(LOCAL, Operation::Read),
                        Grant::new(LOCAL, Operation::Export),
                    ],
                    None,
                )
                .unwrap()
                .into_secret();
            let service = Arc::new(
                Service::open(Settings {
                    data_dir: dir.path().into(),
                    ..Default::default()
                })
                .unwrap(),
            );
            service
                .put(
                    &RequestContext::local(),
                    LOCAL,
                    "first",
                    None,
                    Some("a"),
                    Default::default(),
                )
                .unwrap();
            service
                .put(
                    &RequestContext::local(),
                    LOCAL,
                    "second",
                    None,
                    Some("b"),
                    Default::default(),
                )
                .unwrap();
            let context = service.authenticate(&key).unwrap();
            let id = context.credential().unwrap().id.clone();
            let response = export(
                State(service.clone()),
                Extension(context),
                Ok(Path(LOCAL.into())),
                Ok(Query(ExportQuery {
                    after_record: String::new(),
                    after_revision: 0,
                    generation: None,
                    limit: 50,
                })),
            )
            .await
            .ok()
            .unwrap();
            let mut body = response.into_body().into_data_stream();
            let first: Value =
                serde_json::from_slice(&body.next().await.unwrap().unwrap()).unwrap();
            assert_eq!(first["type"], "revision");
            if revoke {
                store.revoke_local_credential(&id).unwrap();
            } else {
                service
                    .put(
                        &RequestContext::local(),
                        LOCAL,
                        "edited",
                        None,
                        Some("a"),
                        WriteOptions {
                            expected_revision: Some(1),
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
            let next: Value = serde_json::from_slice(&body.next().await.unwrap().unwrap()).unwrap();
            assert_eq!(next["type"], "error");
            assert_eq!(
                next["error"]["code"],
                if revoke {
                    "unauthenticated"
                } else {
                    "export_changed"
                }
            );
            assert!(body.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn line_reader_handles_split_utf8_and_enforces_bounds() {
        let input = "{\"text\":\"é中\"}\n{\"end\":true}";
        let chunks: Vec<_> = input
            .as_bytes()
            .chunks(2)
            .map(|c| Ok::<_, std::convert::Infallible>(Bytes::copy_from_slice(c)))
            .collect();
        let mut reader = Lines {
            body: Body::from_stream(stream::iter(chunks)).into_data_stream(),
            pending: Bytes::new(),
            partial: Vec::new(),
            bytes: 0,
            max_line: 64,
        };
        assert_eq!(
            serde_json::from_slice::<Value>(&reader.next().await.ok().flatten().unwrap()).unwrap()
                ["text"],
            "é中"
        );
        assert!(reader.next().await.ok().flatten().is_some());
        assert!(reader.next().await.ok().flatten().is_none());
        for (bytes, max_line) in [(MAX_BYTES, 64), (0, 2)] {
            let mut reader = Lines {
                body: Body::from("123\n").into_data_stream(),
                pending: Bytes::new(),
                partial: Vec::new(),
                bytes,
                max_line,
            };
            assert!(reader.next().await.is_err());
        }
    }
}
