import sqlite3

import pytest

from priorart.store import LOCAL_COLLECTION_ID, LOCAL_PRINCIPAL_ID, RecordNotFound, Store


@pytest.fixture
def scoped_store(tmp_path):
    store = Store(tmp_path / "priorart.sqlite")
    alice = store.create_principal()
    bob = store.create_principal()
    account_a = store.create_account(owner_principal_id=alice)
    account_b = store.create_account(owner_principal_id=bob)
    a = store.create_collection(owner_account_id=account_a)
    b = store.create_collection(owner_account_id=account_b)
    yield store, a, b, alice, bob
    store.close()


def put(store, collection, principal, text="text", record="same"):
    return store.put(
        text, {"tag": "shared"}, record, collection_id=collection, author_principal_id=principal
    )


def test_identity_ownership_and_defaults(scoped_store):
    store, a, b, alice, bob = scoped_store
    assert a != b
    assert store.get_collection(a).visibility == "restricted"
    assert store.get_collection(LOCAL_COLLECTION_ID).visibility == "restricted"
    assert store.get_collection(a).owner_account_id != store.get_collection(b).owner_account_id
    first = put(store, a, alice, "alice")
    second = put(store, b, bob, "bob")
    assert (first.collection_id, first.record_id, first.revision) == (a, "same", 1)
    assert second.collection_id == b
    assert store.get("same", None, collection_id=a).text == "alice"
    assert store.get("same", None, collection_id=b).text == "bob"
    # Authorship is independent of collection ownership and does not change on update.
    put(store, a, bob, "updated by another principal")
    assert store.get("same", None, collection_id=a).author_principal_id == alice
    assert store.get("same", None, collection_id=b).revision == 1


def test_visibility_and_ownership_constraints(scoped_store):
    store, a, _, _, _ = scoped_store
    connection = store._connection
    with pytest.raises(sqlite3.IntegrityError):
        connection.execute("UPDATE collections SET visibility = 'public' WHERE id = ?", (a,))
    with pytest.raises(sqlite3.IntegrityError):
        store.create_collection(owner_account_id="missing")
    with pytest.raises(sqlite3.IntegrityError):
        store.create_account(owner_principal_id="missing")
    with pytest.raises(ValueError):
        store.create_collection(
            owner_account_id=store.get_collection(a).owner_account_id, visibility="invalid"
        )
    public = store.create_collection(
        owner_account_id=store.get_collection(a).owner_account_id, visibility="public"
    )
    assert store.get_collection(public).visibility == "public"
    with pytest.raises(sqlite3.IntegrityError):
        connection.execute(
            "UPDATE collections SET visibility = 'restricted' WHERE id = ?", (public,)
        )


def test_scoped_reads_deletes_reports_and_indexes(scoped_store):
    store, a, b, alice, bob = scoped_store
    put(store, a, alice, "alice")
    put(store, b, bob, "bob")
    put(store, b, bob, "bob second revision")
    put(store, b, bob, record="only-b")
    for scope in (a, b):
        store.replace_index_documents([("same", 1)], scope, collection_id=scope)
        store.add_report("same", 1, None, scope, collection_id=scope, reporter_principal_id=alice)
    with pytest.raises(RecordNotFound):
        store.get("only-b", None, collection_id=a)
    assert [item.record_id for item in store.live_documents(collection_id=a)] == ["same"]
    assert store.matching_record_ids({"tag": "shared"}, collection_id=a) == {"same"}
    assert store.reports_for("same", collection_id=a)[0].collection_id == a
    assert store.reports_for("same", collection_id=a)[0].text == a
    store.delete("same", collection_id=a)
    store.replace_index_documents([], None, collection_id=a)
    assert store.live_documents(collection_id=a) == []
    assert store.get("same", None, collection_id=b).text == "bob second revision"
    assert store.index_documents(collection_id=b) == [(0, "same", 1)]
    assert store.index_encoder(collection_id=b) == b
    assert store.index_encoder(collection_id=a) is None
    with pytest.raises(RecordNotFound):
        store.add_report(
            "same", 2, None, "wrong revision", collection_id=a, reporter_principal_id=alice
        )


