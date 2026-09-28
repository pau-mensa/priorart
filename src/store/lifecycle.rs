use super::*;

pub(super) fn purge_record_content(
    transaction: &Transaction<'_>,
    collection: &str,
    record: &str,
) -> Result<()> {
    let targets = "collection_id = ?1 AND operation != 'delete' AND target_record_id = ?2";
    transaction.execute(
        &format!("DELETE FROM mutations WHERE idempotency_digest IS NULL AND {targets}"),
        (collection, record),
    )?;
    // Keep key tombstones so a delayed create retry cannot recreate purged content.
    transaction.execute(
        &format!("UPDATE mutations SET payload_digest = '', result = 'null', state = 'applied', target_record_id = NULL WHERE {targets}"),
        (collection, record),
    )?;
    for table in [
        "reports",
        "published_reports",
        "search_hits",
        "index_documents",
        "revisions",
    ] {
        transaction.execute(
            &format!("DELETE FROM {table} WHERE collection_id = ?1 AND record_id = ?2"),
            (collection, record),
        )?;
    }
    transaction.execute(
        "INSERT INTO index_state VALUES (?1, 'purge_version', ?2)
         ON CONFLICT(collection_id, key) DO UPDATE SET value = excluded.value",
        (collection, new_id()),
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO purge_jobs (collection_id) VALUES (?1)",
        [collection],
    )?;
    Ok(())
}

impl Store {
    pub(crate) fn index_purge_version(&self, collection: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT value FROM index_state WHERE collection_id = ?1 AND key = 'purge_version'",
                [collection],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    pub(crate) fn pending_purges(&self, after: &str) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare("SELECT collection_id FROM purge_jobs WHERE collection_id > ?1 UNION SELECT id FROM collection_tombstones WHERE id > ?1 ORDER BY collection_id LIMIT 100")?;
        let rows = statement
            .query_map([after], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub(crate) fn needs_purge(&self, collection: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM purge_jobs WHERE collection_id = ?1)",
            [collection],
            |r| r.get(0),
        )?)
    }

    pub(crate) fn complete_purge(&self, collection: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM purge_jobs WHERE collection_id = ?1",
            [collection],
        )?;
        Ok(())
    }

    pub(crate) fn purge_collection(&self, collection: &str) -> Result<()> {
        let transaction = self.write()?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM collections WHERE id = ?1)",
            [collection],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(StoreError::CollectionNotFound(collection.into()));
        }
        transaction.execute(
            "UPDATE credentials SET grant_version = grant_version + 1 WHERE id IN
             (SELECT credential_id FROM credential_grants WHERE collection_id = ?1)",
            [collection],
        )?;
        for table in [
            "retention_jobs",
            "credential_grants",
            "mutations",
            "reports",
            "published_reports",
            "search_hits",
            "searches",
            "index_documents",
            "index_state",
            "revisions",
            "records",
        ] {
            transaction.execute(
                &format!("DELETE FROM {table} WHERE collection_id = ?1"),
                [collection],
            )?;
        }
        transaction.execute(
            "INSERT INTO collection_tombstones VALUES (?1, ?2)",
            (collection, now()),
        )?;
        transaction.execute("INSERT OR IGNORE INTO purge_jobs VALUES (?1)", [collection])?;
        transaction.execute("DELETE FROM collections WHERE id = ?1", [collection])?;
        crate::fault::check("before_collection_purge_commit")?;
        transaction.commit()?;
        Ok(())
    }
}
