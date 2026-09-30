use priorart::auth::{Grant, Operation as Op, RequestContext};
use priorart::config::Settings;
use priorart::policy::PolicyError;
use priorart::service::{Hit, Service, ServiceError, DATABASE_FILE};
use priorart::store::{Metadata, Store, Visibility};
use serde_json::json;
use tempfile::TempDir;

const LEFT: [(&str, &str); 3] = [
    (
        "cuda",
        "CUDA illegal address after switching attention to bf16",
    ),
    (
        "pytest",
        "pytest fixture scope error when using async fixtures",
    ),
    ("barrier", "worker timeout at the barrier"),
];
const RIGHT: [(&str, &str); 3] = [
    ("nccl", "NCCL timeout: one worker never reached the barrier"),
    (
        "oom",
        "CUDA out of memory with a large batch, fixed by halving it",
    ),
    ("lock", "deadlock when two workers take the same lock"),
];

struct Fixture {
    _dir: TempDir,
    store: Store,
    principal: String,
    /// Sorted, so `x < y`.
    x: String,
    y: String,
    /// Holds LEFT and RIGHT together.
    whole: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join(DATABASE_FILE)).unwrap();
        let principal = store.create_principal().unwrap();
        let mut ids: Vec<String> = (0..3)
            .map(|_| {
                store
                    .create_collection(&principal, Visibility::Restricted)
                    .unwrap()
            })
            .collect();
        let whole = ids.pop().unwrap();
        ids.sort();
        let [x, y] = <[String; 2]>::try_from(ids).unwrap();
        let fixture = Self {
            _dir: dir,
            store,
            principal,
            x,
            y,
            whole,
        };
        for (collection, docs) in [(&fixture.x, LEFT), (&fixture.y, RIGHT)] {
            for (id, text) in docs {
                fixture.put(collection, id, text, None);
                fixture.put(&fixture.whole, id, text, None);
            }
        }
        fixture
    }

    fn put(&self, collection: &str, id: &str, text: &str, metadata: Option<&Metadata>) {
        self.store
            .put(collection, text, metadata, Some(id), &self.principal, None)
            .unwrap();
    }

    fn service(&self, max_loaded_indexes: usize) -> Service {
        Service::open(Settings {
            data_dir: self._dir.path().to_owned(),
            max_loaded_indexes,
            ..Settings::default()
        })
        .unwrap()
    }

    fn context(&self, service: &Service, collections: &[&String]) -> RequestContext {
        let grants: Vec<_> = collections
            .iter()
            .map(|collection| Grant::new(*collection, Op::Read))
            .collect();
        let key = self
            .store
            .issue_credential(&self.principal, &grants, None)
            .unwrap()
            .into_secret();
        service.authenticate(&key).unwrap()
    }
}

fn scored(hits: &[Hit]) -> Vec<(String, f64)> {
    let mut scored: Vec<_> = hits.iter().map(|hit| (hit.id.clone(), hit.score)).collect();
    scored.sort_by(|left, right| left.0.cmp(&right.0));
    scored
}

#[test]
fn split_collections_score_as_one_corpus_and_merge_by_score() {
    let f = Fixture::new();
    let service = f.service(8);
    let context = f.context(&service, &[&f.x, &f.y, &f.whole]);
    for query in ["worker barrier timeout", "cuda", "fixed lock"] {
        let split = service
            .search(&context, &[&f.y, &f.x], query, None, 10)
            .unwrap();
        let whole = service
            .search(&context, &[&f.whole], query, None, 10)
            .unwrap();
        assert!(!split.is_empty(), "{query}");
        assert_eq!(scored(&split), scored(&whole), "{query}");
        assert!(split.windows(2).all(|pair| pair[0].score >= pair[1].score));
        for hit in &split {
            let expected = if LEFT.iter().any(|(id, _)| *id == hit.id) {
                &f.x
            } else {
                &f.y
            };
            assert_eq!(&hit.collection_id, expected);
        }
    }
    let best = service
        .search(&context, &[&f.x, &f.y], "worker barrier timeout", None, 1)
        .unwrap();
    assert_eq!(best.len(), 1);
    assert_eq!(best[0].id, "barrier");
}

#[test]
fn equal_scores_tie_by_collection_then_record_and_filters_apply_per_collection() {
    let f = Fixture::new();
    let tag = |value: &str| json!({"tag": value}).as_object().unwrap().clone();
    for collection in [&f.y, &f.x] {
        f.put(collection, "twin-b", "sentinel twin", Some(&tag("b")));
        f.put(collection, "twin-a", "sentinel twin", Some(&tag("a")));
    }
    let service = f.service(8);
    let context = f.context(&service, &[&f.x, &f.y]);
    let hits = service
        .search(&context, &[&f.y, &f.x], "sentinel", None, 10)
        .unwrap();
    let order: Vec<_> = hits
        .iter()
        .map(|hit| (hit.collection_id.as_str(), hit.id.as_str()))
        .collect();
    assert_eq!(
        order,
        [
            (f.x.as_str(), "twin-a"),
            (f.x.as_str(), "twin-b"),
            (f.y.as_str(), "twin-a"),
            (f.y.as_str(), "twin-b"),
        ]
    );
    assert!(hits.iter().all(|hit| hit.score == hits[0].score));
    let filtered = service
        .search(&context, &[&f.x, &f.y], "sentinel", Some(&tag("b")), 10)
        .unwrap();
    let order: Vec<_> = filtered
        .iter()
        .map(|hit| (hit.collection_id.as_str(), hit.id.as_str()))
        .collect();
    assert_eq!(order, [(f.x.as_str(), "twin-b"), (f.y.as_str(), "twin-b")]);
}

#[test]
fn one_unreadable_collection_rejects_the_whole_selection() {
    let f = Fixture::new();
    let service = f.service(8);
    let context = f.context(&service, &[&f.x]);
    for selection in [vec![&f.x, &f.y], vec![&f.y, &f.x]] {
        assert!(matches!(
            service.search(&context, &selection, "cuda", None, 10),
            Err(ServiceError::Policy(PolicyError::Unavailable))
        ));
    }
    let unknown = "unknown".to_owned();
    assert!(matches!(
        service.search(&context, &[&f.x, &unknown], "cuda", None, 10),
        Err(ServiceError::Policy(PolicyError::Unavailable))
    ));
    for selection in [vec![], vec![&f.x, &f.x]] {
        assert!(matches!(
            service.search(&context, &selection, "cuda", None, 10),
            Err(ServiceError::InvalidInput(_))
        ));
    }
    assert!(matches!(
        service.search(&RequestContext::anonymous(), &[&f.x], "cuda", None, 10),
        Err(ServiceError::Policy(PolicyError::Unavailable))
    ));
}

#[test]
fn a_selection_fits_in_the_loaded_index_cache() {
    let f = Fixture::new();
    let service = f.service(2);
    let context = f.context(&service, &[&f.x, &f.y, &f.whole]);
    assert!(matches!(
        service.search(&context, &[&f.x, &f.y, &f.whole], "cuda", None, 10),
        Err(ServiceError::InvalidInput(_))
    ));
    let pair = service
        .search(&context, &[&f.x, &f.y], "cuda", None, 10)
        .unwrap();
    let evicting = service
        .search(&context, &[&f.whole, &f.y], "cuda", None, 10)
        .unwrap();
    assert_eq!(pair.len(), 2);
    assert_eq!(evicting.len(), 3);
    assert_eq!(
        service
            .search(&context, &[&f.x, &f.y], "cuda", None, 10)
            .unwrap(),
        pair
    );
}
