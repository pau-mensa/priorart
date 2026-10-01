use std::any::Any;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use priorart::auth::RequestContext;
use priorart::config::Settings;
use priorart::lateweave::{
    Candidate, CandidateGenerator, DocumentKey, Gathered, Query, Requirements, SearchPipeline,
    Subset,
};
use priorart::recipe::{self, CollectionIndex, IndexDocument, Recipe};
use priorart::service::{Service, WriteOptions};
use priorart::store::LOCAL_COLLECTION_ID;

/// Ranks every record by descending ID whatever the query says, and fails to
/// index text containing "unindexable".
#[derive(Default)]
struct Reverse {
    loads: AtomicUsize,
}

struct Ids(BTreeSet<String>);

impl CollectionIndex for Ids {
    fn upsert(&mut self, record_id: &str, _: i64, text: &str) -> recipe::Result<()> {
        if text.contains("unindexable") {
            return Err("the encoder is down".into());
        }
        self.0.insert(record_id.to_owned());
        Ok(())
    }

    fn remove(&mut self, record_id: &str) -> recipe::Result<()> {
        self.0.remove(record_id);
        Ok(())
    }
}

impl Recipe for Reverse {
    fn load(
        &self,
        _: &str,
        documents: Vec<IndexDocument>,
    ) -> recipe::Result<Box<dyn CollectionIndex>> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(Ids(documents
            .into_iter()
            .map(|document| document.record_id)
            .collect())))
    }

    fn pipeline(
        &self,
        _: &Query,
        indexes: &[(&str, &dyn CollectionIndex)],
    ) -> recipe::Result<SearchPipeline> {
        let keys = indexes
            .iter()
            .flat_map(|&(collection, index)| {
                let ids = &(index as &dyn Any).downcast_ref::<Ids>().unwrap().0;
                ids.iter()
                    .map(|id| DocumentKey::new(collection, id.as_str()))
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok(SearchPipeline::new(
            Arc::new(Fixed {
                keys,
                requires: Requirements::new(),
            }),
            None,
        ))
    }
}

struct Fixed {
    keys: Vec<DocumentKey>,
    requires: Requirements,
}

impl CandidateGenerator for Fixed {
    fn requires(&self) -> &Requirements {
        &self.requires
    }

    fn score_semantics(&self) -> &str {
        "reverse-id"
    }

    fn gather(
        &self,
        _: &Query,
        limit: usize,
        subset: Option<&Subset>,
    ) -> priorart::lateweave::Result<Gathered> {
        let mut keys: Vec<&DocumentKey> = self
            .keys
            .iter()
            .filter(|key| {
                subset.is_none_or(|subset| {
                    subset
                        .ids(key.corpus())
                        .is_some_and(|ids| ids.contains(key.id()))
                })
            })
            .collect();
        keys.sort_by(|left, right| right.id().cmp(left.id()));
        keys.truncate(limit);
        let candidates = keys
            .into_iter()
            .enumerate()
            .map(|(rank, key)| Candidate {
                key: key.clone(),
                gather_score: 100.0 - rank as f32,
                gather_rank: rank,
                provenance: "reverse".to_owned(),
            })
            .collect();
        Ok(Gathered {
            candidates,
            as_of: SystemTime::now(),
        })
    }
}

#[test]
fn another_recipe_ranks_and_a_failed_update_rebuilds_its_index() {
    let directory = tempfile::tempdir().unwrap();
    let recipe = Arc::new(Reverse::default());
    let service = Service::open_with(
        Settings {
            data_dir: directory.path().to_owned(),
            ..Settings::default()
        },
        recipe.clone(),
    )
    .unwrap();
    let context = RequestContext::local();
    let put = |id: &str, text: &str, lang: &str| {
        let metadata = serde_json::json!({ "lang": lang });
        service
            .put(
                &context,
                LOCAL_COLLECTION_ID,
                text,
                metadata.as_object(),
                Some(id),
                WriteOptions::default(),
            )
            .unwrap()
    };
    let search = |filters: Option<serde_json::Value>| {
        service
            .search(
                &context,
                &[LOCAL_COLLECTION_ID],
                "words no record contains",
                filters.as_ref().and_then(|f| f.as_object()),
                10,
            )
            .unwrap()
            .into_iter()
            .map(|hit| (hit.id, hit.revision))
            .collect::<Vec<_>>()
    };
    put("a", "first", "rust");
    put("b", "second", "python");
    assert_eq!(search(None), [("b".into(), 1), ("a".into(), 1)]);
    assert_eq!(
        search(Some(serde_json::json!({"lang": "rust"}))),
        [("a".into(), 1)]
    );
    assert_eq!(recipe.loads.load(Ordering::Relaxed), 1);

    put("c", "unindexable", "rust");
    assert_eq!(recipe.loads.load(Ordering::Relaxed), 1);
    assert_eq!(
        search(None),
        [("c".into(), 1), ("b".into(), 1), ("a".into(), 1)]
    );
    assert_eq!(recipe.loads.load(Ordering::Relaxed), 2);
    put("a", "revised", "rust");
    assert_eq!(search(None)[2], ("a".into(), 2));
}

/// BM25, except that each search waits for `parties` searches to be ranking at once.
struct Rendezvous {
    parties: usize,
    arrived: AtomicUsize,
}

impl Recipe for Rendezvous {
    fn load(
        &self,
        collection: &str,
        documents: Vec<IndexDocument>,
    ) -> recipe::Result<Box<dyn CollectionIndex>> {
        priorart::gather::Bm25Recipe.load(collection, documents)
    }

    fn pipeline(
        &self,
        query: &Query,
        indexes: &[(&str, &dyn CollectionIndex)],
    ) -> recipe::Result<SearchPipeline> {
        self.arrived.fetch_add(1, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.arrived.load(Ordering::SeqCst) < self.parties {
            if std::time::Instant::now() > deadline {
                return Err("searches on one collection did not overlap".into());
            }
            std::thread::yield_now();
        }
        priorart::gather::Bm25Recipe.pipeline(query, indexes)
    }
}

#[test]
fn searches_on_one_collection_run_concurrently() {
    let directory = tempfile::tempdir().unwrap();
    let recipe = Arc::new(Rendezvous {
        parties: 4,
        arrived: AtomicUsize::new(0),
    });
    let service = Service::open_with(
        Settings {
            data_dir: directory.path().to_path_buf(),
            ..Settings::default()
        },
        recipe.clone(),
    )
    .unwrap();
    let local = RequestContext::local();
    service
        .put(
            &local,
            LOCAL_COLLECTION_ID,
            "nccl barrier timeout",
            None,
            Some("nccl"),
            WriteOptions::default(),
        )
        .unwrap();
    let search = || {
        let hits = service
            .search(&local, &[LOCAL_COLLECTION_ID], "barrier", None, 5)
            .unwrap();
        assert_eq!(hits[0].id, "nccl");
    };
    std::thread::scope(|scope| {
        // The first search is ranking, and holding the collection, before the rest start.
        let first = scope.spawn(search);
        while recipe.arrived.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        let rest: Vec<_> = (1..recipe.parties).map(|_| scope.spawn(search)).collect();
        for search in std::iter::once(first).chain(rest) {
            search.join().unwrap();
        }
    });
}
