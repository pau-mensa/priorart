use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetentionKind {
    Revisions,
    Receipts,
    Reports,
    Mutations,
}

#[derive(Clone, Copy)]
pub enum FeedbackKind {
    Receipt,
    Report,
    Publication,
}

#[derive(Debug, Serialize)]
pub struct RetentionJob {
    pub collection_id: String,
    pub id: String,
    pub kind: RetentionKind,
    pub cutoff: String,
    pub processed: i64,
    pub complete: bool,
}

impl Store {
    pub(crate) fn create_retention_job(
        &self,
        collection: &str,
        id: &str,
        kind: RetentionKind,
        before: i64,
    ) -> Result<RetentionJob> {
        let cutoff = OffsetDateTime::from_unix_timestamp(before)
            .map_err(|_| StoreError::Journal)?
            .format(TIMESTAMP)
            .map_err(|_| StoreError::Journal)?;
        let transaction = self.write()?;
        if let Some(job) = retention_job(&transaction, collection, id)? {
            if job.kind != kind || job.cutoff != cutoff {
                return Err(StoreError::IdempotencyConflict);
            }
            return Ok(job);
        }
        let kind = serde_json::to_value(kind).map_err(|_| StoreError::Journal)?;
        transaction.execute(
            "INSERT INTO retention_jobs(collection_id,id,kind,cutoff) VALUES (?1,?2,?3,?4)",
            (
                collection,
                id,
                kind.as_str().ok_or(StoreError::Journal)?,
                cutoff,
            ),
        )?;
        let job = retention_job(&transaction, collection, id)?.ok_or(StoreError::Journal)?;
        transaction.commit()?;
        Ok(job)
    }

    pub(crate) fn retention_job(&self, collection: &str, id: &str) -> Result<Option<RetentionJob>> {
        retention_job(&self.connection, collection, id)
    }

    pub(crate) fn run_retention_batch(&self, collection: &str, id: &str) -> Result<RetentionJob> {
        let transaction = self.write()?;
        let mut job = retention_job(&transaction, collection, id)?.ok_or(StoreError::Journal)?;
        if job.complete {
            return Ok(job);
        }
        let query = match job.kind {
            RetentionKind::Revisions =>
                "SELECT record_id, revision, created_at FROM revisions WHERE collection_id = ?1 AND created_at < ?2
                 AND (created_at,record_id,revision) >
                     (SELECT cursor_created,cursor_record,cursor_revision FROM retention_jobs WHERE collection_id = ?1 AND id = ?3)
                 ORDER BY created_at, record_id, revision LIMIT 100",
            RetentionKind::Receipts =>
                "SELECT id, 0, '' FROM searches WHERE collection_id = ?1 AND created_at < ?2 ORDER BY created_at, id LIMIT 100",
            RetentionKind::Reports =>
                "SELECT id, 0, '' FROM reports WHERE collection_id = ?1 AND created_at < ?2
                 UNION ALL SELECT id, 1, '' FROM published_reports WHERE collection_id = ?1 AND created_at < ?2 LIMIT 100",
            RetentionKind::Mutations =>
                "SELECT id, 0, '' FROM mutations WHERE collection_id = ?1 AND state = 'applied' AND created_at < ?2 AND payload_digest != '' ORDER BY created_at, id LIMIT 100",
        };
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&collection, &job.cutoff];
        if job.kind == RetentionKind::Revisions {
            params.push(&id);
        }
        let targets = transaction
            .prepare(query)?
            .query_map(params.as_slice(), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut removed = 0;
        for (target, revision, _) in &targets {
            match job.kind {
                RetentionKind::Receipts => remove_receipt(&transaction, collection, target)?,
                RetentionKind::Reports => {
                    remove_report(&transaction, collection, target, *revision == 1)?
                }
                RetentionKind::Mutations => clear_mutation(&transaction, target)?,
                RetentionKind::Revisions => {
                    let latest: i64 = transaction.query_row(
                        "SELECT MAX(revision) FROM revisions WHERE collection_id = ?1 AND record_id = ?2",
                        (collection, target), |r| r.get(0),
                    )?;
                    if *revision == latest {
                        continue;
                    }
                    clear_mutations(&transaction,
                        "collection_id = ?1 AND target_record_id = ?2 AND
                         ((operation = 'put' AND json_extract(result, '$[1]') = ?3) OR
                          (operation = 'report' AND json_extract(result, '$') IN
                           (SELECT id FROM reports WHERE collection_id = ?1 AND record_id = ?2 AND revision = ?3
                            UNION ALL SELECT id FROM published_reports WHERE collection_id = ?1 AND record_id = ?2 AND revision = ?3)))",
                        &[&collection, target, revision],
                    )?;
                    for table in ["reports", "published_reports"] {
                        transaction.execute(&format!("DELETE FROM {table} WHERE collection_id = ?1 AND record_id = ?2 AND revision = ?3"), (collection, target, revision))?;
                    }
                    transaction.execute("DELETE FROM search_hits WHERE collection_id = ?1 AND record_id = ?2 AND revision = ?3", (collection, target, revision))?;
                    transaction.execute("DELETE FROM revisions WHERE collection_id = ?1 AND record_id = ?2 AND revision = ?3", (collection, target, revision))?;
                }
            }
            removed += 1;
        }
        if job.kind == RetentionKind::Revisions {
            if let Some((record, revision, created)) = targets.last() {
                transaction.execute("UPDATE retention_jobs SET cursor_created = ?3, cursor_record = ?4, cursor_revision = ?5 WHERE collection_id = ?1 AND id = ?2", (collection, id, created, record, revision))?;
            }
        }
        if job.kind == RetentionKind::Revisions && removed > 0 {
            transaction.execute(
                "INSERT INTO index_state VALUES (?1, 'purge_version', ?2) ON CONFLICT(collection_id,key) DO UPDATE SET value = excluded.value",
                (collection, new_id()),
            )?;
            transaction.execute("INSERT OR IGNORE INTO purge_jobs VALUES (?1)", [collection])?;
        }
        job.processed += removed;
        job.complete = targets.len() < 100;
        transaction.execute(
            "UPDATE retention_jobs SET processed = ?3, complete = ?4 WHERE collection_id = ?1 AND id = ?2",
            (collection, id, job.processed, job.complete),
        )?;
        crate::fault::check("before_retention_commit")?;
        transaction.commit()?;
        crate::fault::check("after_retention_commit")?;
        Ok(job)
    }

