use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use konedrive_fs::placeholder::{self, State, XATTR_CTAG, XATTR_ITEM_ID, XATTR_STATE};
use konedrive_tree::{
    ContentPresence, ContentVersion, DriveId, FileSyncState, Item, ItemKind, ItemPlace, LocalItemId, LocalVersionChange, OneDriveItemId, OneDriveItemReference, TreeStore,
};

use super::{BackingDirectoryCheck, DiskFinding, DiskFindings, UnresolvedReason};
use crate::folder::disk::HOLDING;
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::upload_rules::tests::rules;
use crate::fuse::upload_rules::UploadRules;

mod convergence;
mod cost;
mod daemon_death;
mod hand_moves;

const ROOT: LocalItemId = LocalItemId::ROOT;

/// The time every file of a test is last modified at, unless the test says another.
const MTIME: i64 = 1_700_000_000;

/// A backing directory under no mount and a store in memory: what the check is run over.
pub(super) struct Folder {
    dir: tempfile::TempDir,
    backing_directory: BackingDirectory,
    pub(super) store: TreeStore,
    upload_rules: UploadRules,
    drive_id: DriveId,
}

pub(super) fn folder() -> Folder {
    let dir = tempfile::tempdir().unwrap();
    let backing_directory = BackingDirectory::open(dir.path()).unwrap();
    Folder { dir, backing_directory, store: TreeStore::in_memory().unwrap(), upload_rules: rules(), drive_id: DriveId::new("d1").unwrap() }
}

pub(super) fn reference(drive: &str, id: &str) -> OneDriveItemReference {
    OneDriveItemReference { drive_id: DriveId::new(drive).unwrap(), item_id: OneDriveItemId::new(id) }
}

pub(super) fn set_mtime(path: &Path, seconds: i64) {
    let file = fs::File::open(path).unwrap();
    placeholder::set_mtime(&file, UNIX_EPOCH + Duration::from_secs(seconds as u64)).unwrap();
}

/// The findings a check wrote, each with how many, without those it has none of.
pub(super) fn found(findings: &DiskFindings) -> Vec<(DiskFinding, usize)> {
    DiskFinding::ALL.into_iter().map(|finding| (finding, findings.count(finding))).filter(|(_, count)| *count > 0).collect()
}

impl Folder {
    pub(super) fn disk(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.dir.path().join(rel)
    }

    // What a person does by hand in the backing directory while no daemon runs.

    /// A file with these bytes, last modified at [`MTIME`].
    pub(super) fn write(&self, rel: impl AsRef<Path>, bytes: &[u8]) {
        let path = self.disk(rel);
        let _ = fs::remove_file(&path);
        fs::write(&path, bytes).unwrap();
        set_mtime(&path, MTIME);
    }

    pub(super) fn make_directory(&self, rel: impl AsRef<Path>) {
        fs::create_dir(self.disk(rel)).unwrap();
    }

