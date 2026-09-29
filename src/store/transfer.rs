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

    pub(crate) fn import_target(
        &self,
        collection: &str,
        principal: &str,
        source_digest: &str,
    ) -> Result<Option<String>> {
        Ok(self.connection.query_row(
            "SELECT record_id FROM import_targets WHERE collection_id = ?1 AND importer_principal_id = ?2 AND source_digest = ?3",
            (collection, principal, source_digest), |r| r.get(0),
        ).optional()?)
    }

    pub(crate) fn import_position(
        &self,
        collection: &str,
        record: &str,
    ) -> Result<Option<(i64, i64)>> {
        Ok(self
            .connection
            .query_row(
                "SELECT source_revision, revision FROM import_provenance
             WHERE collection_id = ?1 AND record_id = ?2 ORDER BY revision DESC LIMIT 1",
                (collection, record),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub(crate) fn commit_import(
        &self,
        intent: &mutations::Intent<'_>,
        record: &str,
        expected: i64,
        source: &TransferRecord,
        truncated: bool,
        source_digest: &str,
    ) -> Result<mutations::Mutation<(String, i64)>> {
        self.commit_mutation(intent, |transaction| {
            let created = put_in(
                transaction,
                intent.collection,
                Content {
                    text: &source.text,
                    truncated,
                    metadata: source.metadata.as_ref(),
                },
                Some(record),
                intent.principal,
                Some(expected),
            )?;
            if expected == 0 {
                transaction.execute(
                    "INSERT INTO import_targets VALUES (?1, ?2, ?3, ?4)",
                    (intent.collection, intent.principal, source_digest, record),
                )?;
            }
            transaction.execute(
                "INSERT INTO import_provenance VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    intent.collection,
                    record,
                    created.revision,
                    intent.principal,
                    source.collection_id,
                    source.record_id,
                    source.revision,
                    source.visibility.as_str(),
                    source.author_principal_id,
                    source.created_at
                ],
            )?;
            Ok(((record.to_owned(), created.revision), record.to_owned()))
        })
    }
}
