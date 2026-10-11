//! A change of the daemon's own that a death cut short, over a backing directory and a
//! store: for each operation that changes a name, the daemon dies before the disk is
//! touched, after it and before the row, and in the middle of a change of several steps.
//! The check finishes or undoes exactly that change from its record, and then finds the
//! folder as the rows say it.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use konedrive_tree::{ContentPresence, ContentVersion, ItemKind, LocalItemId, LocalVersionChange, MadeEntryOwner, PlaceChangeRecord};

use super::unread_parts::is_root;
use super::{assert_no_item_of_onedrive_is_removed_or_moved, changed_here, content, folder_of_each, found, left, reference, set_mtime, DiskFinding, Folder, UnresolvedReason, MTIME};
use crate::folder::disk::ASIDE_ENTRY_PREFIX;

/// The name the entry of `item` is set aside to.
fn aside_name(item: LocalItemId) -> OsString {
    format!("{ASIDE_ENTRY_PREFIX}{item}").into()
}

impl Folder {
    /// What the daemon writes before it changes a name.
    fn record(&mut self, record: &PlaceChangeRecord) {
        self.store.item_store().record_place_change(record).unwrap();
    }

    fn records(&mut self) -> Vec<PlaceChangeRecord> {
        self.store.item_store().list_place_change_records().unwrap().into_iter().map(|(_, record)| record).collect()
    }

    fn read(&self, rel: impl AsRef<Path>) -> Option<Vec<u8>> {
        fs::read(self.disk(rel)).ok()
    }
}

// ---- Make: the record, the entry, the row. ----

fn make_record(f: &mut Folder, rel: &str, kind: ItemKind, replaced: Option<LocalItemId>) -> PlaceChangeRecord {
    PlaceChangeRecord::MakeEntry { kind, place: f.place(Path::new(rel)), owner: MadeEntryOwner::NewItem { replaced } }
}

#[test]
fn a_make_that_died_before_the_disk_leaves_no_row_and_no_record() {
    let mut f = folder_of_each();
    let record = make_record(&mut f, "docs/new.txt", ItemKind::File, None);
    f.record(&record);
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records()), (before, vec![]));
}

#[test]
fn a_make_that_died_after_the_disk_has_its_row_made_as_the_make_would_have() {
    let mut f = folder_of_each();
    for (rel, kind) in [("docs/new.txt", ItemKind::File), ("mine/newdir", ItemKind::Directory), ("a.tmp", ItemKind::File)] {
        let record = make_record(&mut f, rel, kind, None);
        f.record(&record);
    }
    f.write("docs/new.txt", b"");
    f.make_directory("mine/newdir");
    f.write("a.tmp", b"");
    // The rows were made with the records' going: the walk finds them as the disk has them.
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 9)]);
    assert_eq!(f.records(), vec![]);
    let new = f.row("docs/new.txt").unwrap();
    assert_eq!((new.kind, content(&new), new.content_presence, new.onedrive_item.clone()), (ItemKind::File, changed_here(0, 1), ContentPresence::OnDisk, None));
    assert_eq!(f.row("mine/newdir").unwrap().kind, ItemKind::Directory);
    assert!(f.row("a.tmp").unwrap().is_name_ignored, "what the rules say of the name");
    f.assert_settled();
}

#[test]
fn a_file_made_where_one_was_removed_here_takes_its_binding_over_as_the_make_would_have() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    fs::remove_file(f.disk("docs/a.txt")).unwrap();
    f.store.item_store().apply(&LocalVersionChange::Removed { item: a }).unwrap();
    let record = make_record(&mut f, "docs/a.txt", ItemKind::File, Some(a));
    f.record(&record);
    f.write("docs/a.txt", b"");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    let made = f.row("docs/a.txt").unwrap();
    assert_eq!((made.onedrive_item.clone(), content(&made)), (Some(reference("d1", "A")), changed_here(0, 1)), "OneDrive sees one item whose content changed");
    assert_eq!(f.store.item_store().find_item(a).unwrap(), None, "the replaced row went");
    assert_eq!(f.mark_of("docs/a.txt").as_deref(), Some("A"));
    assert_eq!(f.records(), vec![]);
    f.assert_settled();
}

// ---- The backing file of a file that has a row: the record, the row, the file. ----

