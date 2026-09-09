import pytest

from priorart.store import RecordDeleted, RecordNotFound, Store


@pytest.fixture
def store(tmp_path):
    store = Store(tmp_path / "priorart.sqlite")
    yield store
    store.close()


def test_put_creates_and_get_returns_latest(store):
    record_id, revision = store.put("first", {"lang": "python"}, None)
    assert revision == 1
    got = store.get(record_id, None)
    assert got.text == "first"
    assert got.metadata == {"lang": "python"}
    assert got.revision == 1
    assert got.text_sha256
    assert got.created_at.endswith("Z")


def test_second_put_appends_revision(store):
    record_id, _ = store.put("first", None, None)
    same_id, revision = store.put("second", {"k": 1}, record_id)
    assert same_id == record_id
    assert revision == 2
    assert store.get(record_id, None).text == "second"
    assert store.get(record_id, 1).text == "first"
    with pytest.raises(RecordNotFound):
        store.get(record_id, 3)


def test_client_chosen_id(store):
    record_id, revision = store.put("x", None, "my-id")
    assert (record_id, revision) == ("my-id", 1)


def test_unknown_get_raises(store):
    with pytest.raises(RecordNotFound):
        store.get("nope", None)


def test_delete_nulls_text_and_blocks_reuse(store):
    record_id, _ = store.put("secret", {"a": 1}, None)
    store.put("secret 2", None, record_id)
    assert store.delete(record_id) is True
    assert store.delete(record_id) is False
    with pytest.raises(RecordDeleted):
        store.get(record_id, None)
    with pytest.raises(RecordDeleted):
        store.get(record_id, 1)
    with pytest.raises(RecordDeleted):
        store.put("again", None, record_id)
    with pytest.raises(RecordNotFound):
        store.delete("nope")
    assert store.live_documents() == []
    # Tombstone keeps the digest of what was there.
    rows = store._connection.execute(
        "SELECT text, metadata, text_sha256 FROM revisions WHERE record_id = ?", (record_id,)
    ).fetchall()
    assert len(rows) == 2
    assert all(row[0] is None and row[1] is None and row[2] for row in rows)


def test_live_documents_latest_and_ordered(store):
    store.put("a1", None, "a")
    store.put("b1", None, "b")
    store.put("a2", None, "a")
    store.put("c1", None, "c")
    store.delete("c")
    docs = store.live_documents()
    assert [(d.record_id, d.revision, d.text) for d in docs] == [("a", 2, "a2"), ("b", 1, "b1")]


def test_matching_record_ids(store):
    store.put("x", {"lang": "python", "gpu": True}, "p")
    store.put("y", {"lang": "rust"}, "r")
    store.put("z", {"lang": "python"}, "p")  # revision 2 keeps python
    store.put("w", None, "n")
    assert store.matching_record_ids({"lang": "python"}) == {"p"}
    assert store.matching_record_ids({"lang": "python", "gpu": True}) == set()
    assert store.matching_record_ids({"lang": "rust"}) == {"r"}
    assert store.matching_record_ids({"missing": 1}) == set()


def test_reports_round_trip_including_deleted(store):
    store.put("x", None, "rec")
    search_id = store.log_search("q", None, [{"id": "rec", "revision": 1, "score": 1.0}], {})
    assert store.has_search(search_id)
    assert not store.has_search("nope")
    report_id = store.add_report("rec", 1, search_id, "worked")
    store.delete("rec")
    reports = store.reports_for("rec")
    assert [r.id for r in reports] == [report_id]
    assert reports[0].search_id == search_id
    assert reports[0].revision == 1
    assert reports[0].text == "worked"
    with pytest.raises(RecordNotFound):
        store.add_report("nope", None, None, "x")


def test_index_documents_mirror(store):
    assert store.index_documents() == []
    store.replace_index_documents([("b", 1), ("a", 2)], "fake")
    assert store.index_documents() == [(0, "b", 1), (1, "a", 2)]
    assert store.index_encoder() == "fake"
    store.replace_index_documents([], None)
    assert store.index_documents() == []
    assert store.index_encoder() is None
