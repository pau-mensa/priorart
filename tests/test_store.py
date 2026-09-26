import pytest

from priorart.store import (
    LOCAL_COLLECTION_ID,
    LOCAL_PRINCIPAL_ID,
    RecordDeleted,
    RecordNotFound,
    Store,
)


def put(store, *args, **kwargs):
    ref = store.put(*args, **kwargs)
    return ref.record_id, ref.revision


@pytest.fixture
def store(tmp_path):
    store = Store(tmp_path / "priorart.sqlite")
    yield store
    store.close()


def test_put_creates_and_get_returns_latest(store):
    record_id, revision = put(
        store,
        "first",
        {"lang": "python"},
        None,
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    assert revision == 1
    got = store.get(record_id, None, collection_id=LOCAL_COLLECTION_ID)
    assert got.text == "first"
    assert got.metadata == {"lang": "python"}
    assert got.revision == 1
    assert got.text_sha256
    assert got.created_at.endswith("Z")


def test_second_put_appends_revision(store):
    record_id, _ = put(
        store,
        "first",
        None,
        None,
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    same_id, revision = put(
        store,
        "second",
        {"k": 1},
        record_id,
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    assert same_id == record_id
    assert revision == 2
    assert store.get(record_id, None, collection_id=LOCAL_COLLECTION_ID).text == "second"
    assert store.get(record_id, 1, collection_id=LOCAL_COLLECTION_ID).text == "first"
    with pytest.raises(RecordNotFound):
        store.get(record_id, 3, collection_id=LOCAL_COLLECTION_ID)


def test_client_chosen_id(store):
    record_id, revision = put(
        store,
        "x",
        None,
        "my-id",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    assert (record_id, revision) == ("my-id", 1)


def test_unknown_get_raises(store):
    with pytest.raises(RecordNotFound):
        store.get("nope", None, collection_id=LOCAL_COLLECTION_ID)


def test_delete_nulls_text_and_blocks_reuse(store):
    record_id, _ = put(
        store,
        "secret",
        {"a": 1},
        None,
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    put(
        store,
        "secret 2",
        None,
        record_id,
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    assert store.delete(record_id, collection_id=LOCAL_COLLECTION_ID) is True
    assert store.delete(record_id, collection_id=LOCAL_COLLECTION_ID) is False
    with pytest.raises(RecordDeleted):
        store.get(record_id, None, collection_id=LOCAL_COLLECTION_ID)
    with pytest.raises(RecordDeleted):
        store.get(record_id, 1, collection_id=LOCAL_COLLECTION_ID)
    with pytest.raises(RecordDeleted):
        put(
            store,
            "again",
            None,
            record_id,
            collection_id=LOCAL_COLLECTION_ID,
            author_principal_id=LOCAL_PRINCIPAL_ID,
        )
    with pytest.raises(RecordNotFound):
        store.delete("nope", collection_id=LOCAL_COLLECTION_ID)
    assert store.live_documents(collection_id=LOCAL_COLLECTION_ID) == []
    # Tombstone keeps the digest of what was there.
    rows = store._connection.execute(
        "SELECT text, metadata, text_sha256 FROM revisions WHERE record_id = ?", (record_id,)
    ).fetchall()
    assert len(rows) == 2
    assert all(row[0] is None and row[1] is None and row[2] for row in rows)


def test_live_documents_latest_and_ordered(store):
    put(
        store,
        "a1",
        None,
        "a",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    put(
        store,
        "b1",
        None,
        "b",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    put(
        store,
        "a2",
        None,
        "a",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    put(
        store,
        "c1",
        None,
        "c",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    store.delete("c", collection_id=LOCAL_COLLECTION_ID)
    docs = store.live_documents(collection_id=LOCAL_COLLECTION_ID)
    assert [(d.record_id, d.revision, d.text) for d in docs] == [("a", 2, "a2"), ("b", 1, "b1")]


def test_matching_record_ids(store):
    put(
        store,
        "x",
        {"lang": "python", "gpu": True},
        "p",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    put(
        store,
        "y",
        {"lang": "rust"},
        "r",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    put(
        store,
        "z",
        {"lang": "python"},
        "p",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )  # revision 2 keeps python
    put(
        store,
        "w",
        None,
        "n",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    assert store.matching_record_ids({"lang": "python"}, collection_id=LOCAL_COLLECTION_ID) == {"p"}
    assert (
        store.matching_record_ids(
            {"lang": "python", "gpu": True}, collection_id=LOCAL_COLLECTION_ID
        )
        == set()
    )
    assert store.matching_record_ids({"lang": "rust"}, collection_id=LOCAL_COLLECTION_ID) == {"r"}
    assert store.matching_record_ids({"missing": 1}, collection_id=LOCAL_COLLECTION_ID) == set()


def test_reports_round_trip_including_deleted(store):
    put(
        store,
        "x",
        None,
        "rec",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    search_id = store.log_search(
        "q",
        None,
        [{"collection_id": LOCAL_COLLECTION_ID, "id": "rec", "revision": 1, "score": 1.0}],
        {},
        collection_id=LOCAL_COLLECTION_ID,
        requester_principal_id=LOCAL_PRINCIPAL_ID,
    )
    assert store.has_search(search_id, collection_id=LOCAL_COLLECTION_ID)
    assert not store.has_search("nope", collection_id=LOCAL_COLLECTION_ID)
    report_id = store.add_report(
        "rec",
        1,
        search_id,
        "worked",
        collection_id=LOCAL_COLLECTION_ID,
        reporter_principal_id=LOCAL_PRINCIPAL_ID,
    )
    store.delete("rec", collection_id=LOCAL_COLLECTION_ID)
    reports = store.reports_for("rec", collection_id=LOCAL_COLLECTION_ID)
    assert [r.id for r in reports] == [report_id]
    assert reports[0].search_id == search_id
    assert reports[0].revision == 1
    assert reports[0].text == "worked"
    with pytest.raises(RecordNotFound):
        store.add_report(
            "nope",
            None,
            None,
            "x",
            collection_id=LOCAL_COLLECTION_ID,
            reporter_principal_id=LOCAL_PRINCIPAL_ID,
        )


def test_index_documents_mirror(store):
    for record_id in ("a", "b"):
        for _ in range(2):
            put(
                store,
                "text",
                None,
                record_id,
                collection_id=LOCAL_COLLECTION_ID,
                author_principal_id=LOCAL_PRINCIPAL_ID,
            )
    assert store.index_documents(collection_id=LOCAL_COLLECTION_ID) == []
    store.replace_index_documents([("b", 1), ("a", 2)], "fake", collection_id=LOCAL_COLLECTION_ID)
    assert store.index_documents(collection_id=LOCAL_COLLECTION_ID) == [(0, "b", 1), (1, "a", 2)]
    assert store.index_encoder(collection_id=LOCAL_COLLECTION_ID) == "fake"
    store.replace_index_documents([], None, collection_id=LOCAL_COLLECTION_ID)
    assert store.index_documents(collection_id=LOCAL_COLLECTION_ID) == []
    assert store.index_encoder(collection_id=LOCAL_COLLECTION_ID) is None