/// The record and the row of a writer's open of `docs/remote.bin`, which is not on disk,
/// as the mount writes them before it makes the backing file.
fn open_remote_for_writing(f: &mut Folder) -> LocalItemId {
    let remote = f.id("docs/remote.bin");
    let record = PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: f.place(Path::new("docs/remote.bin")), owner: MadeEntryOwner::Item(remote) };
    f.record(&record);
    f.store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: remote }).unwrap();
    f.store.item_store().set_content_presence(remote, ContentPresence::OnDisk).unwrap();
    remote
}

#[test]
fn a_writers_open_that_died_before_its_row_leaves_the_row_as_it_was() {
    let mut f = folder_of_each();
    let remote = f.id("docs/remote.bin");
    let record = PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: f.place(Path::new("docs/remote.bin")), owner: MadeEntryOwner::Item(remote) };
    f.record(&record);
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records()), (before, vec![]));
}

#[test]
fn a_writers_open_that_died_after_its_row_and_before_the_file_has_the_row_put_back() {
    let mut f = folder_of_each();
    let before = f.rows();
    open_remote_for_writing(&mut f);
    assert_ne!(f.rows(), before);
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records()), (before, vec![]), "not on disk, clean, as before the open");
}

#[test]
fn a_writers_open_that_died_after_the_file_was_made_is_closed_as_the_disk_has_it() {
    let mut f = folder_of_each();
    let remote = open_remote_for_writing(&mut f);
    f.write("docs/remote.bin", b"");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    let row = f.item(remote);
    assert_eq!((content(&row), row.content_presence, row.content_before_write.clone()), (changed_here(0, 1), ContentPresence::OnDisk, None));
    assert_eq!(f.records(), vec![]);
    f.assert_settled();
}

// ---- A move to a place nothing is at: the record, the disk, the row. ----

fn move_record(f: &mut Folder, from: &str, to: &str) -> PlaceChangeRecord {
    PlaceChangeRecord::MoveEntry { item: f.id(from), from: f.place(Path::new(from)), to: f.place(Path::new(to)) }
}

#[test]
fn a_move_that_died_before_the_disk_leaves_everything_as_it_was() {
    let mut f = folder_of_each();
    let record = move_record(&mut f, "docs/a.txt", "mine/a.txt");
    f.record(&record);
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records()), (before, vec![]));
}

#[test]
fn a_move_that_died_after_the_disk_is_moved_back_and_no_row_changes() {
    for (from, to) in [("docs/a.txt", "mine/a.txt"), ("top.txt", "docs/renamed.txt"), ("docs", "mine/docs"), ("mine", "docs/mine")] {
        let mut f = folder_of_each();
        let record = move_record(&mut f, from, to);
        f.record(&record);
        f.rename(from, to);
        let before = f.rows();
        assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)], "{from} -> {to}");
        assert_eq!((f.rows(), f.records()), (before, vec![]), "{from} -> {to}");
        assert!(f.disk(from).exists() && !f.disk(to).exists(), "{from} -> {to}");
        f.assert_settled();
    }
}

/// A step moved a file that is not downloaded: nothing on disk changed. A file a person
/// put at the new name since is not that item's entry.
#[test]
fn a_file_put_by_hand_at_the_new_place_of_an_item_with_nothing_on_disk_is_not_moved_under_its_row() {
    let mut f = folder_of_each();
    let record = move_record(&mut f, "docs/remote.bin", "mine/remote.bin");
    f.record(&record);
    f.write("mine/remote.bin", b"put by hand");
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 1)]);
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    assert_eq!(f.row("mine/remote.bin").unwrap().onedrive_item, None);
    assert_eq!((f.read("docs/remote.bin"), f.records()), (None, vec![]));
}

/// The row was written and the record was not taken with it: the disk is not undone.
#[test]
fn a_move_whose_row_was_written_is_not_moved_back() {
    let mut f = folder_of_each();
    let record = move_record(&mut f, "top.txt", "mine/top.txt");
    f.record(&record);
    f.move_through_the_mount("top.txt", "mine/top.txt");
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records()), (before, vec![]));
    assert!(f.disk("mine/top.txt").exists());
}

// ---- A move over another item's entry: the record, the set-aside, the move, the row. ----