    pub(crate) fn delete_feedback(
        &self,
        collection: &str,
        id: &str,
        principal: &str,
        kind: FeedbackKind,
        moderate: bool,
    ) -> Result<bool> {
        let transaction = self.write()?;
        let query = match kind {
            FeedbackKind::Receipt => "SELECT EXISTS(SELECT 1 FROM searches WHERE collection_id = ?1 AND id = ?2 AND requester_principal_id = ?3)",
            FeedbackKind::Report => "SELECT EXISTS(SELECT 1 FROM reports WHERE collection_id = ?1 AND id = ?2 AND reporter_principal_id = ?3)",
            FeedbackKind::Publication => "SELECT EXISTS(SELECT 1 FROM publication_authors WHERE collection_id = ?1 AND publication_id = ?2 AND (principal_id = ?3 OR ?4))",
        };
        let own: bool = if matches!(kind, FeedbackKind::Publication) {
            transaction.query_row(query, (collection, id, principal, moderate), |r| r.get(0))?
        } else {
            transaction.query_row(query, (collection, id, principal), |r| r.get(0))?
        };
        if !own {
            return Ok(false);
        }
        match kind {
            FeedbackKind::Receipt => remove_receipt(&transaction, collection, id)?,
            FeedbackKind::Report => remove_report(&transaction, collection, id, false)?,
            FeedbackKind::Publication => remove_report(&transaction, collection, id, true)?,
        }
        crate::fault::check("before_feedback_delete_commit")?;
        transaction.commit()?;
        Ok(true)
    }
}

fn retention_job(
    connection: &Connection,
    collection: &str,
    id: &str,
) -> Result<Option<RetentionJob>> {
    let row = connection.query_row(
        "SELECT kind,cutoff,processed,complete FROM retention_jobs WHERE collection_id = ?1 AND id = ?2",
        (collection, id), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, bool>(3)?)),
    ).optional()?;
    row.map(|(kind, cutoff, processed, complete)| {
        Ok(RetentionJob {
            collection_id: collection.into(),
            id: id.into(),
            kind: serde_json::from_value(Value::String(kind)).map_err(|_| StoreError::Journal)?,
            cutoff,
            processed,
            complete,
        })
    })
    .transpose()
}

fn clear_mutation(transaction: &Transaction<'_>, id: &str) -> Result<()> {
    clear_mutations(transaction, "id = ?1", &[&id])
}

fn clear_mutations(
    transaction: &Transaction<'_>,
    predicate: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<()> {
    transaction.execute(
        &format!("DELETE FROM mutations WHERE idempotency_digest IS NULL AND ({predicate})"),
        params,
    )?;
    transaction.execute(&format!("UPDATE mutations SET payload_digest = '', result = 'null', target_record_id = NULL, state = 'applied' WHERE {predicate}"), params)?;
    Ok(())
}

fn remove_report(
    transaction: &Transaction<'_>,
    collection: &str,
    id: &str,
    published: bool,
) -> Result<()> {
    clear_mutations(
        transaction,
        "collection_id = ?1 AND operation = 'report' AND json_extract(result, '$') = ?2",
        &[&collection, &id],
    )?;
    let table = if published {
        "published_reports"
    } else {
        "reports"
    };
    transaction.execute(
        &format!("DELETE FROM {table} WHERE collection_id = ?1 AND id = ?2"),
        (collection, id),
    )?;
    Ok(())
}

fn remove_receipt(transaction: &Transaction<'_>, collection: &str, id: &str) -> Result<()> {
    transaction.execute(
        "UPDATE reports SET search_id = NULL WHERE collection_id = ?1 AND search_id = ?2",
        (collection, id),
    )?;
    transaction.execute(
        "DELETE FROM search_hits WHERE collection_id = ?1 AND search_id = ?2",
        (collection, id),
    )?;
    transaction.execute(
        "DELETE FROM searches WHERE collection_id = ?1 AND id = ?2",
        (collection, id),
    )?;
    Ok(())
}
