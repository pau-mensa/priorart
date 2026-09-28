use super::*;
use crate::store::{ExportCursor, TransferRecord};

#[derive(Clone, Copy)]
pub struct ImportOptions<'a> {
    pub batch_key: &'a str,
    pub visibility: Visibility,
    pub publish: bool,
}

impl Service {
    pub fn export_generation(&self, context: &RequestContext, collection: &str) -> Result<i64> {
        let store = self.connect()?;
        policy::collection(&store, context, collection, Operation::Export)?;
        policy::collection(&store, context, collection, Operation::Read)?;
        let version = store.content_version(collection)?;
        policy::validate(&store, context)?;
        Ok(version)
    }

    pub fn export_next(
        &self,
        context: &RequestContext,
        collection: &str,
        generation: i64,
        after: &ExportCursor,
    ) -> Result<Option<TransferRecord>> {
        if after.revision < 0
            || (after.record_id.is_empty() != (after.revision == 0))
            || (!after.record_id.is_empty() && !is_record_id(&after.record_id))
        {
            return invalid("invalid export cursor");
        }
        let handle = self.state(context, collection, Operation::Export)?;
        let state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let store = &state.store;
        policy::collection(store, context, collection, Operation::Export)?;
        let scope = policy::collection(store, context, collection, Operation::Read)?;
        if store.content_version(collection)? != generation {
            return Err(StoreError::ExportChanged.into());
        }
        let result = store.export_next(collection, after)?;
        if store.content_version(collection)? != generation {
            return Err(StoreError::ExportChanged.into());
        }
        policy::validate(store, context)?;
        result
            .map(|r| {
                Ok(TransferRecord {
                    version: 1,
                    collection_id: r.collection_id,
                    visibility: scope.visibility,
                    record_id: r.record_id,
                    revision: r.revision,
                    author_principal_id: r.author_principal_id,
                    created_at: r.created_at,
                    text: r.text.ok_or(StoreError::Journal)?,
                    metadata: r.metadata,
                })
            })
            .transpose()
    }

    pub fn authorize_import(
        &self,
        context: &RequestContext,
        collection: &str,
        options: ImportOptions<'_>,
    ) -> Result<()> {
        let store = self.connect()?;
        let scope = policy::collection(&store, context, collection, Operation::Contribute)?;
        if scope.visibility != options.visibility
            || (scope.visibility == Visibility::Public && !options.publish)
        {
            return invalid(
                "import requires matching destination visibility and explicit public publication",
            );
        }
        intent(
            context,
            collection,
            "put",
            Some(options.batch_key),
            json!(null),
            "contribute",
        )?;
        Ok(())
    }

    pub fn import_revision(
        &self,
        context: &RequestContext,
        collection: &str,
        source: &TransferRecord,
        options: ImportOptions<'_>,
    ) -> Result<Mutation<(String, i64)>> {
        self.authorize_import(context, collection, options)?;
        if source.version != 1
            || !is_record_id(&source.collection_id)
            || !is_record_id(&source.record_id)
            || source.revision <= 0
            || source.created_at.is_empty()
            || source.created_at.len() > 64
            || source
                .author_principal_id
                .as_ref()
                .is_some_and(|id| !is_record_id(id))
            || source.text.trim().is_empty()
            || source.text.len() > self.settings.max_text_bytes
        {
            return invalid("invalid import revision");
        }
        validate_metadata(source.metadata.as_ref(), 65_536)?;
        let principal = context.principal_id().ok_or(PolicyError::Unavailable)?;
        let source_digest = transfer_digest(json!([
            options.batch_key,
            source.collection_id,
            source.record_id
        ]));
        let key = format!(
            "import:{}",
            transfer_digest(json!([
                options.batch_key,
                source.collection_id,
                source.record_id,
                source.revision
            ]))
        );
        let handle = self.state(context, collection, Operation::Contribute)?;
        let mut state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let State { store, indexes } = &mut *state;
        policy::collection(store, context, collection, Operation::Contribute)?;
        let record = store
            .import_target(collection, principal, &source_digest)?
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let position = store.import_position(collection, &record)?;
        let authority = if position.is_some() {
            "update"
        } else {
            "contribute"
        };
        let intent = intent(
            context,
            collection,
            "put",
            Some(&key),
            json!([
                "import-v1",
                source,
                options.visibility.as_str(),
                options.publish
            ]),
            authority,
        )?;
        if let Some((result, original)) = store.replay::<(String, i64)>(&intent)? {
            if original == "update" {
                policy::mutation(
                    store,
                    context,
                    collection,
                    &result.value.0,
                    Operation::Update,
                )?;
            }
            store.get(collection, &result.value.0, Some(result.value.1))?;
            indexes.get(store, collection)?;
            policy::validate(store, context)?;
            return Ok(result);
        }
        if position
            .as_ref()
            .is_some_and(|(source_revision, _)| source.revision <= *source_revision)
        {
            return Err(StoreError::RevisionConflict.into());
        }
        let expected = position.as_ref().map_or(0, |(_, revision)| *revision);
        if expected > 0 {
            policy::mutation(store, context, collection, &record, Operation::Update)?;
        }
        store.check_put_revision(collection, Some(&record), Some(expected))?;
        let index = indexes.get(store, collection)?;
        let encoded = index.encode(&source.text);
        policy::collection(store, context, collection, Operation::Contribute)?;
        if expected > 0 {
            policy::mutation(store, context, collection, &record, Operation::Update)?;
        }
        let encoded = encoded?;
        let committed = store.commit_import(&intent, &record, expected, source, &source_digest)?;
        crate::fault::check("after_record_commit").map_err(IndexError::from)?;
        let result = index.upsert(store, &record, committed.value.1, &source.text, encoded);
        policy::validate(store, context)?;
        result?;
        store.complete_mutations(collection)?;
        Ok(committed)
    }
}

fn transfer_digest(value: Value) -> String {
    Sha256::digest(serde_json::to_vec(&value).expect("serializable transfer identity"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