/// The record of `top.txt`, the user's own, saved over the copy `docs/a.txt` by a rename.
fn save_over_record(f: &mut Folder) -> (PlaceChangeRecord, OsString) {
    let (item, replaced) = (f.id("top.txt"), f.id("docs/a.txt"));
    let aside = aside_name(replaced);
    (PlaceChangeRecord::MoveEntryOver { item, from: f.place(Path::new("top.txt")), to: f.place(Path::new("docs/a.txt")), replaced, aside_name: aside.clone() }, aside)
}

#[test]
fn a_move_over_that_died_before_the_disk_leaves_everything_as_it_was() {
    let mut f = folder_of_each();
    let (record, _) = save_over_record(&mut f);
    f.record(&record);
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records()), (before, vec![]));
}

#[test]
fn a_move_over_that_died_after_the_set_aside_has_the_entry_put_back() {
    let mut f = folder_of_each();
    let (record, aside) = save_over_record(&mut f);
    f.record(&record);
    f.rename("docs/a.txt", &aside);
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records()), (before, vec![]));
    assert_eq!((f.read("docs/a.txt").as_deref(), f.read("top.txt").as_deref(), f.read(&aside)), (Some(&b"aaa"[..]), Some(&b"top"[..]), None));
    assert_eq!(f.mark_of("docs/a.txt").as_deref(), Some("A"));
    f.assert_settled();
}

#[test]
fn a_move_over_that_died_after_the_move_is_moved_back_and_the_entry_put_back() {
    let mut f = folder_of_each();
    let (record, aside) = save_over_record(&mut f);
    f.record(&record);
    f.rename("docs/a.txt", &aside);
    f.rename("top.txt", "docs/a.txt");
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records()), (before, vec![]), "the item of OneDrive keeps its row and its content");
    assert_eq!((f.read("docs/a.txt").as_deref(), f.read("top.txt").as_deref(), f.read(&aside)), (Some(&b"aaa"[..]), Some(&b"top"[..]), None));
    f.assert_settled();
}

// ---- A set-aside before a removal: the record, the set-aside, the row. ----

fn set_aside_record(f: &mut Folder, rel: &str) -> (PlaceChangeRecord, OsString) {
    let item = f.id(rel);
    (PlaceChangeRecord::SetAside { item, place: f.place(Path::new(rel)), aside_name: aside_name(item) }, aside_name(item))
}

#[test]
fn a_removal_that_died_before_the_disk_leaves_everything_as_it_was() {
    let mut f = folder_of_each();
    let (record, _) = set_aside_record(&mut f, "docs/a.txt");
    f.record(&record);
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records()), (before, vec![]));
}

/// A removal by a program, a removal OneDrive asked for and the taking of OneDrive's new
/// content all set the entry aside first: with no row written, it is put back, and nothing
/// is removed anywhere.
#[test]
fn a_set_aside_that_died_before_its_row_has_the_entry_put_back_and_no_row_changes() {
    for rel in ["docs/a.txt", "top.txt", "mine"] {
        let mut f = folder_of_each();
        let (record, aside) = set_aside_record(&mut f, rel);
        f.record(&record);
        f.rename(rel, &aside);
        let before = f.rows();
        assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)], "{rel}");
        assert_eq!((f.rows(), f.records()), (before, vec![]), "{rel}");
        assert!(f.disk(rel).exists() && !f.disk(&aside).exists(), "{rel}");
        f.assert_settled();
    }
}

/// The step that takes OneDrive's new content sets the old copy aside and commits: its row
/// says not on disk, with the new content. The old copy put back would be read as content
/// the user put there, and sent over the new one.
#[test]
fn an_entry_set_aside_is_not_put_back_once_the_row_says_its_content_is_not_on_disk() {
    let mut f = folder_of_each();
    let (record, aside) = set_aside_record(&mut f, "docs/a.txt");
    f.record(&record);
    f.rename("docs/a.txt", &aside);
    let a = f.id("docs/a.txt");
    f.store.item_store().set_content_presence(a, ContentPresence::NotOnDisk).unwrap();
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records()), (before, vec![]));
    assert!(f.disk(&aside).exists() && !f.disk("docs/a.txt").exists(), "it is left for the cleaning of what was set aside");
}

// ---- A landed download: the record, the link, the row. ----

