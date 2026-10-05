//! The examination, in temporary directories with injected batches (write
//! design §11): every rule of §3.4, save-by-rename as real editors do it,
//! copies that kept their attributes, the ignore list and refused names.
//! The folder is placed by the real materializer, which records each item's
//! handle; "is this object alive?" is the fake.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, XATTR_ITEM_ID, XATTR_ROOT};

use super::handles::record_replaced;
use super::liveness::{Liveness, NoLiveness};
use super::testing::{file, folder, names, row, Folder, TIME};
use super::*;
use konedrive_tree::outbox::{OutboxKind, OutboxState};
use konedrive_tree::{Change, Kind, Placement};

use OutboxKind::{Create, Delete, Mkdir, Move, MoveOut, Update};

mod acts;
mod copies;
mod identity;
mod liveness;
mod passed_over;
mod since_listed;

fn id_of(path: &Path) -> Option<String> {
    xattr::get(path, XATTR_ITEM_ID).unwrap().map(|v| String::from_utf8(v).unwrap())
}

/// Copies a file with every user attribute, as `cp -a` and KIO do.
fn copy_keeping_attributes(from: &Path, to: &Path) {
    std::fs::copy(from, to).unwrap();
    for name in xattr::list(from).unwrap() {
        if let Some(value) = xattr::get(from, &name).unwrap() {
            xattr::set(to, &name, &value).unwrap();
        }
    }
}

/// Rules 1–5: new directories and files become rows, parents first; what
/// cannot be uploaded is listed, what is ignored stays local, a name
/// OneDrive refuses is blocked.
#[test]
fn new_things_become_rows_and_what_cannot_be_uploaded_is_listed() {
    let fx = Folder::new(&[]);
    std::fs::create_dir_all(fx.path("new/sub")).unwrap();
    fx.write("new/sub/f.txt", b"x");
    fx.write("new/g.txt", b"y");
    fx.write("notes.txt.swp", b"swap");
    fx.write("a:b.txt", b"z");
    fx.write(".konedrive-mine", b"mine");
    std::os::unix::fs::symlink("new", fx.path("link")).unwrap();
    std::os::unix::fs::symlink("user@host.1234:1700000000", fx.path(".#notes.org")).unwrap();
    nix::unistd::mkfifo(&fx.path("pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();
    let batch = names(&[("", "new"), ("", "notes.txt.swp"), ("", "a:b.txt"), ("", ".konedrive-mine"), ("", "link"), ("", "pipe"), ("", ".#notes.org")]);
    fx.examine(&batch);

    let mut rows = fx.summary();
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        rows,
        vec![
            (Create, "a:b.txt".into(), None),
            (Mkdir, "new".into(), None),
            (Create, "new/g.txt".into(), None),
            (Mkdir, "new/sub".into(), None),
            (Create, "new/sub/f.txt".into(), None),
        ]
    );
    let blocked = fx.row_at("a:b.txt");
    assert_eq!((blocked.state, blocked.reason_text().as_deref()), (OutboxState::Blocked, Some("name-characters")));
    assert_eq!(fx.row_at("new").target_parent.as_deref(), Some("R"));
    assert_eq!(fx.row_at("new/sub").target_parent, None, "its parent's id comes when its mkdir lands");
    let (mkdir, create) = (fx.row_at("new/sub").seq, fx.row_at("new/sub/f.txt").seq);
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(create)).unwrap(), vec![mkdir]);
    assert!(fx.row_at("new").seq < mkdir, "new folders shallowest first");

    let skipped = fx.skipped();
    assert_eq!(
        skipped,
        vec![(".konedrive-mine".into(), "reserved-name".into()), ("link".into(), "symlink".into()), ("pipe".into(), "fifo".into())]
    );
    // Listed while it is there.
    std::fs::remove_file(fx.path("pipe")).unwrap();
    fx.examine(&names(&[("", "pipe")]));
    assert_eq!(fx.skipped().len(), 2);
}

/// The content check: a size change is a change; a same-size change is
/// confirmed by hash; a `touch` uploads nothing and refreshes the stamp; an
/// edit that kept size and time is found only through `FAN_CLOSE_WRITE`.
#[test]
fn an_edit_is_an_update_and_a_touch_uploads_nothing() {
    let fx = Folder::new(&[file("A", "R", "a.txt", b"hello"), file("B", "R", "b.txt", b"12345"), file("C", "R", "c.txt", b"old")]);
    for (rel, content) in [("a.txt", &b"hello"[..]), ("b.txt", b"12345"), ("c.txt", b"old")] {
        fx.hydrate(rel, content);
    }
    // A touch.
    File::options().write(true).open(fx.path("a.txt")).unwrap().set_modified(SystemTime::now()).unwrap();
    fx.examine(&names(&[("", "a.txt")]));
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());
    assert!(placeholder::stamp_matches(&File::open(fx.path("a.txt")).unwrap()).unwrap(), "the stamp is refreshed");

    // The same size, other content.
    fx.write("a.txt", b"HELLO");
    fx.examine(&names(&[("", "a.txt")]));
    let row = fx.row_at("a.txt");
    assert_eq!((row.kind, row.item_id.as_deref(), row.state), (Update, Some("A"), OutboxState::Ready));
    let base = row.base.unwrap();
    assert_eq!((base.etag.as_deref(), base.name.as_deref()), (Some("e-A"), Some("a.txt")));
    assert_eq!((row.target_parent.as_deref(), row.target_name.as_deref()), (Some("R"), Some("a.txt")));

    // Size and time kept: no event, nothing seen; `FAN_CLOSE_WRITE`, found.
    let stamp = placeholder::read_stamp(&File::open(fx.path("b.txt")).unwrap()).unwrap().unwrap();
    fx.write("b.txt", b"54321");
    let kept = SystemTime::UNIX_EPOCH + Duration::new(stamp.mtime_sec as u64, stamp.mtime_nsec as u32);
    File::options().write(true).open(fx.path("b.txt")).unwrap().set_modified(kept).unwrap();
    fx.examine(&names(&[("", "b.txt")]));
    assert!(fx.rows().iter().all(|r| r.item_id.as_deref() != Some("B")));
    let mut written = Batch::new();
    written.written(Path::new(""), OsStr::new("b.txt"), Some(fx.handle("b.txt")));
    fx.examine(&written);
    assert_eq!(fx.row_at("b.txt").kind, Update);

    // Another size: no hash needed.
    fx.write("c.txt", b"longer now");
    fx.examine(&names(&[("", "c.txt")]));
    assert_eq!(fx.row_at("c.txt").kind, Update);
    assert_eq!(fx.rows().len(), 3);
}

