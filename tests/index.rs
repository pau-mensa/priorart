mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::FakeEncoder;
use lateweave::{SearchRequest, SearchResult};
use priorart::encoder::Encoder;
use priorart::index::{collection_index_path, CollectionIndexManager, Index};
use priorart::store::{IndexEntry, Store, Visibility, LOCAL_COLLECTION_ID, LOCAL_PRINCIPAL_ID};
use tempfile::TempDir;

const LOCAL: &str = LOCAL_COLLECTION_ID;

fn setup() -> (TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("priorart.sqlite")).unwrap();
    (directory, store)
}

fn open(directory: &TempDir, store: &Store, encoder: Option<Arc<dyn Encoder>>) -> Index {
    Index::open(directory.path(), store, encoder, LOCAL).unwrap()
}

fn fake() -> Option<Arc<dyn Encoder>> {
    Some(Arc::new(FakeEncoder::new()))
}

fn put(store: &Store, index: &mut Index, record: &str, text: &str) -> i64 {
    let created = store
        .put(
            LOCAL,
            text,
            None,
            Some(record),
            LOCAL_PRINCIPAL_ID,
            store.get(LOCAL, record, None).ok().map(|r| r.revision),
        )
        .unwrap();
    let encoded = index.encode(text).unwrap();
    index
        .upsert(store, record, created.revision, text, encoded)
        .unwrap();
    created.revision
}

fn search(index: &Index, text: &str, gather_limit: usize, limit: usize) -> SearchResult {
    let pipeline = index
        .pipeline(index.document_count(), gather_limit)
        .unwrap();
    pipeline
        .search(&index.query(text), &SearchRequest::new(gather_limit, limit))
        .unwrap()
}

fn top(index: &Index, result: &SearchResult) -> String {
    index.record_ids()[result.documents[0].document_id as usize].clone()
}

fn mirror(store: &Store) -> Vec<(u64, String, i64)> {
    store
        .index_documents(LOCAL)
        .unwrap()
        .into_iter()
        .map(
            |IndexEntry {
                 internal_id,
                 record_id,
                 revision,
             }| (internal_id, record_id, revision),
        )
        .collect()
}

#[test]
fn upsert_update_remove() {
    for encoder in [fake(), None] {
        let (directory, store) = setup();
        let vectors =
            || common::active_index_path(directory.path(), LOCAL_COLLECTION_ID).join("vectors");
        let with_vectors = encoder.is_some();
        let mut index = open(&directory, &store, encoder);
        assert_eq!(index.document_count(), 0);
        assert!(index.manifest().is_none());

        put(&store, &mut index, "a", "cuda illegal address");
        assert_eq!(index.document_count(), 1);
        assert_eq!(vectors().exists(), with_vectors);
        let generation = index.manifest().unwrap().generation();

        put(
            &store,
            &mut index,
            "a",
            "cuda illegal address, bf16 attention",
        );
        assert_eq!(index.document_count(), 1);
        assert_eq!(mirror(&store), [(0, "a".to_owned(), 2)]);
        assert!(index.manifest().unwrap().generation() > generation);

        index.remove(&store, "a").unwrap();
        assert_eq!(index.document_count(), 0);
        assert!(index.manifest().is_none());
        assert!(!vectors().exists());
        assert!(mirror(&store).is_empty());
    }
}

#[test]
fn removing_the_middle_renumbers() {
    let (directory, store) = setup();
    let mut index = open(&directory, &store, fake());
    put(&store, &mut index, "a", "alpha alpha");
    put(&store, &mut index, "b", "beta beta beta");
    put(&store, &mut index, "c", "gamma");
    index.remove(&store, "b").unwrap();
    assert_eq!(
        mirror(&store),
        [(0, "a".to_owned(), 1), (1, "c".to_owned(), 1)]
    );
    let vectors = index.vectors().unwrap();
    assert_eq!(vectors.document_count(), 2);
    assert_eq!(vectors.document_lengths(&[0, 1]).unwrap(), [2, 1]);
    let wanted = HashSet::from(["c".to_owned(), "zzz".to_owned()]);
    assert_eq!(index.internal_ids(&wanted), [1]);
}

#[test]
fn search_finds_the_right_document() {
    let (directory, store) = setup();
    let mut index = open(&directory, &store, fake());
    put(
        &store,
        &mut index,
        "a",
        "CUDA illegal address after switching attention to bf16",
    );
    put(
        &store,
        &mut index,
        "b",
        "pytest fixture scope error when using async",
    );
    put(
        &store,
        &mut index,
        "c",
        "NCCL timeout: one worker never reached the barrier",
    );
    let result = search(&index, "worker never reached barrier", 500, 5);
    assert_eq!(top(&index, &result), "c");
    assert_eq!(
        result.diagnostics.score_semantics,
        "int8-reconstructed-approximate-full-maxsim"
    );
    assert_eq!(result.diagnostics.gatherer, "ExhaustiveGatherer");
    let result = search(&index, "worker never reached barrier", 2, 2);
    assert_eq!(result.diagnostics.gatherer, "LexicalGatherer");
    assert_eq!(top(&index, &result), "c");
}

