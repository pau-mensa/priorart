//! Durable minimal mutation receipts, committed atomically with their content.
use super::*;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mutation<T> {
    pub mutation_id: String,
    pub value: T,
}

pub(crate) struct Intent<'a> {
    pub collection: &'a str,
    pub principal: &'a str,
    pub operation: &'a str,
    pub key: Option<&'a str>,
    pub payload: String,
    pub authority: &'a str,
}

impl Store {
    pub(crate) fn replay<T: DeserializeOwned>(
        &self,
        intent: &Intent<'_>,
    ) -> Result<Option<(Mutation<T>, String)>> {
        replay(&self.connection, intent)
    }

    pub(crate) fn commit_put(
        &self,
        intent: &Intent<'_>,
        text: &str,
        truncated: bool,
        metadata: Option<&Metadata>,
        record: Option<&str>,
        expected: Option<i64>,
    ) -> Result<Mutation<(String, i64, bool)>> {
        self.commit_mutation(intent, |transaction| {
            let record = put_in(
                transaction,
                intent.collection,
                Content {
                    text,
                    truncated,
                    metadata,
                },
                record,
                intent.principal,
                expected,
            )?;
            Ok((
                (record.record_id.clone(), record.revision, truncated),
                record.record_id,
            ))
        })
    }

    pub(crate) fn commit_delete(
        &self,
        intent: &Intent<'_>,
        record: &str,
        expected: Option<i64>,
    ) -> Result<Mutation<()>> {
        self.commit_mutation(intent, |transaction| {
            delete_in(transaction, intent.collection, record, expected)?;
            Ok(((), record.to_owned()))
        })
    }

    pub(super) fn commit_mutation<T: Serialize + DeserializeOwned>(
        &self,
        intent: &Intent<'_>,
        change: impl FnOnce(&Transaction<'_>) -> Result<(T, String)>,
    ) -> Result<Mutation<T>> {
        let transaction = self.write()?;
        if let Some((result, _)) = replay(&transaction, intent)? {
            return Ok(result);
        }
        let (value, target_record) = change(&transaction)?;
        let mutation_id = new_id();
        transaction.execute(
            "INSERT INTO mutations VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            rusqlite::params![
                mutation_id,
                intent.collection,
                intent.principal,
                intent.operation,
                intent.key.map(digest),
                intent.payload,
                intent.authority,
                serde_json::to_string(&value).map_err(|_| StoreError::Journal)?,
                now(),
                target_record
            ],
        )?;
        transaction.commit()?;
        Ok(Mutation { mutation_id, value })
    }
}

fn replay<T: DeserializeOwned>(
    connection: &Connection,
    intent: &Intent<'_>,
) -> Result<Option<(Mutation<T>, String)>> {
    let Some(key) = intent.key else {
        return Ok(None);
    };
    let row: Option<(String, String, String, String)> = connection.query_row(
        "SELECT id, payload_digest, result, authority FROM mutations WHERE collection_id = ?1 AND principal_id = ?2 AND operation = ?3 AND idempotency_digest = ?4",
        (intent.collection, intent.principal, intent.operation, digest(key)),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
    row.map(|(id, payload, value, authority)| {
        if payload.is_empty() {
            return Err(StoreError::MutationPurged);
        }
        if payload != intent.payload {
            return Err(StoreError::IdempotencyConflict);
        }
        let value = serde_json::from_str(&value).map_err(|_| StoreError::Journal)?;
        Ok((
            Mutation {
                mutation_id: id,
                value,
            },
            authority,
        ))
    })
    .transpose()
}