/// A file someone has open for writing is never quiet: its row waits, and
/// is ready once the writer closes (the read-lease probe).
#[test]
fn a_file_open_for_writing_waits_for_its_writer() {
    let fx = Folder::new(&[file("A", "R", "a.txt", b"log")]);
    fx.hydrate("a.txt", b"log");
    let mut writer = File::options().append(true).open(fx.path("a.txt")).unwrap();
    writer.write_all(b" line").unwrap();
    fx.write("new.txt", b"n");
    let mut new_writer = File::options().append(true).open(fx.path("new.txt")).unwrap();
    new_writer.write_all(b"more").unwrap();
    let out = fx.examine(&names(&[("", "a.txt"), ("", "new.txt")]));
    for rel in ["a.txt", "new.txt"] {
        let row = fx.row_at(rel);
        assert_eq!((row.state, row.reason_text().as_deref(), row.next_try), (OutboxState::Waiting, Some("open-for-writing"), Some(1030)), "{rel}");
    }
    assert!(!out.recheck.is_empty());
    assert!(fx.store.call_blocking(move |s| s.outbox_runnable(i64::MAX)).unwrap().is_empty());

    drop(writer);
    drop(new_writer);
    fx.examine(&out.recheck);
    assert_eq!(fx.rows().iter().map(|r| (r.kind, r.state)).collect::<Vec<_>>(), vec![(Update, OutboxState::Ready), (Create, OutboxState::Ready)]);
}

/// A `truncate(2)` of a file that is not downloaded opens nothing, so
/// nothing was filled: the cloud's size and time come back, nothing is
/// uploaded, and the file stays a placeholder.
#[test]
fn a_truncated_placeholder_gets_the_clouds_size_back() {
    let fx = Folder::new(&[file("P", "R", "p.bin", &[7u8; 100])]);
    nix::unistd::truncate(&fx.path("p.bin"), 10).unwrap();
    let out = fx.examine(&names(&[("", "p.bin")]));
    assert_eq!(out.restored, vec![PathBuf::from("p.bin")]);
    let meta = std::fs::symlink_metadata(fx.path("p.bin")).unwrap();
    assert_eq!((meta.len(), meta.mtime()), (100, TIME));
    assert!(fx.rows().is_empty());
    let state = xattr::get(fx.path("p.bin"), placeholder::XATTR_STATE).unwrap().unwrap();
    assert_eq!(state, b"online-only");
}

/// A rename or a move is a move row of the same item; moving it back
/// removes the row. A directory that moves takes the rows inside it along.
#[test]
fn renames_and_moves_are_moves_of_the_item() {
    let fx = Folder::new(&[folder("D", "R", "docs"), file("A", "R", "a.txt", b"a"), file("F", "D", "f.txt", b"f")]);
    fx.rename("a.txt", "b.txt");
    fx.examine(&names(&[("", "a.txt"), ("", "b.txt")]));
    let row = fx.row_at("b.txt");
    assert_eq!((row.kind, row.item_id.as_deref(), row.target_parent.as_deref(), row.target_name.as_deref()), (Move, Some("A"), Some("R"), Some("b.txt")));
    let seq = row.seq;

    fx.rename("b.txt", "docs/b.txt");
    fx.examine(&names(&[("", "b.txt"), ("docs", "b.txt")]));
    let row = fx.row_at("docs/b.txt");
    assert_eq!((row.seq, row.target_parent.as_deref()), (seq, Some("D")), "one row, to the final place");

    fx.rename("docs/b.txt", "a.txt");
    fx.examine(&names(&[("docs", "b.txt"), ("", "a.txt")]));
    assert!(fx.rows().is_empty(), "back where the base has it");

    // A new file in a folder, then the folder renamed.
    fx.write("docs/new.txt", b"n");
    fx.examine(&names(&[("docs", "new.txt")]));
    fx.rename("docs", "papers");
    fx.examine(&names(&[("", "docs"), ("", "papers")]));
    let mut rows = fx.summary();
    rows.sort();
    assert_eq!(rows, vec![(Create, "papers/new.txt".into(), None), (Move, "papers".into(), Some("D".into()))]);

    // Renamed to a name OneDrive refuses: blocked, for the user to rename.
    fx.rename("a.txt", "what?.txt");
    fx.examine(&names(&[("", "a.txt"), ("", "what?.txt")]));
    let row = fx.row_at("what?.txt");
    assert_eq!((row.kind, row.state, row.reason_text().as_deref()), (Move, OutboxState::Blocked, Some("name-characters")));
}

/// Save-by-rename, as editors do it: the item keeps its id, its version
/// history and its links; the new inode takes it over. Nothing is created,
/// deleted or moved, and nobody is asked whether the old inode lives.
#[test]
fn save_by_rename_in_editors_patterns_is_an_update_of_the_item() {
    let files: [(&str, &str); 6] = [("K", "k.txt"), ("G", "g.txt"), ("L", "lo.odt"), ("V1", "v1.txt"), ("V2", "v2.txt"), ("V3", "v3.txt")];
    let changes: Vec<Change> = files.iter().map(|(id, name)| file(id, "R", name, b"old")).collect();
    let fx = Folder::new(&changes);
    for (_, name) in files {
        fx.hydrate(name, b"old");
    }
    let root = File::open(&fx.root.path).unwrap();

    // GNOME (GIO): `.goutputstream-XXXXXX`, renamed over.
    fx.write(".goutputstream-ABC123", b"gnome's new");
    fx.rename(".goutputstream-ABC123", "g.txt");
    fx.examine(&names(&[("", ".goutputstream-ABC123"), ("", "g.txt")]));

    // LibreOffice: a `lu*.tmp` renamed over, its lock file beside.
    fx.write(".~lock.lo.odt#", b"lock");
    fx.write("lu98765xyz.tmp", b"libreoffice's new");
    fx.rename("lu98765xyz.tmp", "lo.odt");
    fx.examine(&names(&[("", ".~lock.lo.odt#"), ("", "lu98765xyz.tmp"), ("", "lo.odt")]));

    // vim, `backupcopy=no` and `writebackup`: the original becomes `file~`,
    // the new file is written, the backup removed.
    fx.rename("v1.txt", "v1.txt~");
    fx.write("v1.txt", b"vim's new");
    std::fs::remove_file(fx.path("v1.txt~")).unwrap();
    fx.examine(&names(&[("", "v1.txt"), ("", "v1.txt~")]));

    // vim with `backup` on: `file~` stays, the user's local backup.
    fx.rename("v2.txt", "v2.txt~");
    fx.write("v2.txt", b"vim's newer");
    fx.examine(&names(&[("", "v2.txt"), ("", "v2.txt~")]));
    assert_eq!(id_of(&fx.path("v2.txt~")), None, "the downloaded backup is the user's own file now");

    // vim copying the attributes to the new file (`+xattr`).
    fx.rename("v3.txt", "v3.txt~");
    fx.write("v3.txt", b"vim's newest!");
    copy_keeping_attributes(&fx.path("v3.txt~"), &fx.path("v3.txt"));
    fx.write("v3.txt", b"vim's newest!");
    std::fs::remove_file(fx.path("v3.txt~")).unwrap();
    fx.examine(&names(&[("", "v3.txt"), ("", "v3.txt~")]));

    // Kate (QSaveFile): written through O_TMPFILE, linked under a temporary
    // name, renamed over; the write is reported under `#<inode>` (§17).
    let fd = nix::fcntl::openat(root.as_fd(), ".", nix::fcntl::OFlag::O_TMPFILE | nix::fcntl::OFlag::O_RDWR, nix::sys::stat::Mode::from_bits_truncate(0o644)).unwrap();
    let tmp = File::from(fd);
    (&tmp).write_all(b"kate's new").unwrap();
    let pseudo = format!("#{}", tmp.metadata().unwrap().ino());
    nix::unistd::linkat(tmp.as_fd(), "", root.as_fd(), "k.txt.aBcDeF", nix::fcntl::AtFlags::AT_EMPTY_PATH).unwrap();
    drop(tmp);
    fx.rename("k.txt.aBcDeF", "k.txt");
    fx.examine(&names(&[("", pseudo.as_str()), ("", "k.txt.aBcDeF"), ("", "k.txt")]));

    let mut rows = fx.summary();
    rows.sort();
    let mut expected: Vec<_> = files.iter().map(|(id, name)| (Update, name.to_string(), Some(id.to_string()))).collect();
    expected.sort();
    assert_eq!(rows, expected);
    for (_, name) in files {
        let row = fx.row_at(name);
        assert_eq!(row.inode.and_then(|i| i.handle), Some(fx.handle(name)), "{name}: the new inode takes the item over");
    }
    assert!(fx.skipped().is_empty(), "temporary and lock files stay local, unlisted");
}

