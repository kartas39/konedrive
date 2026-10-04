CREATE TABLE items (
    id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, kind TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0,
    etag TEXT, ctag TEXT, quickxor TEXT, mime TEXT,
    placement TEXT NOT NULL, thumb_key TEXT,
    local_handle BLOB, local_seq INTEGER NOT NULL DEFAULT 0);
CREATE INDEX items_parent ON items(parent_id);
CREATE INDEX items_handle ON items(local_handle);
INSERT INTO items (id, parent_id, name, kind, placement, local_handle) VALUES ('R', NULL, '', 'folder', 'placed', NULL);
INSERT INTO items (id, parent_id, name, kind, placement, local_handle) VALUES ('L', 'R', 'long', 'folder', 'skipped:name-too-long', X'0100000001010101');
INSERT INTO items (id, parent_id, name, kind, placement, local_handle) VALUES ('F', 'L', 'f', 'folder', 'placed', X'0100000002020202');
INSERT INTO items (id, parent_id, name, kind, placement, local_handle) VALUES ('G', 'F', 'g', 'file', 'placed', X'0100000003030303');
INSERT INTO items (id, parent_id, name, kind, placement, local_handle) VALUES ('T', 'R', 't', 'file', 'placed', X'0100000004040404');
CREATE TABLE staging (
    id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, kind TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0,
    etag TEXT, ctag TEXT, quickxor TEXT, mime TEXT,
    placement TEXT NOT NULL, thumb_key TEXT,
    local_handle BLOB, local_seq INTEGER NOT NULL DEFAULT 0);
CREATE INDEX staging_parent ON staging(parent_id);
CREATE INDEX staging_handle ON staging(local_handle);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
INSERT INTO meta (key, value) VALUES ('schema_version', '3');
INSERT INTO meta (key, value) VALUES ('root_item_id', 'R');
INSERT INTO meta (key, value) VALUES ('delta_link', 'link-1');
CREATE TABLE activity (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, kind TEXT NOT NULL, path TEXT NOT NULL, detail TEXT NOT NULL);
CREATE TABLE conflicts (rescued TEXT PRIMARY KEY, at INTEGER NOT NULL, original TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'rescued');
CREATE TABLE outbox (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    item_id TEXT,
    dev INTEGER, ino INTEGER,
    rel TEXT NOT NULL,
    base_etag TEXT, base_ctag TEXT, base_parent TEXT, base_name TEXT,
    target_parent TEXT, target_name TEXT,
    state TEXT NOT NULL,
    reason TEXT, attempts INTEGER NOT NULL DEFAULT 0, next_try INTEGER,
    snapshot TEXT,
    session_url TEXT, session_expires INTEGER, session_next INTEGER,
    handle BLOB,
    confirmed INTEGER NOT NULL DEFAULT 0);
CREATE INDEX outbox_item ON outbox(item_id);
INSERT INTO outbox (seq, kind, rel, target_parent, target_name, state, snapshot, session_url, session_expires, session_next)
    VALUES (1, 'create', 'new.bin', 'R', 'new.bin', 'ready', '5 1700000000000000001', 'https://up.example/old', 2000, 3);
INSERT INTO outbox (seq, kind, rel, target_parent, target_name, state, reason, attempts, next_try)
    VALUES (2, 'create', 'bad.bin', 'R', 'bad.bin', 'retry', 'hash-mismatch:OLD!1', 2, 1500);
INSERT INTO outbox (seq, kind, item_id, rel, base_parent, base_name, target_name, state, snapshot)
    VALUES (3, 'move-out', 'T', 't', 'R', 't', '/trash/t', 'ready', 'moved-out:trash');
INSERT INTO outbox (seq, kind, rel, target_parent, target_name, state, snapshot, session_url)
    VALUES (4, 'create', 'odd.bin', 'R', 'odd.bin', 'ready', '12 soon', 'https://up.example/odd');
INSERT INTO outbox (seq, kind, rel, target_parent, target_name, state)
    VALUES (5, 'create', 'a.txt', 'R', 'a.txt', 'ready');
UPDATE sqlite_sequence SET seq = 9 WHERE name = 'outbox';
CREATE TABLE local_skipped (rel TEXT PRIMARY KEY, reason TEXT NOT NULL, at INTEGER NOT NULL);
INSERT INTO local_skipped (rel, reason, at) VALUES ('link', 'symlink', 1300);
CREATE TABLE upload_openings (seq INTEGER PRIMARY KEY, parent TEXT NOT NULL, name TEXT NOT NULL, at INTEGER NOT NULL);
INSERT INTO upload_openings (seq, parent, name, at) VALUES (5, 'R', 'a.txt', 100);
CREATE TRIGGER upload_openings_leave AFTER DELETE ON outbox BEGIN DELETE FROM upload_openings WHERE seq = OLD.seq; END;