    pub(super) fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) {
        fs::rename(self.disk(from), self.disk(to)).unwrap();
    }

    /// Marks the file as the whole copy of the OneDrive file `id`, of the cTag `c-<id>`.
    pub(super) fn mark_copy(&self, rel: impl AsRef<Path>, id: &str) {
        let path = self.disk(rel);
        xattr::set(&path, XATTR_ITEM_ID, id.as_bytes()).unwrap();
        xattr::set(&path, XATTR_STATE, State::Hydrated.as_str().as_bytes()).unwrap();
        xattr::set(&path, XATTR_CTAG, format!("c-{id}").as_bytes()).unwrap();
    }

    pub(super) fn mark_directory(&self, rel: impl AsRef<Path>, id: &str) {
        xattr::set(self.disk(rel), XATTR_ITEM_ID, id.as_bytes()).unwrap();
    }

    pub(super) fn mark_of(&self, rel: impl AsRef<Path>) -> Option<String> {
        xattr::get(self.disk(rel), XATTR_ITEM_ID).unwrap().map(|id| String::from_utf8(id).unwrap())
    }

    // The rows.

    pub(super) fn item(&mut self, id: LocalItemId) -> Item {
        self.store.item_store().find_item(id).unwrap().unwrap_or_else(|| panic!("item {id} has no row"))
    }

    /// The item the folder shows at `rel`.
    pub(super) fn row(&mut self, rel: impl AsRef<Path>) -> Option<Item> {
        let mut at = self.item(ROOT);
        for name in rel.as_ref().components() {
            at = self.store.item_store().find_item_at(at.local_item_id, name.as_os_str()).unwrap()?;
        }
        Some(at)
    }

    pub(super) fn id(&mut self, rel: impl AsRef<Path>) -> LocalItemId {
        let rel = rel.as_ref();
        self.row(rel).unwrap_or_else(|| panic!("no row at {rel:?}")).local_item_id
    }

    pub(super) fn rows(&mut self) -> Vec<Item> {
        self.store.item_store().list_items().unwrap()
    }

    fn place(&mut self, rel: &Path) -> ItemPlace {
        let parent = self.id(rel.parent().unwrap());
        ItemPlace { parent_local_item_id: parent, name: rel.file_name().unwrap().to_owned() }
    }

    /// The row a write operation of the mount makes for what is at `rel`: an item OneDrive
    /// does not have, a file with the size and time the disk has.
    pub(super) fn make_row(&mut self, rel: impl AsRef<Path>, kind: ItemKind, content: Option<ContentVersion>) -> LocalItemId {
        let place = self.place(rel.as_ref());
        let is_name_ignored = self.upload_rules.is_name_ignored(&place.name);
        self.store.item_store().apply(&LocalVersionChange::Made { kind, place, content, is_name_ignored, replaced: None }).unwrap()
    }

    /// The row of `id` as it is once OneDrive has the item as `onedrive_id` and the two
    /// sides agree: bound, its synced place the local one, and for a file one hashed content
    /// of the cTag `c-<onedrive_id>` on both sides. No operation of the store writes what
    /// was agreed on yet, so it is written as it is.
    pub(super) fn agree(&mut self, id: LocalItemId, onedrive_id: &str) {
        self.store.item_store().bind_item(id, &reference("d1", onedrive_id)).unwrap();
        let content = if self.item(id).kind == ItemKind::File {
            format!(
                ", local_hash = 'h-{onedrive_id}', local_ctag = 'c-{onedrive_id}', local_change_count = NULL, local_mtime_nsec = 0, \
                 synced_size = local_size, synced_mtime = local_mtime, synced_mtime_nsec = 0, synced_hash = 'h-{onedrive_id}', synced_ctag = 'c-{onedrive_id}'"
            )
        } else {
            String::new()
        };
        self.store.bench_sql(&format!("UPDATE items SET synced_parent_id = local_parent_id, synced_name = local_name{content} WHERE local_item_id = {id}")).unwrap();
    }

    /// A file of the user's own, on disk and in the rows.
    pub(super) fn own_file(&mut self, rel: &str, bytes: &[u8]) -> LocalItemId {
        self.write(rel, bytes);
        let content = ContentVersion::ChangedHere { size: bytes.len() as u64, mtime: MTIME, mtime_nsec: 0, change_count: 1 };
        self.make_row(rel, ItemKind::File, Some(content))
    }

    pub(super) fn own_directory(&mut self, rel: &str) -> LocalItemId {
        self.make_directory(rel);
        self.make_row(rel, ItemKind::Directory, None)
    }

    /// The clean downloaded copy of the OneDrive file `onedrive_id`, on disk and in the rows.
    pub(super) fn copy(&mut self, rel: &str, bytes: &[u8], onedrive_id: &str) -> LocalItemId {
        let id = self.own_file(rel, bytes);
        self.mark_copy(rel, onedrive_id);
        self.agree(id, onedrive_id);
        assert_eq!(self.item(id).file_sync_state(), Some(FileSyncState::Synced));
        id
    }

    /// A directory of OneDrive, on disk and in the rows.
    pub(super) fn onedrive_directory(&mut self, rel: &str, onedrive_id: &str) -> LocalItemId {
        let id = self.own_directory(rel);
        self.mark_directory(rel, onedrive_id);
        self.agree(id, onedrive_id);
        id
    }

    /// A file of OneDrive whose content is not on disk: a row, and nothing in the backing
    /// directory.
    pub(super) fn not_downloaded(&mut self, rel: &str, size: u64, onedrive_id: &str) -> LocalItemId {
        let content = ContentVersion::ChangedHere { size, mtime: MTIME, mtime_nsec: 0, change_count: 1 };
        let id = self.make_row(rel, ItemKind::File, Some(content));
        self.agree(id, onedrive_id);
        self.store.item_store().set_content_presence(id, ContentPresence::NotOnDisk).unwrap();
        assert_eq!(self.item(id).file_sync_state(), Some(FileSyncState::RemoteOnly));
        id
    }

    /// The item at `from` is moved to `to` as a rename through the mount does it: on disk,
    /// and in its row. What was agreed on for it stays: an item of OneDrive in a directory
    /// OneDrive does not have was moved into it, and its move is not sent.
    pub(super) fn move_through_the_mount(&mut self, from: &str, to: &str) {
        let item = self.id(from);
        self.rename(from, to);
        let place = self.place(Path::new(to));
        let is_name_ignored = self.upload_rules.is_name_ignored(&place.name);
        self.store.item_store().apply(&LocalVersionChange::Moved { item, place, is_name_ignored }).unwrap();
    }

    // The check.

    pub(super) fn check(&mut self) -> DiskFindings {
        let check = BackingDirectoryCheck::new(&self.backing_directory, &self.upload_rules, &self.drive_id);
        check.run(&mut self.store.item_store()).unwrap()
    }

    /// Every entry of the backing directory, by its path, without the daemon's own.
    pub(super) fn walk_disk(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut directories = vec![PathBuf::new()];
        while let Some(directory) = directories.pop() {
            for entry in fs::read_dir(self.disk(&directory)).unwrap() {
                let entry = entry.unwrap();
                if crate::folder::disk::daemon_owned(&entry.file_name()) {
                    continue;
                }
                let rel = directory.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    directories.push(rel.clone());
                }
                found.push(rel);
            }
        }
        found.sort();
        found
    }

    /// The rows say what the disk has: every entry on disk has a row at its path with its
    /// kind, and a file's with its size and time; every row whose content is on disk, and
    /// every item OneDrive does not have, has its entry.
    pub(super) fn assert_rows_say_the_disk(&mut self) {
        for rel in self.walk_disk() {
            let metadata = fs::symlink_metadata(self.disk(&rel)).unwrap();
            let row = self.row(&rel).unwrap_or_else(|| panic!("{rel:?} is on disk and has no row"));
            let kind = if metadata.is_dir() { ItemKind::Directory } else { ItemKind::File };
            assert_eq!(row.kind, kind, "{rel:?}");
            if kind == ItemKind::File {
                let content = row.local.as_ref().unwrap().content.as_ref().unwrap();
                assert_eq!((content.size(), content.mtime().0), (metadata.size(), metadata.mtime()), "{rel:?}");
                assert_eq!(row.content_presence, ContentPresence::OnDisk, "{rel:?}");
            }
        }
        for row in self.rows() {
            let Some(place) = row.local.as_ref().and_then(|local| local.place.as_ref()) else { continue };
            let path = self.store.item_store().find_path(row.local_item_id).unwrap().unwrap();
            assert_eq!(path.file_name(), Some(place.name.as_os_str()));
            let on_disk = fs::symlink_metadata(self.disk(&path)).ok();
            if row.content_presence == ContentPresence::OnDisk || row.onedrive_item.is_none() {
                let on_disk = on_disk.unwrap_or_else(|| panic!("the row of {path:?} has no entry on disk: {row:?}"));
                assert_eq!(on_disk.is_dir(), row.kind == ItemKind::Directory, "{path:?}");
            }
            assert_eq!(row.content_before_write, None, "{path:?}");
        }
    }

    /// The check is done: the rows say the disk, and a second run finds nothing changed
    /// anywhere and writes nothing.
    pub(super) fn assert_settled(&mut self) {
        self.assert_rows_say_the_disk();
        let before = self.rows();
        let again = self.check();
        assert!(found(&again).iter().all(|(finding, _)| *finding == DiskFinding::NothingChanged), "a second run found {:?}", found(&again));
        assert_eq!(again.unresolved(), []);
        assert_eq!(self.rows(), before, "a second run wrote");
    }
}