/// A copy that kept konedrive's attributes (`cp -a`, KIO) is a new file,
/// and the original stays the item; a copied folder likewise. A second
/// name of a placeholder is marked (`MarkFile`) and listed.
#[test]
fn copies_that_kept_their_attributes_are_new_files() {
    let fx = Folder::new(&[folder("D", "R", "docs"), file("A", "R", "a.txt", b"abc"), file("F", "D", "f.txt", b"ff"), file("P", "R", "p.bin", b"placeholder")]);
    fx.hydrate("a.txt", b"abc");
    fx.hydrate("docs/f.txt", b"ff");
    copy_keeping_attributes(&fx.path("a.txt"), &fx.path("copy.txt"));
    std::fs::create_dir(fx.path("docs2")).unwrap();
    for name in xattr::list(fx.path("docs")).unwrap() {
        xattr::set(fx.path("docs2"), &name, &xattr::get(fx.path("docs"), &name).unwrap().unwrap()).unwrap();
    }
    copy_keeping_attributes(&fx.path("docs/f.txt"), &fx.path("docs2/f.txt"));
    std::fs::hard_link(fx.path("p.bin"), fx.path("p-link.bin")).unwrap();

    let out = fx.examine(&names(&[("", "copy.txt"), ("", "docs2"), ("", "p-link.bin")]));
    let mut rows = fx.summary();
    rows.sort();
    assert_eq!(rows, vec![(Create, "copy.txt".into(), None), (Create, "docs2/f.txt".into(), None), (Mkdir, "docs2".into(), None)]);
    for copy in ["copy.txt", "docs2", "docs2/f.txt"] {
        assert_eq!(id_of(&fx.path(copy)), None, "{copy} is stripped");
    }
    for original in [("a.txt", "A"), ("docs", "D"), ("docs/f.txt", "F")] {
        assert_eq!(id_of(&fx.path(original.0)).as_deref(), Some(original.1), "{} is untouched", original.0);
    }
    assert_eq!(out.mark_files, vec![PathBuf::from("p.bin")]);
    assert_eq!(fx.skipped(), vec![("p-link.bin".into(), "hard-link".to_owned())]);
}

/// Rule 7: a base item missing from the batch is decided by its object:
/// gone is a delete, alive outside the folder a move out, alive inside a
/// move to look for. With no one to ask, nothing is decided; and a new file
/// at a deleted name, a batch later, is an update after all.
#[test]
fn a_missing_item_is_decided_by_its_object() {
    let fx = Folder::new(&[
        folder("D", "R", "docs"),
        file("A", "R", "a.txt", b"a"),
        file("B", "R", "b.txt", b"b"),
        file("C", "R", "c.txt", b"c"),
        file("E", "R", "e.txt", b"e"),
    ]);
    let (b, c) = (fx.handle("b.txt"), fx.handle("c.txt"));
    std::fs::remove_file(fx.path("a.txt")).unwrap();
    std::fs::rename(fx.path("b.txt"), fx.outside.join("b.txt")).unwrap();
    fx.liveness.alive(b.clone(), fx.outside.join("b.txt"));
    fx.rename("c.txt", "docs/c.txt");
    fx.liveness.alive(c, fx.path("docs/c.txt"));
    let out = fx.examine(&names(&[("", "a.txt"), ("", "b.txt"), ("", "c.txt")]));

    assert_eq!(fx.summary(), vec![(Delete, "a.txt".into(), Some("A".into())), (MoveOut, "b.txt".into(), Some("B".into()))]);
    assert_eq!(fx.row_at("b.txt").inode.and_then(|i| i.handle), Some(b));
    // What is elsewhere in the folder is not reported: the next look finds it there.
    assert!(out.undecided.is_empty() && out.unproven.is_empty(), "{:?} {:?}", out.undecided, out.unproven);
    fx.examine(&out.recheck);
    assert_eq!(fx.row_at("docs/c.txt").kind, Move);

    std::fs::remove_file(fx.path("e.txt")).unwrap();
    let out = fx.examine_with(&names(&[("", "e.txt")]), &NoLiveness);
    assert_eq!(out.undecided, vec!["E".to_owned()]);
    assert!(fx.rows().iter().all(|r| r.item_id.as_deref() != Some("E")));
    // Undecided is not forgotten: looked at again, it is decided once it can be.
    fx.examine(&out.recheck);
    assert_eq!(fx.row_at("e.txt").kind, Delete);

    // A new file where the deleted one was.
    fx.write("a.txt", b"again");
    fx.examine(&names(&[("", "a.txt")]));
    let row = fx.row_at("a.txt");
    assert_eq!((row.kind, row.item_id.as_deref()), (Update, Some("A")));
}

