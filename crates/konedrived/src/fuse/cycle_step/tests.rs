//! The decisions of the step are tested with their modules; here is the step itself, on the
//! parts of a mount built over a folder that no mount lies over.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::{Change, Kind, NewTree, Row};

use super::uncommitted_cycle::{list_removed_ids, UncommittedCycle};
use super::{CycleOutcome, OneDriveChange};
use crate::fuse::conflict_copies::ConflictCopy;
use crate::fuse::directory_contents::tests::row;
use crate::fuse::inotify_bridge::Announcement;
use crate::fuse::mount::tests::{build_mount, build_mount_with, BuiltMount};
use crate::local::held::Rw;

/// Stages `changes` as a cycle of the delta listing in the store of `built`, and has the
/// step applied to it inside that call of the store, as a cycle does; `rw` is what the
/// outbox holds.
fn apply(built: &BuiltMount, changes: Vec<Change>, rw: Option<Rw>) -> CycleOutcome {
    let cycle_step = Arc::clone(built.mount.cycle_step());
    let applied = built.store.call_blocking(move |store| {
        store.begin_staging(NewTree::Delta)?;
        store.stage(&changes)?;
        cycle_step.apply_staged_cycle(store, rw.as_ref())
    });
    applied.unwrap()
}

/// The OneDrive file `id` as the fixture's drive has it in `parent`, with another content.
fn with_new_content(id: &str, parent: &str, name: &str) -> Change {
    Change::Upsert(Row { ctag: Some("another".into()), size: 7, ..row(id, Some(parent), name, Kind::File) })
}

fn rows_said(built: &BuiltMount) -> Vec<(OutboxKind, PathBuf)> {
    built.store.call_blocking(|store| store.outbox_rows()).unwrap().into_iter().map(|row| (row.kind, row.rel)).collect()
}

/// The OneDrive id `A` in the directory `D`, placed at `rel`.
pub(super) fn placed(kind: Kind, rel: &str) -> Option<(Row, PathBuf)> {
    let rel = PathBuf::from(rel);
    let name = rel.file_name().unwrap().to_string_lossy().into_owned();
    Some((row("A", Some("D"), &name, kind), rel))
}

#[test]
fn the_ids_a_cycle_removed_are_those_that_vanished() {
    let change = |id: &str, old, new| OneDriveChange::of(id.to_owned(), old, new).unwrap();
    let changes = [
        change("gone", placed(Kind::File, "a.txt"), None),
        change("new", None, placed(Kind::File, "b.txt")),
        change("moved", placed(Kind::File, "c.txt"), placed(Kind::File, "d.txt")),
        change("stayed", placed(Kind::File, "e.txt"), placed(Kind::File, "e.txt")),
        change("gone too", placed(Kind::Folder, "docs"), None),
    ];
    assert_eq!(list_removed_ids(&changes), ["gone", "gone too"]);
}

#[test]
fn a_downloaded_copy_whose_content_is_stale_is_removed() {
    let built = build_mount();
    built.download("a.txt", "A", b"old");
    let outcome = apply(&built, vec![with_new_content("A", "R", "a.txt")], None);
    assert!(matches!(&outcome.changes[..], [OneDriveChange::Stayed { id, .. }] if id == "A"));
    assert!(!built.disk.join("a.txt").exists());
}

/// The directories down to the new path are made, each marked with its OneDrive id.
#[test]
fn a_downloaded_copy_moves_with_its_file() {
    let built = build_mount();
    built.download("a.txt", "A", b"content");
    let outcome = apply(&built, vec![Change::Upsert(row("A", Some("D"), "a.txt", Kind::File))], None);
    assert!(matches!(&outcome.changes[..], [OneDriveChange::Moved { id, .. }] if id == "A"));
    assert!(!built.disk.join("a.txt").exists());
    assert_eq!(std::fs::read(built.disk.join("docs/a.txt")).unwrap(), b"content");
    assert_eq!(xattr::get(built.disk.join("docs"), XATTR_ITEM_ID).unwrap().as_deref(), Some(b"D".as_slice()));
}