fn changed_here(size: u64, change_count: u64) -> Option<ContentVersion> {
    Some(ContentVersion::ChangedHere { size, mtime: MTIME, mtime_nsec: 0, change_count })
}

fn content(row: &Item) -> Option<ContentVersion> {
    row.local.as_ref().and_then(|local| local.content.clone())
}

/// A folder with one of each thing the rows can say of a path, each as the disk has it.
fn folder_of_each() -> Folder {
    let mut f = folder();
    f.onedrive_directory("docs", "D");
    f.copy("docs/a.txt", b"aaa", "A");
    f.not_downloaded("docs/remote.bin", 9, "R");
    f.own_directory("mine");
    f.own_file("mine/own.txt", b"own");
    f.own_file("top.txt", b"top");
    f
}

#[test]
fn a_folder_that_agrees_with_its_rows_has_nothing_changed() {
    let mut f = folder_of_each();
    let before = f.rows();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!(findings.unresolved(), []);
    assert_eq!(f.rows(), before);
    f.assert_rows_say_the_disk();
}

#[test]
fn a_file_and_a_directory_made_by_hand_are_new_items_with_what_the_rules_say_of_their_names() {
    let mut f = folder_of_each();
    f.write("docs/new.txt", b"new!");
    f.make_directory("cache");
    f.write("cache/in.txt", b"x");
    f.write("a.tmp", b"t");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 4)]);

    let new = f.row("docs/new.txt").unwrap();
    assert_eq!((new.kind, new.onedrive_item.clone(), content(&new)), (ItemKind::File, None, changed_here(4, 1)));
    assert_eq!((new.content_presence, new.is_name_ignored, new.synced_content.clone()), (ContentPresence::OnDisk, false, None));
    let cache = f.row("cache").unwrap();
    assert_eq!((cache.kind, cache.is_name_ignored), (ItemKind::Directory, true));
    let inside = f.row("cache/in.txt").unwrap();
    assert!(!inside.is_name_ignored && f.store.item_store().is_ignored(inside.local_item_id).unwrap(), "left here with its directory");
    assert!(f.row("a.tmp").unwrap().is_name_ignored);
    f.assert_settled();
}