/// `rm -rf` of a folder is one delete: the rows inside it go with it, and
/// it waits for what left it first.
#[test]
fn a_folder_delete_is_one_row_that_waits_for_what_left_it() {
    let fx = Folder::new(&[
        folder("D", "R", "docs"),
        file("X", "D", "x.txt", b"x"),
        file("Y", "D", "y.txt", b"y"),
        file("Z", "D", "z.txt", b"z"),
    ]);
    fx.hydrate("docs/y.txt", b"y");
    fx.write("docs/y.txt", b"edited");
    fx.examine(&names(&[("docs", "y.txt")]));
    assert_eq!(fx.row_at("docs/y.txt").kind, Update);

    fx.rename("docs/x.txt", "x.txt");
    std::fs::remove_dir_all(fx.path("docs")).unwrap();
    fx.examine(&names(&[("docs", "x.txt"), ("", "x.txt"), ("docs", "y.txt"), ("docs", "z.txt"), ("", "docs")]));
    assert_eq!(fx.summary(), vec![(Move, "x.txt".into(), Some("X".into())), (Delete, "docs".into(), Some("D".into()))]);
    let (moved, deleted) = (fx.row_at("x.txt").seq, fx.row_at("docs").seq);
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(deleted)).unwrap(), vec![moved]);
}

/// A batch that would remove more than the guard allows is held until the
/// user confirms, a delete and a move out alike; a small one goes, and what
/// was confirmed is not held again.
#[test]
fn the_mass_delete_guard_holds_a_large_delete() {
    let mut changes = vec![folder("BIG", "R", "big"), file("O", "R", "other.txt", b"o")];
    changes.extend((0..60).map(|n| file(&format!("F{n}"), "BIG", &format!("f{n}.txt"), b"f")));
    let fx = Folder::new(&changes);
    std::fs::remove_file(fx.path("other.txt")).unwrap();
    let out = fx.examine(&names(&[("", "other.txt")]));
    assert_eq!((out.held, fx.row_at("other.txt").state), (0, OutboxState::Ready));

    std::fs::remove_dir_all(fx.path("big")).unwrap();
    let out = fx.examine(&names(&[("", "big")]));
    assert_eq!(out.held, 62, "the delete of other.txt still waiting adds up with the folder");
    assert_eq!(fx.row_at("other.txt").state, OutboxState::Held);
    let row = fx.row_at("big");
    assert_eq!((row.kind, row.state, row.reason_text().as_deref()), (Delete, OutboxState::Held, Some("mass-delete")));
    assert!(fx.store.call_blocking(move |s| s.outbox_runnable(i64::MAX)).unwrap().iter().all(|r| r.seq != row.seq));
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_release_held()).unwrap(), 2);
    assert!(fx.store.call_blocking(move |s| s.outbox_runnable(i64::MAX)).unwrap().iter().any(|r| r.seq == row.seq));

    // What the user confirmed is not counted or held again while the worker gets through it.
    fx.write("new.txt", b"n");
    let out = fx.examine(&names(&[("", "new.txt")]));
    assert_eq!(out.held, 0);
    for removed in ["big", "other.txt"] {
        let row = fx.row_at(removed);
        assert_eq!((row.kind, row.state, row.confirmed), (Delete, OutboxState::Ready, true), "{removed}");
    }

    // A folder moved out (the Trash is one) removes as much from OneDrive as a delete, and
    // is held the same way.
    let fx = Folder::new(&changes);
    std::fs::rename(fx.path("big"), fx.outside.join("big")).unwrap();
    fx.liveness.alive_tree(&fx.outside.join("big"));
    let out = fx.examine(&names(&[("", "big")]));
    assert_eq!(out.held, 61);
    let row = fx.row_at("big");
    assert_eq!((row.kind, row.state), (MoveOut, OutboxState::Held));
}

/// Rule 6, an id the base does not know: a downloaded file from elsewhere
/// becomes the user's own and is uploaded; one marked as not downloaded
/// that holds data cannot be read here, and is listed with its attributes
/// left alone, never removed.
#[test]
fn a_file_from_elsewhere_is_uploaded_if_downloaded_and_listed_if_not() {
    let fx = Folder::new(&[]);
    fx.write("foreign.txt", b"content");
    xattr::set(fx.path("foreign.txt"), XATTR_ITEM_ID, b"OTHER").unwrap();
    xattr::set(fx.path("foreign.txt"), placeholder::XATTR_STATE, b"hydrated").unwrap();
    fx.write("ghost.bin", &[7u8; 8192]);
    xattr::set(fx.path("ghost.bin"), XATTR_ITEM_ID, b"GHOST").unwrap();
    xattr::set(fx.path("ghost.bin"), placeholder::XATTR_STATE, b"online-only").unwrap();
    fx.write("linked.txt", b"two names");
    xattr::set(fx.path("linked.txt"), XATTR_ITEM_ID, b"LINKED").unwrap();
    xattr::set(fx.path("linked.txt"), placeholder::XATTR_STATE, b"hydrated").unwrap();
    std::fs::hard_link(fx.path("linked.txt"), fx.outside.join("other-account.txt")).unwrap();
    let out = fx.examine(&names(&[("", "foreign.txt"), ("", "ghost.bin"), ("", "linked.txt")]));
    assert_eq!(fx.summary(), vec![(Create, "foreign.txt".into(), None)]);
    assert_eq!(out.stripped, vec![PathBuf::from("foreign.txt")]);
    assert_eq!(id_of(&fx.path("linked.txt")).as_deref(), Some("LINKED"), "stripping it would strip its other name too");
    assert_eq!(id_of(&fx.path("ghost.bin")).as_deref(), Some("GHOST"));
    let skipped = fx.skipped();
    assert_eq!(skipped, vec![("ghost.bin".into(), "not-downloaded".into()), ("linked.txt".into(), "hard-link".into())]);
}

/// A pending create follows its file: renamed, it is created at the new
/// name; renamed to an ignored name or deleted before it was sent, it
/// leaves nothing; a new folder removed takes its rows along.
#[test]
fn a_pending_create_follows_its_file_until_it_is_gone() {
    let fx = Folder::new(&[]);
    fx.write("n.txt", b"n");
    fx.examine(&names(&[("", "n.txt")]));
    let seq = fx.row_at("n.txt").seq;
    fx.rename("n.txt", "m.txt");
    fx.examine(&names(&[("", "n.txt"), ("", "m.txt")]));
    assert_eq!((fx.row_at("m.txt").seq, fx.rows().len()), (seq, 1));
    fx.rename("m.txt", "m.txt.tmp");
    fx.examine(&names(&[("", "m.txt"), ("", "m.txt.tmp")]));
    assert!(fx.rows().is_empty(), "an ignored name stays local");

    fx.write("q.txt", b"q");
    std::fs::create_dir(fx.path("dir")).unwrap();
    fx.write("dir/f.txt", b"f");
    fx.examine(&names(&[("", "q.txt"), ("", "dir")]));
    assert_eq!(fx.rows().len(), 3);
    std::fs::remove_file(fx.path("q.txt")).unwrap();
    std::fs::remove_dir_all(fx.path("dir")).unwrap();
    let out = fx.examine(&names(&[("", "q.txt"), ("", "dir")]));
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());
    assert_eq!(out.applied.removed.len(), 3);
}

