use super::*;
use crate::store::{Content, ExportCursor, TransferRecord};

impl Service {
    pub fn export_generation(&self, context: &RequestContext, collection: &str) -> Result<i64> {
        let store = self.connect()?;
        policy::collection(&store, context, collection, Operation::Admin)?;
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
        let handle = self.state(context, collection, Operation::Admin)?;
        let state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let store = &state.store;
        let scope = policy::collection(store, context, collection, Operation::Admin)?;
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
        batch_key: &str,
    ) -> Result<()> {
        let store = self.connect()?;
        policy::collection(&store, context, collection, Operation::Write)?;
        intent(context, collection, "put", Some(batch_key), json!(null))?;
        Ok(())
    }

    /// Imports one exported revision under its source record ID. By default
    /// source revision N must land as revision N, so an existing record with
    /// that ID conflicts; `overwrite` appends it as a new revision instead.
    pub fn import_revision(
        &self,
        context: &RequestContext,
        collection: &str,
        source: &TransferRecord,
        batch_key: &str,
        overwrite: bool,
    ) -> Result<Mutation<(String, i64)>> {
        self.authorize_import(context, collection, batch_key)?;
        if source.version != 1
            || !is_record_id(&source.collection_id)
            || !is_record_id(&source.record_id)
            || source.revision <= 0
            || source.text.trim().is_empty()
        {
            return invalid("invalid import revision");
        }
        validate_metadata(source.metadata.as_ref(), 65_536)?;
        let key = format!(
            "import:{}",
            transfer_digest(json!([
                batch_key,
                source.collection_id,
                source.record_id,
                source.revision
            ]))
        );
        let handle = self.state(context, collection, Operation::Write)?;
        let mut state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let State { store, index } = &mut *state;
        let intent = intent(
            context,
            collection,
            "put",
            Some(&key),
            json!(["import-v1", source, overwrite]),
        )?;
        let record = source.record_id.as_str();
        if let Some(result) = store.replay::<(String, i64)>(&intent)? {
            policy::mutation(store, context, collection, record)?;
            store.get(collection, record, Some(result.value.1))?;
            return Ok(result);
        }
        if store.record_author(collection, record)?.is_some() {
            policy::mutation(store, context, collection, record)?;
        }
        let fitted = self.cutoff(&source.text);
        let committed = store.commit_import(
            &intent,
            record,
            (!overwrite).then_some(source.revision - 1),
            Content {
                text: fitted,
                truncated: fitted.len() < source.text.len(),
                metadata: source.metadata.as_ref(),
            },
        )?;
        self.reindex(collection, "import", index, |index| {
            index.upsert(record, committed.value.1, fitted)
        });
        Ok(committed)
    }
}

fn transfer_digest(value: Value) -> String {
    Sha256::digest(serde_json::to_vec(&value).expect("serializable transfer identity"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