/// A file manager copies the extended attributes with the file, and the mode: a downloaded
/// copy has no write bit.
#[test]
fn a_copy_made_by_hand_keeps_the_marks_and_is_a_new_item_without_them() {
    let mut f = folder_of_each();
    let original = f.id("docs/a.txt");
    f.write("docs/a (copy).txt", b"aaa");
    f.mark_copy("docs/a (copy).txt", "A");
    fs::set_permissions(f.disk("docs/a (copy).txt"), fs::Permissions::from_mode(0o444)).unwrap();
    f.make_directory("docs (copy)");
    f.mark_directory("docs (copy)", "D");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::MadeWithStaleMark, 2)]);

    assert_eq!(f.id("docs/a.txt"), original);
    assert_eq!(f.row("docs/a.txt").unwrap().onedrive_item, Some(reference("d1", "A")));
    let copy = f.row("docs/a (copy).txt").unwrap();
    assert_eq!((copy.onedrive_item.clone(), content(&copy)), (None, changed_here(3, 1)));
    assert_eq!(f.row("docs (copy)").unwrap().onedrive_item, None);
    assert_eq!((f.mark_of("docs/a (copy).txt"), f.mark_of("docs (copy)"), f.mark_of("docs/a.txt")), (None, None, Some("A".to_owned())));
    assert_eq!(xattr::get(f.disk("docs/a (copy).txt"), XATTR_CTAG).unwrap(), None);
    f.assert_settled();
}