/// The record of the landing of `docs/remote.bin`, and the file as the landing links it:
/// whole, marked, with the content's size and time.
fn land_remote(f: &mut Folder, content: ContentVersion, is_linked: bool) -> LocalItemId {
    let remote = f.id("docs/remote.bin");
    let record = PlaceChangeRecord::LinkFile { item: remote, place: f.place(Path::new("docs/remote.bin")), content: content.clone() };
    f.record(&record);
    if is_linked {
        f.write("docs/remote.bin", &vec![b'r'; content.size() as usize]);
        set_mtime(&f.disk("docs/remote.bin"), content.mtime().0);
        f.mark_copy("docs/remote.bin", "R");
    }
    remote
}

#[test]
fn a_landing_that_died_before_the_link_leaves_everything_as_it_was() {
    let mut f = folder_of_each();
    let downloaded = content(&f.row("docs/remote.bin").unwrap()).unwrap();
    land_remote(&mut f, downloaded, false);
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records()), (before, vec![]));
}

#[test]
fn a_landing_that_died_after_the_link_has_its_row_say_on_disk() {
    let mut f = folder_of_each();
    let downloaded = content(&f.row("docs/remote.bin").unwrap()).unwrap();
    let remote = land_remote(&mut f, downloaded.clone(), true);
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)], "the row was written with the record's going");
    let row = f.item(remote);
    assert_eq!((row.content_presence, content(&row), row.unsent_since), (ContentPresence::OnDisk, Some(downloaded), None));
    assert_eq!(f.records(), vec![]);
    f.assert_settled();
}

/// The file is a download of a content the row has no more. Left there, it would be content
/// changed here, and the older content would be sent over what OneDrive has.
#[test]
fn a_landed_file_whose_row_has_another_content_by_now_is_removed_and_nothing_is_sent() {
    let mut f = folder_of_each();
    let older = ContentVersion::Hashed { size: 5, mtime: MTIME - 100, mtime_nsec: 0, hash: Some("h-older".to_owned()), ctag: Some("c-older".to_owned()) };
    land_remote(&mut f, older, true);
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!((f.rows(), f.records(), f.read("docs/remote.bin")), (before, vec![], None));
}

// ---- A place that cannot be looked at. ----

#[test]
fn a_record_whose_place_cannot_be_looked_at_stands_and_its_item_is_held_until_it_can() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    let record = move_record(&mut f, "top.txt", "mine/top.txt");
    f.record(&record);
    f.rename("top.txt", "mine/top.txt");
    let (top, mine) = (f.id("top.txt"), f.id("mine"));
    let before = f.rows();

    fs::set_permissions(f.disk("mine"), fs::Permissions::from_mode(0o000)).unwrap();
    let findings = f.check();
    fs::set_permissions(f.disk("mine"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 3)]);
    assert_eq!(findings.list_held_items(), [mine, top], "the directory that could not be looked at, and the item the record names");
    assert!(findings.has_unread_part());
    assert_eq!(left(&findings), [PathBuf::from("mine"), PathBuf::from("mine/top.txt"), PathBuf::from("top.txt")]);
    assert!(findings.unresolved().iter().any(|left| left.reason == UnresolvedReason::PlaceChangeUnsettled));
    assert_eq!((f.rows(), f.records()), (before.clone(), vec![record]), "the row is not found removed, and the record stands");

    // Readable: the change is undone, and nothing is left.
    let findings = f.check();
    assert_eq!((found(&findings), findings.unresolved(), findings.list_held_items()), (vec![(DiskFinding::NothingChanged, 6)], &[][..], &[][..]));
    assert_eq!((f.rows(), f.records()), (before, vec![]));
    assert!(f.disk("top.txt").exists());
}

// ---- An undo that cannot be done: the record goes, and the walk decides the places. ----

/// Review S2 a): `mv mine/own.txt moved.txt` through the mount, a death after the disk, and
/// then `rmdir mine` by hand. The record stood at every start and `moved.txt` had no row.
#[test]
fn a_move_whose_old_directory_is_gone_is_not_undone_and_the_moved_file_is_a_new_item() {
    let mut f = folder_of_each();
    let record = move_record(&mut f, "mine/own.txt", "moved.txt");
    f.record(&record);
    f.rename("mine/own.txt", "moved.txt");
    fs::remove_dir(f.disk("mine")).unwrap();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 4), (DiskFinding::Made, 1), (DiskFinding::Removed, 2)]);
    assert_eq!((findings.unresolved(), findings.list_held_items(), findings.has_unread_part(), f.records()), (&[][..], &[][..], false, vec![]));
    let moved = f.row("moved.txt").unwrap();
    assert_eq!((content(&moved), moved.onedrive_item.clone()), (changed_here(3, 1), None));
    assert!(moved.unsent_since.is_some() && f.row("mine").is_none());
    f.assert_settled();
}