#[test]
fn lexical_only_search() {
    let (directory, store) = setup();
    let mut index = open(&directory, &store, None);
    put(&store, &mut index, "a", "cuda illegal address");
    put(&store, &mut index, "b", "nccl barrier timeout");
    let result = search(&index, "barrier", 500, 5);
    assert_eq!(top(&index, &result), "b");
    assert_eq!(result.diagnostics.score_semantics, "bm25-lucene");
}

#[test]
fn a_consistent_reopen_does_not_reencode() {
    let (directory, store) = setup();
    let encoder = Arc::new(FakeEncoder::new());
    let mut index = open(&directory, &store, Some(encoder.clone()));
    put(&store, &mut index, "a", "alpha");
    let calls = encoder.calls();
    let reopened = open(&directory, &store, Some(encoder.clone()));
    assert_eq!(reopened.document_count(), 1);
    assert_eq!(encoder.calls(), calls);
}

#[test]
fn a_mirror_mismatch_rebuilds() {
    let (directory, store) = setup();
    let mut index = open(&directory, &store, fake());
    put(&store, &mut index, "a", "alpha");
    put(&store, &mut index, "b", "beta");
    // A crash between the record write and the mirror write.
    store
        .replace_index_documents(LOCAL, &[("a", 1)], Some("fake"))
        .unwrap();
    let reopened = open(&directory, &store, fake());
    assert_eq!(reopened.record_ids(), ["a", "b"]);
    assert_eq!(reopened.vectors().unwrap().document_count(), 2);
    assert_eq!(
        mirror(&store),
        [(0, "a".to_owned(), 1), (1, "b".to_owned(), 1)]
    );
}

#[test]
fn a_stale_revision_or_missing_vectors_rebuild() {
    let (directory, store) = setup();
    let mut index = open(&directory, &store, fake());
    put(&store, &mut index, "a", "alpha");
    store
        .put(
            LOCAL,
            "alpha two",
            None,
            Some("a"),
            LOCAL_PRINCIPAL_ID,
            Some(1),
        )
        .unwrap();
    open(&directory, &store, fake());
    assert_eq!(mirror(&store), [(0, "a".to_owned(), 2)]);
    std::fs::remove_dir_all(
        common::active_index_path(directory.path(), LOCAL_COLLECTION_ID).join("vectors"),
    )
    .unwrap();
    let reopened = open(&directory, &store, fake());
    assert_eq!(reopened.vectors().unwrap().document_count(), 1);
}

#[test]
fn a_half_published_vector_store_rebuilds() {
    let (directory, store) = setup();
    let mut index = open(&directory, &store, fake());
    put(&store, &mut index, "a", "alpha");
    put(&store, &mut index, "b", "beta");
    drop(index);
    std::fs::write(
        common::active_index_path(directory.path(), LOCAL_COLLECTION_ID)
            .join("vectors/storage.json"),
        b"{",
    )
    .unwrap();
    let reopened = open(&directory, &store, fake());
    assert_eq!(reopened.vectors().unwrap().document_count(), 2);
}

#[test]
fn a_representation_change_rebuilds() {
    let (directory, store) = setup();
    let mut index = open(&directory, &store, fake());
    put(&store, &mut index, "a", "alpha");
    let other: Arc<dyn Encoder> = Arc::new(FakeEncoder::with("other", 8));
    let reopened = open(&directory, &store, Some(other));
    assert_eq!(reopened.vectors().unwrap().representation().dimension(), 8);
    let metadata = std::fs::read_to_string(
        common::active_index_path(directory.path(), LOCAL_COLLECTION_ID)
            .join("vectors/storage.json"),
    )
    .unwrap();
    assert!(metadata.contains("\"other\""));
}

#[test]
fn switching_encoders_on_and_off() {
    let (directory, store) = setup();
    let mut index = open(&directory, &store, None);
    put(&store, &mut index, "a", "alpha");
    assert!(
        !common::active_index_path(directory.path(), LOCAL_COLLECTION_ID)
            .join("vectors")
            .exists()
    );
    let reopened = open(&directory, &store, fake());
    assert_eq!(reopened.vectors().unwrap().document_count(), 1);
    let mut lexical = open(&directory, &store, None);
    assert!(lexical.vectors().is_none());
    assert_eq!(lexical.document_count(), 1);
    // Vectors left behind by an earlier encoder run are not trusted after
    // lexical-only writes, even when the document count matches.
    store.delete(LOCAL, "a", Some(1)).unwrap();
    lexical.remove(&store, "a").unwrap();
    put(&store, &mut lexical, "b", "beta beta gamma");
    let reopened = open(&directory, &store, fake());
    let result = search(&reopened, "beta", 500, 1);
    assert_eq!(top(&reopened, &result), "b");
    assert_eq!(reopened.vectors().unwrap().document_count(), 1);
    let stored = reopened.vectors().unwrap().fetch(&[0], None).unwrap();
    assert_eq!(stored.lengths(), [3]);
}

