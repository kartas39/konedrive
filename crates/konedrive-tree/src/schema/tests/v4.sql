CREATE TABLE items (
                        id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, kind TEXT NOT NULL,
                        size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0,
                        etag TEXT, ctag TEXT, quickxor TEXT, mime TEXT,
                        placement TEXT NOT NULL, thumb_key TEXT,
                        local_handle BLOB, local_seq INTEGER NOT NULL DEFAULT 0);
INSERT INTO items (id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, thumb_key, local_handle, local_seq) VALUES ('R', NULL, '', 'folder', 0, 0, NULL, NULL, NULL, NULL, 'placed', NULL, NULL, 0);
INSERT INTO items (id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, thumb_key, local_handle, local_seq) VALUES ('D', 'R', 'd', 'folder', 0, 0, NULL, NULL, NULL, NULL, 'placed', NULL, X'0100000001010101', 0);
INSERT INTO items (id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, thumb_key, local_handle, local_seq) VALUES ('A', 'D', 'a.txt', 'file', 1, 0, NULL, 'c-A', NULL, NULL, 'placed', NULL, X'0100000002020202', 0);
INSERT INTO items (id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, thumb_key, local_handle, local_seq) VALUES ('B', 'R', 'b.txt', 'file', 1, 0, NULL, 'c-B', NULL, NULL, 'placed', NULL, X'0100000003030303', 0);
INSERT INTO items (id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, thumb_key, local_handle, local_seq) VALUES ('M', 'R', 'm.txt', 'file', 1, 0, NULL, 'c-M', NULL, NULL, 'placed', NULL, X'0100000004040404', 0);
INSERT INTO items (id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, thumb_key, local_handle, local_seq) VALUES ('L', 'R', 'long', 'folder', 0, 0, NULL, NULL, NULL, NULL, 'skipped:name-too-long', NULL, NULL, 0);
INSERT INTO items (id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, thumb_key, local_handle, local_seq) VALUES ('I', 'L', 'inside.txt', 'file', 1, 0, NULL, 'c-I', NULL, NULL, 'placed', NULL, NULL, 0);
CREATE TABLE staging (
                        id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, kind TEXT NOT NULL,
                        size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0,
                        etag TEXT, ctag TEXT, quickxor TEXT, mime TEXT,
                        placement TEXT NOT NULL, thumb_key TEXT,
                        local_handle BLOB, local_seq INTEGER NOT NULL DEFAULT 0);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
INSERT INTO meta (key, value) VALUES ('schema_version', '4');
INSERT INTO meta (key, value) VALUES ('root_item_id', 'R');
INSERT INTO meta (key, value) VALUES ('delta_link', 'link-1');
INSERT INTO meta (key, value) VALUES ('outbox_seq', '12');
CREATE TABLE activity (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, kind TEXT NOT NULL,
                                        path TEXT NOT NULL, detail TEXT NOT NULL);
INSERT INTO activity (id, at, kind, path, detail) VALUES (1, 900, 'uploaded', '/f/b.txt', '');
CREATE TABLE conflicts (rescued TEXT PRIMARY KEY, at INTEGER NOT NULL, original TEXT NOT NULL,
                                         kind TEXT NOT NULL DEFAULT 'rescued');
INSERT INTO conflicts (rescued, at, original, kind) VALUES ('/f/b (conflict).txt', 950, '/f/b.txt', 'copy');
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
        confirmed INTEGER NOT NULL DEFAULT 0, size INTEGER, bad_item TEXT, bad_item_ctag TEXT, bad_item_etag TEXT);
