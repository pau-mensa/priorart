use super::*;

pub(super) fn purge_record_content(
    transaction: &Transaction<'_>,
    collection: &str,
    record: &str,
) -> Result<()> {
    let targets = "collection_id = ?1 AND operation = 'put' AND target_record_id = ?2";
    transaction.execute(
        &format!("DELETE FROM mutations WHERE idempotency_digest IS NULL AND {targets}"),
        (collection, record),
    )?;
    // Keep key tombstones so a delayed create retry cannot recreate purged content.
    transaction.execute(
        &format!("UPDATE mutations SET payload_digest = '', result = 'null', target_record_id = NULL WHERE {targets}"),
        (collection, record),
    )?;
    transaction.execute(
        "DELETE FROM revisions WHERE collection_id = ?1 AND record_id = ?2",
        (collection, record),
    )?;
    Ok(())
}

impl Store {
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
        for table in ["credential_grants", "mutations", "revisions", "records"] {
            transaction.execute(
                &format!("DELETE FROM {table} WHERE collection_id = ?1"),
                [collection],
            )?;
        }
        transaction.execute(
            "INSERT INTO collection_tombstones VALUES (?1, ?2)",
            (collection, now()),
        )?;
        transaction.execute("DELETE FROM collections WHERE id = ?1", [collection])?;
        transaction.commit()?;
        Ok(())
    }
}