/// the outbox on the bus: a directory the worker is making right now is an item
/// already, whatever its name: ignoring its name keeps the new things inside
/// it waiting — they go up once it is made — and only its name waits behind.
#[test]
fn ignoring_a_directory_being_made_keeps_what_is_inside_it() {
    let mut fx = Folder::new(&[]);
    std::fs::create_dir(fx.path("build")).unwrap();
    fx.write("build/a.o", b"a");
    fx.examine(&names(&[("", "build")]));
    let mkdir = fx.row_at("build");
    assert_eq!((mkdir.kind, fx.row_at("build/a.o").kind), (Mkdir, Create));
    fx.store.call_blocking(move |s| s.outbox_set_state(mkdir.seq, OutboxState::Running, None, None)).unwrap();
    fx.ignore = IgnoreList::new(["build"]);
    fx.examine(&names(&[("", "build"), ("build", "a.o")]));
    let rows = fx.rows();
    assert!(rows.iter().any(|r| r.kind == Create && r.rel == Path::new("build/a.o")), "{:?}", fx.summary());
    assert!(rows.iter().any(|r| r.seq == mkdir.seq && r.state == OutboxState::Running), "{:?}", fx.summary());
}

/// The Full local scan finds what events would have shown: changes made
/// while the daemon was not running. Run twice, it finds the same.
#[test]
fn the_full_scan_finds_what_changed_while_the_daemon_was_down() {
    let fx = Folder::new(&[
        folder("D", "R", "docs"),
        file("A", "R", "a.txt", b"a"),
        file("B", "R", "b.txt", b"b"),
        file("C", "D", "c.txt", b"c"),
        file("E", "R", "e.txt", b"e"),
    ]);
    fx.hydrate("a.txt", b"a");
    fx.write("n.txt", b"n");
    fx.write("a.txt", b"a, edited");
    fx.rename("b.txt", "docs/b.txt");
    std::fs::remove_file(fx.path("e.txt")).unwrap();
    std::fs::create_dir(fx.path("new")).unwrap();
    fx.write("new/m.txt", b"m");

    let expected = vec![
        (Mkdir, "new".into(), None),
        (Create, "n.txt".into(), None),
        (Update, "a.txt".into(), Some("A".into())),
        (Move, "docs/b.txt".into(), Some("B".into())),
        (Create, "new/m.txt".into(), None),
        (Delete, "e.txt".into(), Some("E".into())),
    ];
    fx.examine(&Batch::full());
    let mut first = fx.summary();
    let mut want = expected.clone();
    first.sort();
    want.sort();
    assert_eq!(first, want);
    fx.examine(&Batch::full());
    let mut second = fx.summary();
    second.sort();
    assert_eq!(second, want, "nothing new the second time");
    assert!(fx.row_at("new").seq < fx.row_at("new/m.txt").seq, "a new folder before what is in it");
}

/// Across two batches: a placeholder dragged out of a folder is a
/// move-out waiting for its download; the folder deleted a minute later
/// keeps it, and its delete waits for it (WR5).
#[test]
fn a_placeholder_dragged_out_before_its_folder_was_deleted_is_not_deleted_with_it() {
    let fx = Folder::new(&[folder("D", "R", "Docs"), file("P", "D", "p.bin", b"only in the cloud"), file("Q", "D", "q.txt", b"q")]);
    let p = fx.handle("Docs/p.bin");
    std::fs::rename(fx.path("Docs/p.bin"), fx.outside.join("p.bin")).unwrap();
    fx.liveness.alive(p.clone(), fx.outside.join("p.bin"));
    fx.examine(&names(&[("Docs", "p.bin")]));
    assert_eq!(fx.summary(), vec![(MoveOut, "Docs/p.bin".into(), Some("P".into()))]);

    std::fs::remove_dir_all(fx.path("Docs")).unwrap();
    fx.examine(&names(&[("", "Docs")]));
    assert_eq!(fx.summary(), vec![(MoveOut, "Docs/p.bin".into(), Some("P".into())), (Delete, "Docs".into(), Some("D".into()))]);
    let (moved_out, deleted) = (fx.row_at("Docs/p.bin").seq, fx.row_at("Docs").seq);
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(deleted)).unwrap(), vec![moved_out]);
}

/// A placeholder dragged out of a folder and the folder deleted, in one batch: the
/// folder's listing fails, so what left it is asked after — by the objects the batch saw,
/// or, when no event told of the move (a directory the watcher could not mark), every item
/// still with the folder is asked where it is. Either way the placeholder is a move-out
/// that the folder's delete waits for (WR5).
#[test]
fn a_placeholder_dragged_out_and_its_folder_deleted_in_one_batch_is_a_move_out() {
    for seen in [true, false] {
        let fx = Folder::new(&[folder("D", "R", "Docs"), file("P", "D", "p.bin", b"only in the cloud"), file("Q", "D", "q.txt", b"q")]);
        let p = fx.handle("Docs/p.bin");
        std::fs::rename(fx.path("Docs/p.bin"), fx.outside.join("p.bin")).unwrap();
        fx.liveness.alive(p.clone(), fx.outside.join("p.bin"));
        std::fs::remove_dir_all(fx.path("Docs")).unwrap();
        let mut batch = names(&[("", "Docs")]);
        if seen {
            batch.name(Path::new("Docs"), OsStr::new("p.bin"));
            batch.name(Path::new("Docs"), OsStr::new("q.txt"));
            batch.object(p.clone());
        }
        fx.examine(&batch);
        let mut rows = fx.summary();
        rows.sort();
        assert_eq!(rows, vec![(Delete, "Docs".into(), Some("D".into())), (MoveOut, "Docs/p.bin".into(), Some("P".into()))], "seen: {seen}");
        assert_eq!(fx.row_at("Docs/p.bin").inode.and_then(|i| i.handle), Some(p), "seen: {seen}");
        let (moved_out, deleted) = (seq_of(&fx, MoveOut, "Docs/p.bin"), seq_of(&fx, Delete, "Docs"));
        assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(deleted)).unwrap(), vec![moved_out], "seen: {seen}");
    }
}