def test_cross_collection_references_fail_in_database(scoped_store):
    store, a, b, alice, bob = scoped_store
    put(store, a, alice)
    put(store, b, bob)
    put(store, b, bob)  # Revision 2 exists only in B.
    put(store, b, bob, record="only-b")
    search = store.log_search("q", None, [], {}, collection_id=b, requester_principal_id=bob)
    connection = store._connection
    with pytest.raises(sqlite3.IntegrityError):
        connection.execute(
            "INSERT INTO revisions (collection_id, record_id, revision, text_sha256, created_at)"
            " VALUES (?, 'only-b', 1, 'hash', 'now')",
            (a,),
        )
    with pytest.raises(sqlite3.IntegrityError):
        connection.execute(
            "INSERT INTO reports (collection_id, id, record_id, revision, text, created_at)"
            " VALUES (?, 'bad-revision', 'same', 2, 'text', 'now')",
            (a,),
        )
    with pytest.raises(sqlite3.IntegrityError):
        connection.execute(
            "INSERT INTO reports (collection_id, id, record_id, search_id, text, created_at)"
            " VALUES (?, 'bad-search', 'same', ?, 'text', 'now')",
            (a, search),
        )
    store.replace_index_documents([("same", 1)], "old", collection_id=a)
    with pytest.raises(sqlite3.IntegrityError):
        store.replace_index_documents([("same", 2)], None, collection_id=a)
    assert store.index_documents(collection_id=a) == [(0, "same", 1)]
    assert store.index_encoder(collection_id=a) == "old"
    with pytest.raises(sqlite3.IntegrityError):
        store.log_search(
            "q",
            None,
            [{"collection_id": a, "id": "same", "revision": 2, "score": 1.0}],
            {},
            collection_id=a,
            requester_principal_id=alice,
        )
    assert connection.execute(
        "SELECT count(*) FROM searches WHERE collection_id = ?", (a,)
    ).fetchone() == (0,)
    assert not store.has_search(search, collection_id=a)
    with pytest.raises(sqlite3.IntegrityError):
        connection.execute("INSERT INTO search_hits VALUES (?, ?, 0, 'same', 1, 1.0)", (a, search))
    with pytest.raises(ValueError):
        store.log_search(
            "q",
            None,
            [{"collection_id": b, "id": "same", "revision": 1, "score": 1.0}],
            {},
            collection_id=a,
            requester_principal_id=alice,
        )


def test_scope_is_required(scoped_store):
    store, _, _, _, _ = scoped_store
    with pytest.raises(TypeError):
        store.live_documents()
    with pytest.raises(TypeError):
        store.get("same", None)


def test_index_rejects_nonlocal_scope(scoped_store, tmp_path):
    from priorart.index import Index

    store, a, _, _, _ = scoped_store
    with pytest.raises(ValueError, match="only the local collection"):
        Index(tmp_path, store, None, collection_id=a)


def test_local_service_never_reads_other_collections(tmp_path):
    from priorart.config import Settings
    from priorart.service import Service

    service = Service(Settings(data_dir=tmp_path), None)
    store = service.store
    account = store.create_account(owner_principal_id=LOCAL_PRINCIPAL_ID)
    other = store.create_collection(owner_account_id=account)
    put(store, other, LOCAL_PRINCIPAL_ID, "private sentinel", "hidden")
    service.put("local searchable sentinel", None, "local-record")
    service.index.rebuild()
    assert service.health()["document_count"] == 1
    assert [h.id for h in service.search("sentinel").hits] == ["local-record"]
    assert service.get("local-record").collection_id == LOCAL_COLLECTION_ID
    with pytest.raises(RecordNotFound):
        service.get("hidden")
    service.close()