#[test]
fn lexical_scores_after_writes_match_a_reopened_index() {
    let (directory, store) = setup();
    let mut index = open(&directory, &store, None);
    put(&store, &mut index, "a", "alpha beta");
    put(&store, &mut index, "b", "beta gamma gamma");
    put(&store, &mut index, "c", "gamma delta alpha");
    put(&store, &mut index, "a", "delta delta beta");
    store.delete(LOCAL, "b", Some(1)).unwrap();
    index.remove(&store, "b").unwrap();
    assert_eq!(
        mirror(&store),
        [(0, "c".to_owned(), 1), (1, "a".to_owned(), 2)]
    );
    assert_eq!(index.record_ids(), ["c", "a"]);
    let reopened = open(&directory, &store, None);
    let ranked = |index: &Index| {
        let result = search(index, "alpha beta gamma delta", 500, 10);
        result
            .documents
            .iter()
            .map(|document| {
                (
                    index.record_ids()[document.document_id as usize].clone(),
                    document.score,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(ranked(&index), ranked(&reopened));
    assert_eq!(ranked(&index).len(), 2);
}

fn collection(store: &Store) -> String {
    let account = store.create_account(LOCAL_PRINCIPAL_ID).unwrap();
    store
        .create_collection(&account, Visibility::Restricted)
        .unwrap()
}

fn scoped_put(store: &Store, index: &mut Index, collection: &str, text: &str) {
    let encoded = index.encode(text).unwrap();
    let revision = store
        .put(
            collection,
            text,
            None,
            Some("same"),
            LOCAL_PRINCIPAL_ID,
            None,
        )
        .unwrap();
    index
        .upsert(store, "same", revision.revision, text, encoded)
        .unwrap();
}

#[test]
fn collection_rebuilds_and_mutations_are_isolated() {
    for encoder in [fake(), None] {
        let (directory, store) = setup();
        let other = collection(&store);
        let mut manager =
            CollectionIndexManager::new(directory.path(), encoder, 2.try_into().unwrap());
        scoped_put(&store, manager.get(&store, LOCAL).unwrap(), LOCAL, "alpha");
        scoped_put(
            &store,
            manager.get(&store, &other).unwrap(),
            &other,
            "beta beta",
        );
        let local_manifest = manager
            .get(&store, LOCAL)
            .unwrap()
            .manifest()
            .unwrap()
            .clone();
        let other_manifest = manager
            .get(&store, &other)
            .unwrap()
            .manifest()
            .unwrap()
            .clone();
        assert_ne!(local_manifest.corpus_id(), other_manifest.corpus_id());
        let other_path = common::active_index_path(directory.path(), &other).join("manifest.json");
        let other_bytes = std::fs::read(&other_path).unwrap();
        let other_mirror = store.index_documents(&other).unwrap();
        let index = manager.get(&store, LOCAL).unwrap();
        index.rebuild(&store).unwrap();
        assert_eq!(index.record_ids(), ["same"]);
        assert!(index
            .pipeline(2, 1)
            .unwrap()
            .search(&index.query("beta"), &SearchRequest::new(1, 1))
            .unwrap()
            .documents
            .is_empty());
        store.delete(LOCAL, "same", Some(1)).unwrap();
        index.remove(&store, "same").unwrap();
        assert_eq!(std::fs::read(other_path).unwrap(), other_bytes);
        assert_eq!(store.index_documents(&other).unwrap(), other_mirror);
        let index = manager.get(&store, &other).unwrap();
        assert_eq!(index.manifest().unwrap(), &other_manifest);
        assert_eq!(top(index, &search(index, "beta", 500, 5)), "same");
        assert_eq!(index.revisions(), [1]);
    }
}

#[test]
fn lru_eviction_reopens_without_reencoding_or_changing_generation() {
    let (directory, store) = setup();
    let other = collection(&store);
    let third = collection(&store);
    let encoder = Arc::new(FakeEncoder::new());
    let mut manager = CollectionIndexManager::new(
        directory.path(),
        Some(encoder.clone()),
        2.try_into().unwrap(),
    );
    scoped_put(&store, manager.get(&store, LOCAL).unwrap(), LOCAL, "alpha");
    let manifest = manager
        .get(&store, LOCAL)
        .unwrap()
        .manifest()
        .unwrap()
        .clone();
    scoped_put(&store, manager.get(&store, &other).unwrap(), &other, "beta");
    manager.get(&store, LOCAL).unwrap(); // touch local, evict other
    manager.get(&store, &third).unwrap();
    assert!(manager.loaded(&other).is_none());
    assert!(manager.loaded(LOCAL).is_some());
    manager.get(&store, &other).unwrap(); // evict local
    assert!(manager.loaded(LOCAL).is_none());
    let calls = encoder.calls();
    assert_eq!(
        manager.get(&store, LOCAL).unwrap().manifest().unwrap(),
        &manifest
    );
    assert_eq!(encoder.calls(), calls);
    assert_eq!(manager.loaded_count(), 2);
    assert!(manager.get(&store, "missing").is_err());
    assert_eq!(manager.loaded_count(), 2);
}

#[test]
fn generated_storage_components_do_not_interpret_collection_ids_as_paths() {
    let (directory, store) = setup();
    let ids = [
        "local",
        "LOCAL",
        "../local",
        "/tmp/local",
        "a/b",
        "a_b",
        "..",
        "",
    ];
    let paths: HashSet<_> = ids
        .iter()
        .map(|id| collection_index_path(directory.path(), id))
        .collect();
    assert_eq!(paths.len(), ids.len());
    for path in paths {
        assert_eq!(path.parent().unwrap(), directory.path().join("indexes"));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), 64);
        assert!(name.bytes().all(|c| c.is_ascii_hexdigit()));
    }
    assert!(Index::open(directory.path(), &store, None, "../missing").is_err());
    assert!(!directory.path().join("indexes").exists());
}

#[test]
fn foreign_or_mismatched_manifests_rebuild_only_the_selected_collection() {
    let (directory, store) = setup();
    let other = collection(&store);
    let encoder = Arc::new(FakeEncoder::new());
    let mut local = open(&directory, &store, Some(encoder.clone()));
    scoped_put(&store, &mut local, LOCAL, "alpha");
    let mut second = Index::open(directory.path(), &store, Some(encoder.clone()), &other).unwrap();
    scoped_put(&store, &mut second, &other, "beta");
    let local_path = common::active_index_path(directory.path(), LOCAL).join("manifest.json");
    let other_path = common::active_index_path(directory.path(), &other).join("manifest.json");
    let other_bytes = std::fs::read(&other_path).unwrap();
    std::fs::copy(&other_path, &local_path).unwrap();
    let calls = encoder.calls();
    let reopened = open(&directory, &store, Some(encoder.clone()));
    assert_eq!(encoder.calls(), calls + 1);
    assert_eq!(reopened.manifest().unwrap().corpus_id(), LOCAL);
    for field in ["representation", "records", "vector_generation", "recipe"] {
        let local_path = common::active_index_path(directory.path(), LOCAL).join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&local_path).unwrap()).unwrap();
        match field {
            "representation" => manifest[field]["dimension"] = 99.into(),
            "records" => manifest[field][0][1] = 99.into(),
            "vector_generation" => manifest[field] = 999.into(),
            _ => manifest[field] = "unknown-recipe".into(),
        }
        std::fs::write(&local_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let calls = encoder.calls();
        open(&directory, &store, Some(encoder.clone()));
        assert_eq!(encoder.calls(), calls + 1, "{field}");
    }
    assert_eq!(std::fs::read(other_path).unwrap(), other_bytes);
}

#[test]
fn failed_rebuild_and_failed_load_remain_recoverable() {
    let (directory, store) = setup();
    let other = collection(&store);
    let encoder = Arc::new(FakeEncoder::new());
    let mut manager = CollectionIndexManager::new(
        directory.path(),
        Some(encoder.clone()),
        1.try_into().unwrap(),
    );
    scoped_put(&store, manager.get(&store, LOCAL).unwrap(), LOCAL, "alpha");
    store
        .put(&other, "beta", None, Some("same"), LOCAL_PRINCIPAL_ID, None)
        .unwrap();
    encoder.set_failing(true);
    assert!(manager.get(&store, LOCAL).unwrap().rebuild(&store).is_err());
    assert!(manager.loaded(LOCAL).unwrap().pipeline(1, 1).is_err());
    assert!(manager.get(&store, LOCAL).is_err());
    assert!(manager.get(&store, &other).is_err());
    assert_eq!(manager.loaded_count(), 0);
    encoder.set_failing(false);
    let index = manager.get(&store, LOCAL).unwrap();
    assert_eq!(top(index, &search(index, "alpha", 500, 5)), "same");
    let index = manager.get(&store, &other).unwrap();
    assert_eq!(top(index, &search(index, "beta", 500, 5)), "same");
}