INSERT INTO outbox (seq, kind, item_id, dev, ino, rel, base_etag, base_ctag, base_parent, base_name, target_parent, target_name, state, reason, attempts, next_try, snapshot, session_url, session_expires, session_next, handle, confirmed, size, bad_item, bad_item_ctag, bad_item_etag) VALUES (1, 'create', NULL, 7, 10, 'd/new.bin', NULL, NULL, NULL, NULL, 'D', 'new.bin', 'ready', NULL, 0, NULL, '1000 1700000000123456789', 'https://up.example/session-1', 2000, 640, X'010000000A0A0A0A', 0, 900, NULL, NULL, NULL);
INSERT INTO outbox (seq, kind, item_id, dev, ino, rel, base_etag, base_ctag, base_parent, base_name, target_parent, target_name, state, reason, attempts, next_try, snapshot, session_url, session_expires, session_next, handle, confirmed, size, bad_item, bad_item_ctag, bad_item_etag) VALUES (2, 'create', NULL, 7, 11, 'opening.txt', NULL, NULL, NULL, NULL, 'R', 'opening.txt', 'ready', NULL, 0, NULL, NULL, NULL, NULL, NULL, X'010000000B0B0B0B', 0, 5, NULL, NULL, NULL);
INSERT INTO outbox (seq, kind, item_id, dev, ino, rel, base_etag, base_ctag, base_parent, base_name, target_parent, target_name, state, reason, attempts, next_try, snapshot, session_url, session_expires, session_next, handle, confirmed, size, bad_item, bad_item_ctag, bad_item_etag) VALUES (3, 'update', 'B', 7, 3, 'b.txt', 'e-B', 'c-B', 'R', 'b.txt', 'R', 'b.txt', 'retry', 'hash-mismatch', 1, 1500, NULL, NULL, NULL, NULL, X'0100000003030303', 0, 42, 'BAD', 'c-BAD', NULL);
INSERT INTO outbox (seq, kind, item_id, dev, ino, rel, base_etag, base_ctag, base_parent, base_name, target_parent, target_name, state, reason, attempts, next_try, snapshot, session_url, session_expires, session_next, handle, confirmed, size, bad_item, bad_item_ctag, bad_item_etag) VALUES (4, 'move-out', 'M', 7, 4, 'm.txt', 'e-M', 'c-M', 'R', 'm.txt', NULL, '/elsewhere/m.txt', 'ready', NULL, 0, NULL, 'moved-out:local', NULL, NULL, NULL, X'0100000004040404', 0, NULL, NULL, NULL, NULL);
INSERT INTO outbox (seq, kind, item_id, dev, ino, rel, base_etag, base_ctag, base_parent, base_name, target_parent, target_name, state, reason, attempts, next_try, snapshot, session_url, session_expires, session_next, handle, confirmed, size, bad_item, bad_item_ctag, bad_item_etag) VALUES (5, 'delete', 'A', 7, 2, 'd/a.txt', 'e-A', 'c-A', 'R', 'a.txt', NULL, NULL, 'held', 'mass-delete', 0, NULL, NULL, NULL, NULL, NULL, X'0100000002020202', 0, NULL, NULL, NULL, NULL);
INSERT INTO outbox (seq, kind, item_id, dev, ino, rel, base_etag, base_ctag, base_parent, base_name, target_parent, target_name, state, reason, attempts, next_try, snapshot, session_url, session_expires, session_next, handle, confirmed, size, bad_item, bad_item_ctag, bad_item_etag) VALUES (7, 'create', NULL, 7, 13, X'636166E92E747874', NULL, NULL, NULL, NULL, 'R', 'x', 'blocked', 'name-not-utf8', 0, NULL, NULL, NULL, NULL, NULL, X'010000000D0D0D0D', 0, 3, NULL, NULL, NULL);
CREATE TABLE local_skipped (rel TEXT PRIMARY KEY, reason TEXT NOT NULL, at INTEGER NOT NULL, size INTEGER);
INSERT INTO local_skipped (rel, reason, at, size) VALUES ('link', 'symlink', 1300, 0);
CREATE TABLE deferred (
        id TEXT PRIMARY KEY, seq INTEGER NOT NULL, gone INTEGER NOT NULL,
        parent_id TEXT, name TEXT, kind TEXT, size INTEGER, mtime INTEGER, etag TEXT, ctag TEXT,
        quickxor TEXT, mime TEXT, placement TEXT);