/// A store that was lost: the marks are all that says which item of OneDrive an entry is.
#[test]
fn marked_entries_no_row_knows_are_new_items_bound_to_the_ids_of_their_marks() {
    let mut f = folder();
    f.make_directory("docs");
    f.mark_directory("docs", "D");
    f.write("docs/a.txt", b"aaa");
    f.mark_copy("docs/a.txt", "A");
    f.write("own.txt", b"o");
    f.write("docs/unversioned.txt", b"uuuu");
    xattr::set(f.disk("docs/unversioned.txt"), XATTR_ITEM_ID, b"U").unwrap();
    xattr::set(f.disk("docs/unversioned.txt"), XATTR_STATE, State::Hydrated.as_str().as_bytes()).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::Made, 1), (DiskFinding::MarkedWithoutRow, 3)]);

    assert_eq!(f.row("docs").unwrap().onedrive_item, Some(reference("d1", "D")));
    let a = f.row("docs/a.txt").unwrap();
    // The content is told by the cTag of the mark, with no hash: the plan takes the copy as
    // clean when a listing brings that cTag as OneDrive's.
    let marked = ContentVersion::Hashed { size: 3, mtime: MTIME, mtime_nsec: 0, hash: None, ctag: Some("c-A".to_owned()) };
    assert_eq!((a.onedrive_item.clone(), content(&a), a.content_presence), (Some(reference("d1", "A")), Some(marked), ContentPresence::OnDisk));
    assert_eq!(a.synced_content, None, "nothing is agreed on before a listing brings OneDrive's version");
    assert_eq!(xattr::get(f.disk("docs/a.txt"), XATTR_CTAG).unwrap().as_deref(), Some(&b"c-A"[..]));
    // A copy landed before a landing wrote the cTag has no such mark: nothing tells its content but that it is here.
    assert_eq!(content(&f.row("docs/unversioned.txt").unwrap()), changed_here(4, 1));
    assert_eq!(f.row("own.txt").unwrap().onedrive_item, None);
    f.assert_settled();
}

/// A lost store whose listing came first: the rows of OneDrive's items are there, and the
/// folder shows none of them. The item on disk is merged with the row that has its id, in
/// whichever drive that row says.
#[test]
fn a_marked_entry_is_merged_with_the_row_of_its_id_that_the_folder_does_not_show() {
    let mut f = folder();
    let listed = f.store.item_store().make_onedrive_item(&reference("other-drive", "A"), ItemKind::File).unwrap();
    f.write("a.txt", b"aaa");
    f.mark_copy("a.txt", "A");
    assert_eq!(found(&f.check()), [(DiskFinding::MarkedWithoutRow, 1)]);
    assert_eq!(f.row("a.txt").unwrap().onedrive_item, Some(reference("other-drive", "A")));
    assert_eq!(f.store.item_store().find_item(listed).unwrap(), None, "the listed row went into the one the folder shows");
    assert_eq!(f.rows().len(), 2);
    f.assert_settled();
}

/// An item's children are of its drive: a shared folder is another drive's.
#[test]
fn a_marked_entry_no_row_has_is_bound_in_the_drive_of_the_directory_it_is_in() {
    let mut f = folder();
    f.make_directory("shared");
    let shared = f.make_row("shared", ItemKind::Directory, None);
    f.store.item_store().bind_item(shared, &reference("other-drive", "S")).unwrap();
    f.mark_directory("shared", "S");
    f.write("shared/a.txt", b"aaa");
    f.mark_copy("shared/a.txt", "A");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 1), (DiskFinding::MarkedWithoutRow, 1)]);
    assert_eq!(f.row("shared/a.txt").unwrap().onedrive_item, Some(reference("other-drive", "A")));
}

#[test]
fn a_file_whose_size_or_time_is_not_the_rows_has_its_content_changed() {
    let mut f = folder_of_each();
    f.write("docs/a.txt", b"edited");
    f.mark_copy("docs/a.txt", "A");
    set_mtime(&f.disk("top.txt"), MTIME + 60);
    let before = f.row("docs/a.txt").unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 4), (DiskFinding::ContentChanged, 2)]);

    let a = f.row("docs/a.txt").unwrap();
    assert_eq!(content(&a), changed_here(6, 1));
    assert_eq!((a.synced_content.clone(), a.onedrive_item.clone(), a.content_before_write.clone()), (before.synced_content, before.onedrive_item, None));
    assert_eq!(a.file_sync_state(), Some(FileSyncState::ModifiedHere));
    let top = f.row("top.txt").unwrap();
    assert_eq!(content(&top), Some(ContentVersion::ChangedHere { size: 3, mtime: MTIME + 60, mtime_nsec: 0, change_count: 2 }), "its count is raised");
    f.assert_settled();
}