/// Until a listing has completed there is no base. Against a first listing still being
/// placed, a downloaded file of ours would be taken for a stranger, stripped and uploaded
/// again.
#[test]
fn an_unfinished_listing_is_no_base_to_examine_against() {
    let fx = Folder::unfinished(&[file("A", "R", "a.txt", b"abc")]);
    fx.hydrate("a.txt", b"abc");
    let err = fx.try_examine(&fx.disk(), &Batch::full(), &fx.liveness).unwrap_err();
    assert!(matches!(err, ExamineError::NoBase), "{err:?}");
    // A first listing placed page by page, part-way: rows in `items`, and
    // still no base.
    fx.store.call_blocking(move |s| s.commit_page(&[Change::Root(row("R", None, "", Kind::Folder, b""))], "next-2")).unwrap();
    fx.store.call_blocking(move |s| s.set_delta_link("an old link")).unwrap();
    let err = fx.try_examine(&fx.disk(), &Batch::full(), &fx.liveness).unwrap_err();
    assert!(matches!(err, ExamineError::NoBase), "{err:?}");
    assert_eq!(id_of(&fx.path("a.txt")).as_deref(), Some("A"), "nothing stripped");
    assert!(fx.rows().is_empty());
}

/// `rm -rf exports && mkdir exports && cp … exports/`. The old folder's
/// delete comes first, the new folder waits for it and its files for the
/// folder; the delete waits for nothing that is only at the same path.
#[test]
fn a_folder_removed_and_made_again_is_deleted_before_the_new_one_is_made() {
    let fx = Folder::new(&[folder("E", "R", "exports"), file("O", "E", "old.txt", b"o")]);
    std::fs::remove_dir_all(fx.path("exports")).unwrap();
    std::fs::create_dir(fx.path("exports")).unwrap();
    fx.write("exports/new.txt", b"n");
    fx.examine(&names(&[("", "exports")]));
    assert_eq!(
        fx.summary(),
        vec![(Delete, "exports".into(), Some("E".into())), (Mkdir, "exports".into(), None), (Create, "exports/new.txt".into(), None)]
    );
    let rows = fx.rows();
    let (delete, mkdir, create) = (rows[0].seq, rows[1].seq, rows[2].seq);
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(mkdir)).unwrap(), vec![delete]);
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(create)).unwrap(), vec![mkdir]);
    assert!(fx.store.call_blocking(move |s| s.outbox_blockers(delete)).unwrap().is_empty());
}

/// `mv d d.old && mkdir d`: the new folder waits for the old one's move.
#[test]
fn a_folder_made_at_the_name_of_one_renamed_away_waits_for_its_move() {
    let fx = Folder::new(&[folder("D", "R", "d"), file("F", "D", "f.txt", b"f")]);
    fx.rename("d", "d.old");
    std::fs::create_dir(fx.path("d")).unwrap();
    fx.examine(&names(&[("", "d"), ("", "d.old")]));
    assert_eq!(fx.summary(), vec![(Move, "d.old".into(), Some("D".into())), (Mkdir, "d".into(), None)]);
    let (moved, made) = (fx.row_at("d.old").seq, fx.row_at("d").seq);
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(made)).unwrap(), vec![moved]);
}

/// A source that serves one new version, for a real replacement.
struct Served(&'static [u8]);

#[async_trait::async_trait]
impl crate::hydration::source::ContentSource for Served {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<crate::hydration::source::Fetched, crate::hydration::source::SourceError> {
        let mut hash = konedrive_graph::quickxor::QuickXor::new();
        hash.update(self.0);
        Ok(crate::hydration::source::Fetched {
            served_from: from,
            size: self.0.len() as u64,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_500),
            version: Some(crate::hydration::source::Version { ctag: "c2".into(), quick_xor: Some(hash.finish()) }),
            stream: Box::new(std::io::Cursor::new(self.0[from as usize..].to_vec())),
        })
    }
}

/// A replacement makes a new inode, recorded at the swap; a move out
/// afterwards is a move-out, not a delete of the old inode.
#[test]
fn a_replaced_file_moved_out_is_a_move_out() {
    let fx = Folder::new(&[file("A", "R", "a.txt", b"old")]);
    fx.hydrate("a.txt", b"old");
    let disk = fx.disk();
    let replacement = crate::remote::materialize::Replacement { id: "A".into(), rel: "a.txt".into(), ctag: "c2".into(), size: 3 };
    let outcome = fx.runtime.block_on(crate::remote::materialize::replace(&disk, &fx.locks, &Served(b"new"), &replacement));
    assert!(matches!(outcome, crate::remote::materialize::ReplaceOutcome::Replaced), "{outcome:?}");
    record_replaced(&disk, &fx.store, "A", Path::new("a.txt"));
    let now = fx.handle("a.txt");
    assert_eq!(fx.store.call_blocking(move |s| s.local_handle("A")).unwrap(), Some(now.clone()));

    std::fs::rename(fx.path("a.txt"), fx.outside.join("a.txt")).unwrap();
    fx.liveness.alive(now, fx.outside.join("a.txt"));
    fx.examine(&names(&[("", "a.txt")]));
    assert_eq!(fx.summary(), vec![(MoveOut, "a.txt".into(), Some("A".into()))]);
}

/// What the guard counts: removals trickling in over several batches add up, and the
/// guard holds them all once they do; and each item counts once — a folder of 9 items one
/// of which left it first is 9 removals, under the floor of 10.
#[test]
fn removals_add_up_over_batches_and_each_item_counts_once_for_the_guard() {
    let changes: Vec<Change> = (0..62).map(|n| file(&format!("F{n}"), "R", &format!("f{n:02}.txt"), b"f")).collect();
    let fx = Folder::new(&changes);
    let remove = |range: std::ops::Range<usize>| {
        let mut batch = Batch::new();
        for n in range {
            std::fs::remove_file(fx.path(&format!("f{n:02}.txt"))).unwrap();
            batch.name(Path::new(""), OsStr::new(&format!("f{n:02}.txt")));
        }
        fx.examine(&batch)
    };
    assert_eq!(remove(0..12).held, 0, "12 of 62 is under a fifth");
    assert_eq!(remove(12..24).held, 24, "with the 12 still waiting, 24 is not");
    assert!(fx.rows().iter().all(|r| r.state == OutboxState::Held));

    let mut changes = vec![folder("F", "R", "f"), file("O1", "R", "o1.txt", b"o"), file("O2", "R", "o2.txt", b"o")];
    changes.extend((0..8).map(|n| file(&format!("F{n}"), "F", &format!("f{n}.txt"), b"f")));
    let fx = Folder::new(&changes);
    let p = fx.handle("f/f0.txt");
    std::fs::rename(fx.path("f/f0.txt"), fx.outside.join("f0.txt")).unwrap();
    fx.liveness.alive(p, fx.outside.join("f0.txt"));
    assert_eq!(fx.examine(&names(&[("f", "f0.txt")])).held, 0);
    std::fs::remove_dir_all(fx.path("f")).unwrap();
    assert_eq!(fx.examine(&names(&[("", "f")])).held, 0, "the folder and its 8 items, the one that left counted once");
    assert!(fx.rows().iter().all(|r| r.state == OutboxState::Ready));
}

