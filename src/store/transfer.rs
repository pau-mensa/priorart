use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferRecord {
    pub version: u32,
    pub collection_id: String,
    pub visibility: Visibility,
    pub record_id: String,
    pub revision: i64,
    pub author_principal_id: Option<String>,
    pub created_at: String,
    pub text: String,
    pub metadata: Option<Metadata>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportCursor {
    pub record_id: String,
    pub revision: i64,
}

impl Store {
    pub(crate) fn content_version(&self, collection: &str) -> Result<i64> {
        self.connection
            .query_row(
                "SELECT content_version FROM collections WHERE id = ?1",
                [collection],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::CollectionNotFound(collection.into()))
    }

    pub(crate) fn export_next(
        &self,
        collection: &str,
        after: &ExportCursor,
    ) -> Result<Option<Revision>> {
        let sql = format!(
            "SELECT {REVISION_COLUMNS} WHERE r.collection_id = ?1 AND rec.deleted_at IS NULL
            AND (r.record_id, r.revision) > (?2, ?3) ORDER BY r.record_id, r.revision LIMIT 1"
        );
        Ok(self
            .connection
            .query_row(
                &sql,
                (collection, &after.record_id, after.revision),
                revision_row,
            )
            .optional()?)
    }

    /// Writes `record` as a new revision, conditional on `expected` like a put.
    pub(crate) fn commit_import(
        &self,
        intent: &mutations::Intent<'_>,
        record: &str,
        expected: Option<i64>,
        content: Content<'_>,
    ) -> Result<mutations::Mutation<(String, i64)>> {
        self.commit_mutation(intent, |transaction| {
            let created = put_in(
                transaction,
                intent.collection,
                content,
                Some(record),
                intent.principal,
                expected,
            )?;
            Ok(((record.to_owned(), created.revision), record.to_owned()))
        })
    }
}