/// Without the cTag mark a clean copy of an older version cannot be told from the row's.
#[test]
fn a_copy_of_the_rows_size_and_time_marked_with_another_ctag_has_its_content_changed() {
    let mut f = folder_of_each();
    xattr::set(f.disk("docs/a.txt"), XATTR_CTAG, b"c-older").unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    assert_eq!(f.row("docs/a.txt").unwrap().file_sync_state(), Some(FileSyncState::ModifiedHere));
    f.assert_settled();
}

#[test]
fn what_is_gone_from_the_disk_is_removed_here_and_a_clean_copy_is_only_not_on_disk() {
    let mut f = folder_of_each();
    let changed = f.copy("docs/changed.txt", b"ccc", "C");
    f.store.item_store().apply(&LocalVersionChange::Truncated { item: changed, size: 3, mtime: MTIME, mtime_nsec: 0 }).unwrap();
    let (clean, own, mine) = (f.id("docs/a.txt"), f.id("top.txt"), f.id("mine"));
    let before = f.item(clean);
    fs::remove_file(f.disk("docs/a.txt")).unwrap();
    fs::remove_file(f.disk("docs/changed.txt")).unwrap();
    fs::remove_file(f.disk("top.txt")).unwrap();
    fs::remove_dir_all(f.disk("mine")).unwrap();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 2), (DiskFinding::Removed, 4), (DiskFinding::ContentNoLongerOnDisk, 1)]);

    assert_eq!(f.item(clean), Item { content_presence: ContentPresence::NotOnDisk, ..before }, "not a removal: nothing else of the row changes");
    let changed = f.item(changed);
    assert_eq!((changed.local.clone(), changed.onedrive_item.clone()), (None, Some(reference("d1", "C"))), "the removal is to be sent");
    assert!(changed.synced_place.is_some());
    assert_eq!(f.store.item_store().find_item(own).unwrap(), None, "an item OneDrive never had loses its row");
    assert_eq!((f.store.item_store().find_item(mine).unwrap(), f.row("mine/own.txt")), (None, None), "with what was in it");
    f.assert_settled();
}

/// A directory of OneDrive needs none on disk; what was downloaded into one that is gone is
/// each decided by itself.
#[test]
fn a_directory_of_onedrive_that_is_gone_stays_and_its_clean_copies_are_not_on_disk() {
    let mut f = folder_of_each();
    fs::remove_dir_all(f.disk("docs")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentNoLongerOnDisk, 1)]);
    assert_eq!(f.row("docs").unwrap().onedrive_item, Some(reference("d1", "D")));
    assert_eq!(f.row("docs/a.txt").unwrap().content_presence, ContentPresence::NotOnDisk);
    f.assert_settled();
}

/// A directory OneDrive does not have goes with all that was in it, also what OneDrive has:
/// a row cannot stay in a directory whose row is gone.
#[test]
fn a_directory_onedrive_does_not_have_that_is_gone_takes_the_items_of_onedrive_in_it() {
    let mut f = folder();
    f.own_directory("mine");
    let clean = f.copy("a.txt", b"aaa", "A");
    let remote = f.not_downloaded("remote.bin", 9, "R");
    f.move_through_the_mount("a.txt", "mine/a.txt");
    let place = ItemPlace { parent_local_item_id: f.id("mine"), name: "remote.bin".into() };
    f.store.item_store().apply(&LocalVersionChange::Moved { item: remote, place, is_name_ignored: false }).unwrap();
    fs::remove_dir_all(f.disk("mine")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::Removed, 3)]);
    assert_eq!((f.row("mine"), f.item(clean).local, f.item(remote).local), (None, None, None));
    assert_eq!(f.item(clean).onedrive_item, Some(reference("d1", "A")));
    f.assert_settled();
}

