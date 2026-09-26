-- Frozen legacy v1 schema (before schema versioning); independent of migration code.

CREATE TABLE IF NOT EXISTS records (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    deleted_at TEXT
);
CREATE TABLE IF NOT EXISTS revisions (
    record_id TEXT NOT NULL REFERENCES records(id),
    revision INTEGER NOT NULL,
    text TEXT,
    metadata TEXT,
    text_sha256 TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (record_id, revision)
);
CREATE TABLE IF NOT EXISTS reports (
    id TEXT PRIMARY KEY,
    record_id TEXT NOT NULL REFERENCES records(id),
    revision INTEGER,
    search_id TEXT,
    text TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS reports_record ON reports(record_id, created_at);
CREATE TABLE IF NOT EXISTS searches (
    id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    filters TEXT,
    hits TEXT NOT NULL,
    timings TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS index_documents (
    internal_id INTEGER PRIMARY KEY,
    record_id TEXT NOT NULL UNIQUE,
    revision INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS index_state (
    key TEXT PRIMARY KEY,
    value TEXT
);

-- Representative frozen records, revisions, tombstones, feedback, and index state.
INSERT INTO "index_documents" VALUES(0,'other',1);
INSERT INTO "index_documents" VALUES(1,'live',2);
INSERT INTO "index_state" VALUES('encoder','legacy-encoder');
INSERT INTO "records" VALUES('live','2026-09-09T00:00:00.000000Z',NULL);
INSERT INTO "records" VALUES('other','2026-09-09T00:00:00.000000Z',NULL);
INSERT INTO "records" VALUES('deleted','2026-09-09T00:00:00.000000Z','2026-09-09T01:00:00.000000Z');
INSERT INTO "reports" VALUES('report-live','live',1,'search-v1','Worked on the old revision','2026-09-09T00:00:00.000000Z');
INSERT INTO "reports" VALUES('report-deleted','deleted',NULL,NULL,'Unversioned report retained after delete','2026-09-09T00:00:00.000000Z');
INSERT INTO "revisions" VALUES('live',1,'CUDA watchdog timeout','{"lang":"python","nested":{"gpu":true}}','c6c4c2c83949cc3b94ab47d3dd07025f409998d4861229f55a813ff1cd027e5e','2026-09-09T00:00:00.000000Z');
INSERT INTO "revisions" VALUES('live',2,'CUDA watchdog fixed — café','{"attempt":2,"lang":"python"}','9bc21fb9aa57706262c2cacef6d66fcf118cb3701febd1511b918cb58be4b4c2','2026-09-09T00:00:00.000000Z');
INSERT INTO "revisions" VALUES('other',1,'A second live record',NULL,'8903ed5a47c013cfcbccb4e5ba2e65fe9d0d3e8e26e3954d5ed52aef5c8a1da7','2026-09-09T00:00:00.000000Z');
INSERT INTO "revisions" VALUES('deleted',1,NULL,NULL,'100d76d2bfe20acd52c35208fc5f62164045bd20ccb3f436ec41b0929fb445ce','2026-09-09T00:00:00.000000Z');
INSERT INTO "revisions" VALUES('deleted',2,NULL,NULL,'c72d4ec26905f09050a25a0dfbf059f8f513ce502219ad99af16d2d47f860744','2026-09-09T00:00:00.000000Z');
INSERT INTO "searches" VALUES('search-v1','CUDA timeout','{"lang":"python"}','[{"id":"live","revision":1,"score":2.5}]','{"total_seconds":0.01}','2026-09-09T00:00:00.000000Z');