/// A file a change of the user's waits on is left as it is: its change from OneDrive waits.
#[test]
fn what_the_outbox_holds_is_skipped() {
    let built = build_mount();
    built.download("a.txt", "A", b"mine");
    let rw = Rw { held: HashSet::from(["A".to_owned()]), ..Rw::default() };
    let outcome = apply(&built, vec![with_new_content("A", "R", "a.txt")], Some(rw));
    assert!(outcome.changes.is_empty());
    assert_eq!(std::fs::read(built.disk.join("a.txt")).unwrap(), b"mine");
}

/// `docs` goes only once `docs/deep` went, whatever the order the cycle has them in: a
/// directory that still holds one would stay.
#[test]
fn the_directories_gone_from_onedrive_are_removed_deepest_first() {
    let built = build_mount_with(&[Change::Upsert(row("E", Some("D"), "deep", Kind::Folder))], &[]);
    built.make_marked_directory("docs", "D");
    built.make_marked_directory("docs/deep", "E");
    let outcome = apply(&built, vec![Change::Delete("D".into()), Change::Delete("C".into()), Change::Delete("E".into())], None);
    assert!(!built.disk.join("docs/deep").exists() && !built.disk.join("docs").exists());
    assert!(outcome.conflicts.kept_directories.is_empty());
}

/// What the user made where a file of OneDrive comes is moved aside, and that is recorded in
/// the outbox inside the cycle's own call of the store.
#[test]
fn what_a_cycle_keeps_of_the_users_own_is_recorded() {
    let built = build_mount();
    std::fs::write(built.disk.join("new.txt"), b"mine").unwrap();
    let outcome = apply(&built, vec![Change::Upsert(row("N", Some("R"), "new.txt", Kind::File))], None);
    assert_eq!(outcome.conflicts.moved_aside, [ConflictCopy { original: "new.txt".into(), copy: "new-fedora.txt".into() }]);
    assert_eq!(std::fs::read(built.disk.join("new-fedora.txt")).unwrap(), b"mine");
    assert_eq!(rows_said(&built), [(OutboxKind::Create, PathBuf::from("new-fedora.txt"))]);
}

/// What a cycle leaves is kept until it is committed, and taken once: the pin of what is
/// gone comes off, and nothing is left for a second commit.
#[test]
fn what_a_cycle_leaves_is_done_once_when_it_is_committed() {
    let built = build_mount_with(&[], &["A"]);
    apply(&built, vec![Change::Delete("A".into())], None);
    let cycle_step = built.mount.cycle_step();
    let vanished = Announcement::NameVanished { rel: "a.txt".into(), kind: Kind::File, id: "A".into(), parent_id: Some("R".into()) };
    assert_eq!(*crate::panic::lock(&cycle_step.uncommitted_cycle), UncommittedCycle { announcements: vec![vanished], removed_ids: vec!["A".to_owned()] });
    assert_eq!(built.mount.files_interface().count_pins(), 1, "not before the cycle is committed");
    cycle_step.announce_committed_cycle();
    assert_eq!(built.mount.files_interface().count_pins(), 0);
    assert_eq!(*crate::panic::lock(&cycle_step.uncommitted_cycle), UncommittedCycle::default());
}

/// A cycle that was applied and never committed is applied again: what it left is not
/// announced twice.
#[test]
fn only_the_last_cycle_applied_leaves_anything() {
    let built = build_mount();
    apply(&built, vec![Change::Delete("A".into())], None);
    apply(&built, vec![Change::Delete("B".into())], None);
    let uncommitted_cycle = crate::panic::lock(&built.mount.cycle_step().uncommitted_cycle);
    assert_eq!(uncommitted_cycle.removed_ids, ["B"]);
    assert!(matches!(&uncommitted_cycle.announcements[..], [Announcement::NameVanished { id, .. }] if id == "B"));
}