#[test]
fn the_whole_copy_of_a_file_whose_row_says_not_on_disk_is_on_disk() {
    let mut f = folder_of_each();
    f.write("docs/remote.bin", b"123456789");
    f.mark_copy("docs/remote.bin", "R");
    let before = f.row("docs/remote.bin").unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::CopyLandedBeforeRow, 1)]);
    assert_eq!(f.row("docs/remote.bin").unwrap(), Item { content_presence: ContentPresence::OnDisk, ..before });
    assert_eq!(f.row("docs/remote.bin").unwrap().file_sync_state(), Some(FileSyncState::Synced));
    f.assert_settled();
}

#[test]
fn any_other_file_where_a_rows_content_is_not_on_disk_is_content_the_user_put_there() {
    let mut f = folder_of_each();
    f.write("docs/remote.bin", b"my own bytes");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    let row = f.row("docs/remote.bin").unwrap();
    assert_eq!((content(&row), row.content_presence), (changed_here(12, 1), ContentPresence::OnDisk));
    assert_eq!(row.file_sync_state(), Some(FileSyncState::ModifiedHere));
    f.assert_settled();
}

/// A file marked with the row's id and not whole holds nothing of the file.
#[test]
fn a_file_that_is_not_whole_where_content_is_not_on_disk_changes_nothing() {
    let mut f = folder_of_each();
    fs::write(f.disk("docs/remote.bin"), b"").unwrap();
    xattr::set(f.disk("docs/remote.bin"), XATTR_ITEM_ID, b"R").unwrap();
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!(f.rows(), before);
}

/// Another file's bytes laid over a file: `cp --preserve=all` onto a name that is there.
#[test]
fn a_file_overwritten_with_another_items_copy_has_its_content_changed_and_loses_the_stale_mark() {
    let mut f = folder_of_each();
    f.write("top.txt", b"aaa");
    f.mark_copy("top.txt", "A");
    let top = f.id("top.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    let row = f.row("top.txt").unwrap();
    assert_eq!((row.local_item_id, row.onedrive_item.clone(), content(&row)), (top, None, changed_here(3, 2)));
    assert_eq!(f.mark_of("top.txt"), None);
    assert_eq!(f.row("docs/a.txt").unwrap().onedrive_item, Some(reference("d1", "A")));
    f.assert_settled();
}

/// A file removed here whose removal is not sent, made again by hand: the mount would have
/// the made file take over its binding, and OneDrive see one item whose content changed.
#[test]
fn a_file_made_by_hand_where_one_was_removed_here_takes_over_its_binding() {
    let mut f = folder();
    let changed = f.copy("a.txt", b"aaa", "A");
    f.store.item_store().apply(&LocalVersionChange::Truncated { item: changed, size: 3, mtime: MTIME, mtime_nsec: 0 }).unwrap();
    fs::remove_file(f.disk("a.txt")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::Removed, 1)]);
    let synced_content = f.item(changed).synced_content;

    f.write("a.txt", b"again");
    assert_eq!(found(&f.check()), [(DiskFinding::Made, 1)]);
    let row = f.row("a.txt").unwrap();
    assert_eq!((row.onedrive_item.clone(), content(&row), row.synced_content.clone()), (Some(reference("d1", "A")), changed_here(5, 1), synced_content));
    assert_eq!(f.store.item_store().find_item(changed).unwrap(), None);
    f.assert_settled();
}

#[test]
fn a_name_that_is_not_utf8_has_its_row() {
    let mut f = folder_of_each();
    let name = OsString::from(OsStr::from_bytes(b"caf\xe9.txt"));
    let moved = f.id("docs/a.txt");
    f.write(&name, b"latin1");
    f.make_directory(OsStr::from_bytes(b"\xff\xfe"));
    f.rename("docs/a.txt", Path::new(OsStr::from_bytes(b"\xff\xfe")).join(OsStr::from_bytes(b"a\xe9")));
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Made, 2), (DiskFinding::Moved, 1)]);
    assert_eq!(content(&f.row(&name).unwrap()), changed_here(6, 1));
    let directory = f.id(OsStr::from_bytes(b"\xff\xfe"));
    let place = f.item(moved).local.unwrap().place.unwrap();
    assert_eq!((place.parent_local_item_id, place.name.as_bytes()), (directory, &b"a\xe9"[..]));
    f.assert_settled();
}