INSERT INTO deferred (id, seq, gone, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement) VALUES ('DG', 9, 1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
INSERT INTO deferred (id, seq, gone, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement) VALUES ('B', 10, 0, 'R', 'b.txt', 'file', 7, 600, 'e-B2', 'c-B2', NULL, NULL, 'placed');
CREATE TABLE outbox_gone (id TEXT PRIMARY KEY, local_seq INTEGER NOT NULL);
INSERT INTO outbox_gone (id, local_seq) VALUES ('GONE', 11);
CREATE TABLE leaving (id TEXT PRIMARY KEY, rel BLOB NOT NULL, handle BLOB);
INSERT INTO leaving (id, rel, handle) VALUES ('L', X'6C6F6E67', X'0101');
CREATE TABLE leaving_items (id TEXT PRIMARY KEY, leaving TEXT NOT NULL);
INSERT INTO leaving_items (id, leaving) VALUES ('I', 'L');
CREATE TABLE staging_gone (id TEXT PRIMARY KEY);
CREATE TABLE upload_sessions (url TEXT PRIMARY KEY, parent TEXT, name TEXT, opened INTEGER NOT NULL);
INSERT INTO upload_sessions (url, parent, name, opened) VALUES ('https://up.example/session-1', 'D', 'new.bin', 1001);
CREATE TABLE upload_openings (seq INTEGER PRIMARY KEY, parent TEXT NOT NULL, name TEXT NOT NULL, at INTEGER NOT NULL, last INTEGER);
INSERT INTO upload_openings (seq, parent, name, at, last) VALUES (2, 'R', 'opening.txt', 1100, 1100);
CREATE TABLE upload_openings_left (parent TEXT NOT NULL, name TEXT NOT NULL, at INTEGER NOT NULL, last INTEGER NOT NULL, left_at INTEGER NOT NULL);
INSERT INTO upload_openings_left (parent, name, at, last, left_at) VALUES ('R', 'left.txt', 1200, 1200, 1250);
UPDATE sqlite_sequence SET seq = 7 WHERE name = 'outbox';
CREATE INDEX items_parent ON items(parent_id);
CREATE INDEX items_handle ON items(local_handle);
CREATE INDEX staging_parent ON staging(parent_id);
CREATE INDEX staging_handle ON staging(local_handle);
CREATE INDEX outbox_item ON outbox(item_id);
CREATE INDEX items_seq ON items(local_seq);
CREATE INDEX items_unplaced ON items(id) WHERE local_handle IS NULL AND placement = 'placed';
CREATE INDEX items_skipped ON items(id) WHERE placement != 'placed';
CREATE INDEX outbox_object ON outbox(dev, ino);
CREATE INDEX outbox_handle ON outbox(handle);
CREATE INDEX outbox_rel ON outbox(rel);
CREATE INDEX outbox_due ON outbox(state, next_try, seq);
CREATE INDEX outbox_target_parent ON outbox(target_parent);
CREATE INDEX outbox_kind ON outbox(kind);
CREATE INDEX outbox_frees ON outbox(seq) WHERE base_parent IS NOT NULL AND base_name IS NOT NULL AND (base_parent IS NOT target_parent OR base_name IS NOT target_name);
CREATE INDEX upload_openings_parent ON upload_openings(parent);
CREATE INDEX upload_openings_left_parent ON upload_openings_left(parent);
CREATE TRIGGER upload_openings_left_behind AFTER DELETE ON outbox
        BEGIN
            INSERT INTO upload_openings_left (parent, name, at, last, left_at)
                SELECT parent, name, at, COALESCE(last, at), CAST(strftime('%s', 'now') AS INTEGER) FROM upload_openings WHERE seq = OLD.seq;
            DELETE FROM upload_openings WHERE seq = OLD.seq;
        END;
CREATE INDEX upload_sessions_parent ON upload_sessions(parent);
CREATE INDEX outbox_session ON outbox(session_url) WHERE session_url IS NOT NULL;