/// Review S2 b): `rm docs/a.txt` through the mount, a death after the set-aside, and then
/// `rm -r docs` by hand.
#[test]
fn a_set_aside_whose_directory_is_gone_is_not_undone_and_the_item_is_not_shown_any_more() {
    let mut f = folder_of_each();
    let (record, aside) = set_aside_record(&mut f, "docs/a.txt");
    let a = f.id("docs/a.txt");
    f.record(&record);
    f.rename("docs/a.txt", &aside);
    fs::remove_dir_all(f.disk("docs")).unwrap();
    let before = f.rows();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 5), (DiskFinding::NotShownAnyMore, 1)]);
    assert_eq!((findings.unresolved(), findings.list_held_items(), f.records()), (&[][..], &[][..], vec![]));
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    assert_eq!((f.item(a).local, f.item(a).unsent_since), (None, None));
    assert!(f.disk(&aside).exists(), "left for the cleaning of what was set aside");
    for _ in 0..2 {
        assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5)], "it does not come back at a later start");
    }
}

/// Review S3: an editor's save over `docs/a.txt`, a death after both renames, and then the
/// person saves that name again by hand. The person's file was moved under the editor's
/// temporary name, and the document showed the content from before the save.
#[test]
fn a_save_over_is_not_undone_over_what_the_person_saved_at_the_name_since() {
    let mut f = folder_of_each();
    let (record, aside) = save_over_record(&mut f);
    let a = f.id("docs/a.txt");
    f.record(&record);
    f.rename("docs/a.txt", &aside);
    f.rename("top.txt", "docs/a.txt");
    f.write("docs/a.txt", b"saved again by hand, no daemon");
    let before = f.rows();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 4), (DiskFinding::ContentChanged, 1), (DiskFinding::Removed, 1)]);
    assert_eq!((findings.unresolved(), findings.list_held_items(), f.records()), (&[][..], &[][..], vec![]));
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    // The document is the person's last save, under the name they gave it, and is sent.
    assert_eq!((f.read("docs/a.txt").as_deref(), f.read("top.txt")), (Some(&b"saved again by hand, no daemon"[..]), None));
    assert_eq!((f.id("docs/a.txt"), content(&f.item(a))), (a, changed_here(30, 1)));
    f.assert_settled();
}

/// Review NOTE 3: a move that died before the disk; the person moved the entry elsewhere
/// and made a file at the record's new place. That file is not the moved entry.
#[test]
fn a_file_the_person_made_at_the_new_place_of_a_move_that_never_reached_the_disk_is_not_moved_back() {
    let mut f = folder_of_each();
    let record = move_record(&mut f, "docs/a.txt", "docs/b.txt");
    f.record(&record);
    f.rename("docs/a.txt", "top2.txt");
    f.write("docs/b.txt", b"the person's own b");
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Made, 2), (DiskFinding::NotShownAnyMore, 1)]);
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    assert_eq!((f.read("docs/b.txt").as_deref(), f.read("docs/a.txt"), f.records()), (Some(&b"the person's own b"[..]), None, vec![]));
}

/// Review NOTE 1: a file of the size and the second of the download that was to land, which
/// the person put there: it does not carry the marks of the landed copy, and stays.
#[test]
fn a_file_of_the_recorded_size_and_time_that_is_not_marked_as_the_landed_copy_is_not_removed() {
    let mut f = folder_of_each();
    let older = ContentVersion::Hashed { size: 5, mtime: MTIME - 100, mtime_nsec: 0, hash: Some("h-older".to_owned()), ctag: Some("c-older".to_owned()) };
    land_remote(&mut f, older, false);
    f.write("docs/remote.bin", b"12345");
    set_mtime(&f.disk("docs/remote.bin"), MTIME - 100);
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    assert_eq!((f.read("docs/remote.bin").as_deref(), f.records()), (Some(&b"12345"[..]), vec![]));
}