/// The daemon's own names are not the folder's: the mount shows nothing under them.
#[test]
fn what_is_under_a_name_the_daemon_uses_for_itself_has_no_row() {
    let mut f = folder_of_each();
    f.make_directory(HOLDING);
    f.write(Path::new(HOLDING).join("kept.txt"), b"k");
    let before = f.rows();
    let findings = f.check();
    assert_eq!((found(&findings), findings.unresolved()), (vec![(DiskFinding::NothingChanged, 6)], &[][..]));
    assert_eq!(f.rows(), before);
}

/// The folder shows no symbolic link: it gets no row, the row that was at its name is left
/// as it is, and the path is said.
#[test]
fn what_is_neither_a_file_nor_a_directory_is_left_and_said() {
    let mut f = folder_of_each();
    std::os::unix::fs::symlink("/etc/hostname", f.disk("link")).unwrap();
    fs::remove_file(f.disk("top.txt")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", f.disk("top.txt")).unwrap();
    let before = f.rows();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 5)]);
    let unresolved: Vec<_> = findings.unresolved().iter().map(|left| (left.path.clone(), left.reason.clone())).collect();
    assert_eq!(unresolved, [(PathBuf::from("link"), UnresolvedReason::NotFileOrDirectory), (PathBuf::from("top.txt"), UnresolvedReason::NotFileOrDirectory)]);
    assert_eq!(f.rows(), before);
}

/// A directory that cannot be read is not an empty one: nothing below it is removed.
#[test]
fn the_rows_below_a_directory_that_cannot_be_read_are_left_and_the_directory_is_said() {
    // SAFETY: no arguments, no failure.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let mut f = folder_of_each();
    fs::set_permissions(f.disk("mine"), fs::Permissions::from_mode(0o000)).unwrap();
    let before = f.rows();
    let findings = f.check();
    fs::set_permissions(f.disk("mine"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 5)]);
    assert!(matches!(findings.unresolved(), [left] if left.path == Path::new("mine") && matches!(left.reason, UnresolvedReason::Unreadable(_))), "{:?}", findings.unresolved());
    assert_eq!(f.rows(), before);
}

/// A file that holds nothing, marked with an id no row has: a made item's content is on
/// disk, so no row can say what it is before OneDrive's listing brings the item.
#[test]
fn a_file_that_is_not_whole_and_marked_with_an_id_no_row_has_is_left_and_said() {
    let mut f = folder_of_each();
    fs::write(f.disk("partial.bin"), b"").unwrap();
    xattr::set(f.disk("partial.bin"), XATTR_ITEM_ID, b"P").unwrap();
    let before = f.rows();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 6)]);
    assert!(matches!(findings.unresolved(), [left] if left.path == Path::new("partial.bin") && left.reason == UnresolvedReason::PartialCopy));
    assert_eq!(f.rows(), before);
}

/// The rows go no deeper than every walk of them does: what is deeper on disk is not read,
/// and is said.
#[test]
fn a_directory_deeper_than_the_rows_go_is_not_read_and_is_said() {
    let mut f = folder();
    let mut deepest = PathBuf::new();
    for _ in 0..konedrive_fs::MAX_DEPTH + 2 {
        deepest.push("d");
    }
    fs::create_dir_all(f.disk(&deepest)).unwrap();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::Made, konedrive_fs::MAX_DEPTH)]);
    let last_with_a_row: PathBuf = deepest.components().take(konedrive_fs::MAX_DEPTH).collect();
    assert!(matches!(findings.unresolved(), [left] if left.path == last_with_a_row && left.reason == UnresolvedReason::TooDeep), "{:?}", findings.unresolved());
    assert!(f.row(&last_with_a_row).is_some());
}
