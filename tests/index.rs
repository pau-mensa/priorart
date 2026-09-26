mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::FakeEncoder;
use lateweave::{SearchRequest, SearchResult};
use priorart::encoder::Encoder;
use priorart::index::{Index, IndexError};
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
        .put(LOCAL, text, None, Some(record), LOCAL_PRINCIPAL_ID)
        .unwrap();
    index.upsert(store, record, created.revision, text).unwrap();
    created.revision
}

fn search(index: &mut Index, text: &str, gather_limit: usize, limit: usize) -> SearchResult {
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
        let vectors = directory.path().join("vectors");
        let with_vectors = encoder.is_some();
        let mut index = open(&directory, &store, encoder);
        assert_eq!(index.document_count(), 0);
        assert!(index.manifest().is_none());
        assert!(!vectors.exists());

        put(&store, &mut index, "a", "cuda illegal address");
        assert_eq!(index.document_count(), 1);
        assert_eq!(vectors.exists(), with_vectors);
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
        assert!(!vectors.exists());
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
    let result = search(&mut index, "worker never reached barrier", 500, 5);
    assert_eq!(top(&index, &result), "c");
    assert_eq!(
        result.diagnostics.score_semantics,
        "int8-reconstructed-approximate-full-maxsim"
    );
    assert_eq!(result.diagnostics.gatherer, "ExhaustiveGatherer");
    let result = search(&mut index, "worker never reached barrier", 2, 2);
    assert_eq!(result.diagnostics.gatherer, "LexicalGatherer");
    assert_eq!(top(&index, &result), "c");
}

#[test]
fn lexical_only_search() {
    let (directory, store) = setup();
    let mut index = open(&directory, &store, None);
    put(&store, &mut index, "a", "cuda illegal address");
    put(&store, &mut index, "b", "nccl barrier timeout");
    let result = search(&mut index, "barrier", 500, 5);
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
        .put(LOCAL, "alpha two", None, Some("a"), LOCAL_PRINCIPAL_ID)
        .unwrap();
    open(&directory, &store, fake());
    assert_eq!(mirror(&store), [(0, "a".to_owned(), 2)]);
    std::fs::remove_dir_all(directory.path().join("vectors")).unwrap();
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
    std::fs::write(directory.path().join("vectors/storage.json"), b"{").unwrap();
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
    let metadata = std::fs::read_to_string(directory.path().join("vectors/storage.json")).unwrap();
    assert!(metadata.contains("\"other\""));
}

#[test]
fn switching_encoders_on_and_off() {
    let (directory, store) = setup();
    let mut index = open(&directory, &store, None);
    put(&store, &mut index, "a", "alpha");
    assert!(!directory.path().join("vectors").exists());
    let reopened = open(&directory, &store, fake());
    assert_eq!(reopened.vectors().unwrap().document_count(), 1);
    let mut lexical = open(&directory, &store, None);
    assert!(lexical.vectors().is_none());
    assert_eq!(lexical.document_count(), 1);
    // Vectors left behind by an earlier encoder run are not trusted after
    // lexical-only writes, even when the document count matches.
    store.delete(LOCAL, "a").unwrap();
    lexical.remove(&store, "a").unwrap();
    put(&store, &mut lexical, "b", "beta beta gamma");
    let mut reopened = open(&directory, &store, fake());
    let result = search(&mut reopened, "beta", 500, 1);
    assert_eq!(top(&reopened, &result), "b");
    assert_eq!(reopened.vectors().unwrap().document_count(), 1);
    let stored = reopened.vectors().unwrap().fetch(&[0], None).unwrap();
    assert_eq!(stored.lengths(), [3]);
}

#[test]
fn only_the_local_collection_is_indexed() {
    let (directory, store) = setup();
    let account = store.create_account(LOCAL_PRINCIPAL_ID).unwrap();
    let other = store
        .create_collection(&account, Visibility::Restricted)
        .unwrap();
    assert!(matches!(
        Index::open(directory.path(), &store, None, &other),
        Err(IndexError::UnsupportedCollection)
    ));
}