/// A row the worker is sending is never removed: a delete, or the
/// rename to an ignored name, waits behind it.
#[test]
fn a_row_being_sent_is_never_taken_from_under_the_worker() {
    let fx = Folder::new(&[]);
    fx.write("n.txt", b"n");
    fx.write("m.txt", b"m");
    fx.examine(&names(&[("", "n.txt"), ("", "m.txt")]));
    for row in fx.rows() {
        fx.store.call_blocking(move |s| s.outbox_set_state(row.seq, OutboxState::Running, None, None)).unwrap();
    }
    std::fs::remove_file(fx.path("n.txt")).unwrap();
    fx.rename("m.txt", "m.txt.tmp");
    fx.examine(&names(&[("", "n.txt"), ("", "m.txt"), ("", "m.txt.tmp")]));
    let rows = fx.rows();
    assert_eq!(rows.len(), 4, "{:?}", fx.summary());
    let find = |kind: OutboxKind, rel: &str| rows.iter().find(|r| r.kind == kind && r.rel == Path::new(rel)).cloned().unwrap();
    let (n_create, m_create) = (find(Create, "n.txt"), find(Create, "m.txt"));
    let (n_delete, m_move) = (find(Delete, "n.txt"), find(Move, "m.txt.tmp"));
    assert_eq!((n_create.state, m_create.state), (OutboxState::Running, OutboxState::Running));
    assert_eq!((n_delete.state, m_move.state), (OutboxState::Ready, OutboxState::Ready));
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(n_delete.seq)).unwrap(), vec![n_create.seq]);
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(m_move.seq)).unwrap(), vec![m_create.seq]);
}

/// Nothing is examined in a root that was deleted, or that no longer
/// carries its root id.
#[test]
fn nothing_is_examined_in_a_root_that_went_away() {
    let fx = Folder::new(&[file("A", "R", "a.txt", b"a")]);
    let disk = fx.disk();
    xattr::remove(&fx.root.path, XATTR_ROOT).unwrap();
    let err = fx.try_examine(&disk, &Batch::full(), &fx.liveness).unwrap_err();
    assert!(matches!(err, ExamineError::RootGone), "{err:?}");
    std::fs::remove_dir_all(&fx.root.path).unwrap();
    let err = fx.try_examine(&disk, &Batch::full(), &fx.liveness).unwrap_err();
    assert!(matches!(err, ExamineError::RootGone), "{err:?}");
    assert!(fx.rows().is_empty());
}

/// An answer that cannot be placed for sure decides nothing: a path
/// that is not a path, or a place in the folder where the object is not.
#[test]
fn an_answer_that_cannot_be_placed_decides_nothing() {
    let fx = Folder::new(&[folder("D", "R", "docs"), file("B", "R", "b.txt", b"b"), file("C", "R", "c.txt", b"c"), file("X", "D", "x.txt", b"x")]);
    let (b, c) = (fx.handle("b.txt"), fx.handle("c.txt"));
    std::fs::rename(fx.path("b.txt"), fx.outside.join("b.txt")).unwrap();
    fx.liveness.alive(b, "(unreachable)/elsewhere/b.txt");
    std::fs::remove_file(fx.path("c.txt")).unwrap();
    fx.liveness.alive(c, fx.path("docs/x.txt"));
    let out = fx.examine(&names(&[("", "b.txt"), ("", "c.txt")]));
    let mut undecided = out.undecided.clone();
    undecided.sort();
    assert_eq!(undecided, vec!["B".to_owned(), "C".to_owned()]);
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());
}

/// A directory this daemon may not read is passed over and reported;
/// the rest of the folder is examined, and nothing in it counts as missing.
#[test]
fn an_unreadable_directory_does_not_stop_the_examination() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: running as root, which chmod 000 cannot refuse");
        return;
    }
    let fx = Folder::new(&[folder("D", "R", "locked"), file("X", "D", "x.txt", b"x")]);
    fx.write("new.txt", b"n");
    std::fs::set_permissions(fx.path("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let out = fx.examine(&Batch::full());
    std::fs::set_permissions(fx.path("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.unreadable, vec![PathBuf::from("locked")]);
    assert_eq!(fx.summary(), vec![(Create, "new.txt".into(), None)]);
}

fn runnable(fx: &Folder) -> Vec<(OutboxKind, String)> {
    fx.store.call_blocking(move |s| s.outbox_runnable(i64::MAX)).unwrap().into_iter().map(|r| (r.kind, r.rel.display().to_string())).collect()
}

fn seq_of(fx: &Folder, kind: OutboxKind, rel: &str) -> i64 {
    fx.rows().into_iter().find(|r| r.kind == kind && r.rel == Path::new(rel)).unwrap_or_else(|| panic!("no {kind:?} at {rel}: {:?}", fx.summary())).seq
}

/// A `mkdir` from an earlier batch, moved over a folder deleted since,
/// keeps its place in line and still waits for the delete that frees its
/// name — offline, `mkdir exports.new; …; rm -rf exports; mv exports.new
/// exports`.
#[test]
fn a_new_folder_moved_over_one_deleted_since_waits_for_its_delete() {
    let fx = Folder::new(&[folder("E", "R", "exports"), file("O", "E", "old.txt", b"o")]);
    std::fs::create_dir(fx.path("exports.new")).unwrap();
    fx.write("exports.new/a.txt", b"a");
    fx.examine(&names(&[("", "exports.new")]));
    let mkdir = seq_of(&fx, Mkdir, "exports.new");
    std::fs::remove_dir_all(fx.path("exports")).unwrap();
    fx.rename("exports.new", "exports");
    fx.examine(&names(&[("", "exports"), ("", "exports.new")]));
    assert_eq!(seq_of(&fx, Mkdir, "exports"), mkdir, "the merged row keeps its seq");
    let delete = seq_of(&fx, Delete, "exports");
    assert!(delete > mkdir);
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(mkdir)).unwrap(), vec![delete]);
    let create = seq_of(&fx, Create, "exports/a.txt");
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(create)).unwrap(), vec![mkdir]);
}

