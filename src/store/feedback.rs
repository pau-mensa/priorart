use super::*;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SearchReceipt {
    pub collection_id: String,
    pub id: String,
    pub created_at: String,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Serialize)]
pub struct PublishedReport {
    pub collection_id: String,
    pub id: String,
    pub record_id: String,
    pub revision: i64,
    pub text: String,
    pub created_at: String,
}

impl Store {
    pub(crate) fn search_receipt(
        &self,
        collection: &str,
        id: &str,
        principal: &str,
    ) -> Result<Option<SearchReceipt>> {
        let created_at = self.connection.query_row(
            "SELECT created_at FROM searches WHERE collection_id = ?1 AND id = ?2 AND requester_principal_id = ?3",
            (collection, id, principal), |r| r.get::<_, String>(0),
        ).optional()?;
        let Some(created_at) = created_at else {
            return Ok(None);
        };
        let mut statement = self.connection.prepare(
            "SELECT record_id, revision FROM search_hits WHERE collection_id = ?1 AND search_id = ?2 ORDER BY position",
        )?;
        let hits = statement
            .query_map((collection, id), |r| {
                Ok(SearchHit {
                    record_id: r.get(0)?,
                    revision: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Some(SearchReceipt {
            collection_id: collection.into(),
            id: id.into(),
            created_at,
            hits,
        }))
    }

    pub(crate) fn own_report_target(
        &self,
        collection: &str,
        id: &str,
        principal: &str,
    ) -> Result<Option<(String, i64)>> {
        Ok(self.connection.query_row(
            "SELECT record_id, revision FROM reports WHERE collection_id = ?1 AND id = ?2 AND reporter_principal_id = ?3 AND revision IS NOT NULL",
            (collection, id, principal), |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?)
    }

    pub(crate) fn published_reports(
        &self,
        collection: &str,
        record: &str,
    ) -> Result<Vec<PublishedReport>> {
        let mut statement = self.connection.prepare(
            "SELECT collection_id, id, record_id, revision, text, created_at FROM published_reports WHERE collection_id = ?1 AND record_id = ?2 ORDER BY created_at, id",
        )?;
        let reports = statement
            .query_map((collection, record), |r| {
                Ok(PublishedReport {
                    collection_id: r.get(0)?,
                    id: r.get(1)?,
                    record_id: r.get(2)?,
                    revision: r.get(3)?,
                    text: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(reports)
    }
}