/// A folder replaced by its own subfolder (`mv F/sub F.tmp && rm -rf F
/// && mv F.tmp F`). The move waits on the delete for the name, the delete
/// on the move for what is inside it: the name's wait is dropped, the
/// move runs (through a temporary name, §4.4) and the delete follows it.
#[test]
fn a_folder_replaced_by_its_own_subfolder_does_not_wait_for_ever() {
    let fx = Folder::new(&[folder("F", "R", "F"), folder("S", "F", "sub"), file("A", "S", "a.txt", b"a"), file("B", "F", "b.txt", b"b")]);
    fx.rename("F/sub", "F.tmp");
    std::fs::remove_dir_all(fx.path("F")).unwrap();
    fx.rename("F.tmp", "F");
    fx.examine(&names(&[("F", "sub"), ("", "F.tmp"), ("F", "b.txt"), ("", "F")]));
    let mut rows = fx.summary();
    rows.sort();
    assert_eq!(rows, vec![(Move, "F".into(), Some("S".into())), (Delete, "F".into(), Some("F".into()))]);
    let (moved, deleted) = (seq_of(&fx, Move, "F"), seq_of(&fx, Delete, "F"));
    assert_eq!(runnable(&fx), vec![(Move, "F".into())]);
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(deleted)).unwrap(), vec![moved], "the folder still goes after what left it");
}

/// A folder wrapped in a new one of its own name (`mkdir t && mv d t/
/// && mv t d`). The move waits for the new folder, the new folder for the
/// name: the name's wait is dropped, and the new folder is made first.
#[test]
fn a_folder_wrapped_in_a_new_one_of_its_name_does_not_wait_for_ever() {
    let fx = Folder::new(&[folder("D", "R", "d"), file("X", "D", "x.txt", b"x")]);
    std::fs::create_dir(fx.path("t")).unwrap();
    fx.rename("d", "t/d");
    fx.rename("t", "d");
    fx.examine(&names(&[("", "t"), ("", "d"), ("t", "d")]));
    let mut rows = fx.summary();
    rows.sort();
    assert_eq!(rows, vec![(Mkdir, "d".into(), None), (Move, "d/d".into(), Some("D".into()))]);
    assert_eq!(runnable(&fx), vec![(Mkdir, "d".into())]);
    let (moved, made) = (seq_of(&fx, Move, "d/d"), seq_of(&fx, Mkdir, "d"));
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(moved)).unwrap(), vec![made]);
}

/// An item of a deleted folder alive elsewhere in the folder, where the
/// batch did not look: the folder waits until it is found, then its delete
/// waits for the item's move.
#[test]
fn a_folder_whose_item_is_elsewhere_in_the_folder_waits_until_it_is_found() {
    let fx = Folder::new(&[folder("D", "R", "docs"), folder("O", "R", "other"), file("X", "D", "x.txt", b"x"), file("Y", "D", "y.txt", b"y")]);
    let x = fx.handle("docs/x.txt");
    fx.rename("docs/x.txt", "other/x.txt");
    fx.liveness.alive(x, fx.path("other/x.txt"));
    std::fs::remove_dir_all(fx.path("docs")).unwrap();
    let out = fx.examine(&names(&[("", "docs")]));
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());
    assert_eq!(out.undecided, vec!["D".to_owned()]);
    fx.examine(&out.recheck);
    let mut rows = fx.summary();
    rows.sort();
    assert_eq!(rows, vec![(Move, "other/x.txt".into(), Some("X".into())), (Delete, "docs".into(), Some("D".into()))]);
    let (moved, deleted) = (seq_of(&fx, Move, "other/x.txt"), seq_of(&fx, Delete, "docs"));
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(deleted)).unwrap(), vec![moved]);
}

/// An item being moved into a folder (its row running) that then left
/// the folder, and the folder deleted: the item gets its own move-out behind
/// the running row, not the folder's delete.
#[test]
fn a_running_move_into_a_folder_that_went_gets_its_own_follow_up() {
    let fx = Folder::new(&[folder("D", "R", "docs"), file("X", "R", "x.txt", b"x")]);
    fx.rename("x.txt", "docs/x.txt");
    fx.examine(&names(&[("", "x.txt"), ("docs", "x.txt")]));
    let running = seq_of(&fx, Move, "docs/x.txt");
    fx.store.call_blocking(move |s| s.outbox_set_state(running, OutboxState::Running, None, None)).unwrap();
    let x = fx.handle("docs/x.txt");
    std::fs::rename(fx.path("docs/x.txt"), fx.outside.join("x.txt")).unwrap();
    fx.liveness.alive(x, fx.outside.join("x.txt"));
    std::fs::remove_dir_all(fx.path("docs")).unwrap();
    fx.examine(&names(&[("", "docs")]));
    let follow_up = seq_of(&fx, MoveOut, "docs/x.txt");
    assert_eq!(fx.store.call_blocking(move |s| s.outbox_blockers(follow_up)).unwrap(), vec![running]);
    assert!(fx.rows().iter().any(|r| r.kind == Delete && r.item_id.as_deref() == Some("D")));
}

/// A subfolder dragged out of a folder, which cannot be removed yet
/// itself (an item in it has no recorded handle), keeps its parent from
/// being deleted too: otherwise the cloud would delete the subfolder with
/// the parent, and its placeholders outside would read zeros (WR5).
#[test]
fn a_subfolder_dragged_out_that_is_held_back_keeps_its_parent_from_being_deleted() {
    let fx = Folder::new(&[
        folder("D", "R", "Docs"),
        folder("S", "D", "Sub"),
        file("A", "S", "a.bin", b"only in the cloud"),
        file("B", "S", "b.bin", b"only in the cloud"),
    ]);
    fx.store.call_blocking(move |s| s.set_local_handle("A", None)).unwrap();
    let (sub, b) = (fx.handle("Docs/Sub"), fx.handle("Docs/Sub/b.bin"));
    std::fs::rename(fx.path("Docs/Sub"), fx.outside.join("Sub")).unwrap();
    fx.liveness.alive(sub, fx.outside.join("Sub"));
    fx.liveness.alive(b, fx.outside.join("Sub/b.bin"));
    std::fs::remove_dir_all(fx.path("Docs")).unwrap();
    let out = fx.examine(&names(&[("", "Docs")]));
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());
    let mut unproven = out.unproven.clone();
    unproven.sort();
    assert_eq!(unproven, vec!["D".to_owned(), "S".to_owned()]);
    // Unproven, not undecided: nothing is asked again every 30 s through the helper; the
    // reconcile places the item again.
    assert!(out.undecided.is_empty() && out.recheck.is_empty(), "{:?} {:?}", out.undecided, out.recheck);
}
