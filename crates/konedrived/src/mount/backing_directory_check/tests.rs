use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use konedrive_fs::placeholder::{self, State, XATTR_CTAG, XATTR_ITEM_ID, XATTR_STATE};
use konedrive_tree::{
    make_sync_plan, Change, ContentPresence, ContentVersion, DriveId, FileSyncState, Item, ItemKind, ItemPlace, Kind, ListingLink, LocalItemId, LocalVersionChange, OneDriveItemId, OneDriveItemReference,
    OneDriveStep, Placement, Row, SyncPlan, SyncPlanInput, SyncStep, TreeStore,
};

use super::{BackingDirectoryCheck, DiskFinding, DiskFindings, UnresolvedReason};
use crate::mount::backing_directory::BackingDirectory;
use crate::mount::upload_rules::tests::rules;
use crate::mount::upload_rules::UploadRules;

mod convergence;
mod cost;
mod own_marks;
mod place_change_deaths;
mod rebuilt_store;
mod review_musts;
mod table_rows;
mod unread_parts;

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
    Folder { dir, backing_directory, store: store_with_listed_root(), upload_rules: rules(), drive_id: DriveId::new("d1").unwrap() }
}

/// A store in memory that knows its drive, `d1`, whose root, `ROOT`, a full listing told of
/// and ended: nothing else has a row.
fn store_with_listed_root() -> TreeStore {
    let mut store = TreeStore::in_memory().unwrap();
    store.set_drive_id("d1").unwrap();
    let listing = store.item_store().begin_full_listing().unwrap();
    let root = Change::Root(Row { id: "ROOT".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: Some("e-R".into()), ctag: None, quickxor: None, mime: None, placement: Placement::Placed });
    let outcome = store.item_store().write_listed_changes(listing.id, &[root], listing.began_at_count, &ListingLink::DeltaLink("link-0".into())).unwrap();
    assert!(outcome.is_link_written());
    store
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

    /// A copy of the file at `from` made at `to` as `cp -a` makes it: the bytes, the time
    /// and the marks.
    pub(super) fn copy_with_marks(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) {
        let (from, to) = (self.disk(from), self.disk(to));
        fs::copy(&from, &to).unwrap();
        fs::set_permissions(&to, fs::Permissions::from_mode(0o644)).unwrap();
        for mark in [XATTR_ITEM_ID, XATTR_STATE, XATTR_CTAG] {
            if let Some(value) = xattr::get(&from, mark).unwrap() {
                xattr::set(&to, mark, &value).unwrap();
            }
        }
        set_mtime(&to, fs::metadata(&from).unwrap().mtime());
    }

    /// One more name of the file at `from`, as `ln` makes it.
    pub(super) fn link(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) {
        fs::hard_link(self.disk(from), self.disk(to)).unwrap();
    }

    /// Whether the file is marked whole, and the cTag it is marked with.
    pub(super) fn copy_marks_of(&self, rel: impl AsRef<Path>) -> (bool, Option<String>) {
        let path = self.disk(rel);
        let is_whole = xattr::get(&path, XATTR_STATE).unwrap().as_deref() == Some(State::Hydrated.as_str().as_bytes());
        (is_whole, xattr::get(&path, XATTR_CTAG).unwrap().map(|ctag| String::from_utf8(ctag).unwrap()))
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

    pub(super) fn place(&mut self, rel: &Path) -> ItemPlace {
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
    /// sides agree: bound, its synced place and its place in OneDrive the local one, and for
    /// a file one hashed content of the cTag `c-<onedrive_id>` on all three. Written as it
    /// is: the listing and the commits that write these are not the check's to run.
    pub(super) fn agree(&mut self, id: LocalItemId, onedrive_id: &str) {
        self.agree_in(id, "d1", onedrive_id);
    }

    /// [`agree`](Self::agree) for an item of another drive than the folder's own.
    pub(super) fn agree_in(&mut self, id: LocalItemId, drive: &str, onedrive_id: &str) {
        self.store.item_store().bind_item(id, &reference(drive, onedrive_id)).unwrap();
        let content = if self.item(id).kind == ItemKind::File {
            format!(
                ", local_hash = 'h-{onedrive_id}', local_ctag = 'c-{onedrive_id}', local_change_count = NULL, local_mtime_nsec = 0, \
                 synced_size = local_size, synced_mtime = local_mtime, synced_mtime_nsec = 0, synced_hash = 'h-{onedrive_id}', synced_ctag = 'c-{onedrive_id}', \
                 onedrive_size = local_size, onedrive_mtime = local_mtime, onedrive_mtime_nsec = 0, onedrive_hash = 'h-{onedrive_id}', onedrive_ctag = 'c-{onedrive_id}'"
            )
        } else {
            String::new()
        };
        self.store
            .bench_sql(&format!(
                "UPDATE items SET synced_parent_id = local_parent_id, synced_name = local_name, \
                     onedrive_parent_id = local_parent_id, onedrive_name = local_name, onedrive_folded_name = local_folded_name, onedrive_etag = 'e-{onedrive_id}'{content} \
                   WHERE local_item_id = {id}"
            ))
            .unwrap();
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

    /// A file of the item `onedrive_id` of the drive `drive`, the two sides agreeing: a clean
    /// copy on disk with the bytes `twin`, or one that is not downloaded.
    pub(super) fn file_of_drive(&mut self, rel: &str, drive: &str, onedrive_id: &str, on_disk: bool) -> LocalItemId {
        let item = if on_disk { self.own_file(rel, b"twin") } else { self.make_row(rel, ItemKind::File, changed_here(4, 1)) };
        if on_disk {
            self.mark_copy(rel, onedrive_id);
        }
        self.agree_in(item, drive, onedrive_id);
        if !on_disk {
            self.store.item_store().set_content_presence(item, ContentPresence::NotOnDisk).unwrap();
        }
        item
    }

    /// A directory of the drive `drive`, on disk and in the rows: a shared folder is another
    /// drive's than the folder it is in.
    pub(super) fn directory_of_drive(&mut self, rel: &str, drive: &str, onedrive_id: &str) -> LocalItemId {
        let id = self.own_directory(rel);
        self.mark_directory(rel, onedrive_id);
        self.agree_in(id, drive, onedrive_id);
        id
    }

    /// The item at `from` is moved to `to` as a rename through the mount does it: on disk,
    /// and in its row. What was agreed on for it stays: an item of OneDrive in a directory
    /// OneDrive does not have was moved into it, and its move is not sent.
    pub(super) fn move_through_the_mount(&mut self, from: &str, to: &str) {
        let item = self.id(from);
        // An item with nothing on disk is moved in its row alone.
        if self.disk(from).exists() {
            self.rename(from, to);
        }
        let place = self.place(Path::new(to));
        let is_name_ignored = self.upload_rules.is_name_ignored(&place.name);
        self.store.item_store().apply(&LocalVersionChange::Moved { item, place, is_name_ignored }).unwrap();
    }

    // OneDrive, and the plan.

    /// What OneDrive has, as the rows say it: each item a listing would name, a directory
    /// before what is in it.
    pub(super) fn list_onedrive(&mut self) -> Vec<Change> {
        let rows = self.rows();
        let onedrive_id = |id: LocalItemId| rows.iter().find(|row| row.local_item_id == id).and_then(|row| row.onedrive_item.as_ref()).map(|reference| reference.item_id.as_str().to_owned());
        let mut listed = Vec::new();
        for row in &rows {
            let (Some(reference), Some(onedrive)) = (&row.onedrive_item, &row.onedrive) else { continue };
            let Some(place) = &onedrive.place else { continue };
            let (size, mtime, quickxor, ctag) = match &onedrive.content {
                Some(ContentVersion::Hashed { size, mtime, hash, ctag, .. }) => (*size, *mtime, hash.clone(), ctag.clone()),
                _ => (0, 0, None, None),
            };
            let kind = if row.kind == ItemKind::Directory { Kind::Folder } else { Kind::File };
            listed.push(Change::Upsert(Row {
                id: reference.item_id.as_str().to_owned(),
                parent_id: Some(onedrive_id(place.parent_local_item_id).expect("OneDrive has the directory it is in")),
                name: place.name.to_str().unwrap().to_owned(),
                kind,
                size,
                mtime,
                etag: row.onedrive_etag.clone(),
                ctag,
                quickxor,
                mime: None,
                placement: Placement::Placed,
            }));
        }
        listed
    }

    /// The store is lost and made again, as at a change of its schema: no row but the
    /// root's. What OneDrive had, for the listing that follows the check.
    pub(super) fn rebuild_store(&mut self) -> Vec<Change> {
        let listed = self.list_onedrive();
        self.store = store_with_listed_root();
        listed
    }

    /// A listing of changes that says `changes` in one page and ends.
    pub(super) fn write_listing(&mut self, changes: &[Change]) {
        let listing = self.store.item_store().begin_delta_listing().unwrap();
        let outcome = self.store.item_store().write_listed_changes(listing.id, changes, listing.began_at_count, &ListingLink::DeltaLink("link".into())).unwrap();
        assert!(outcome.is_link_written(), "the listing left something to read again: {outcome:?}");
    }

    /// The plan made from every row there is: the set is closed, nothing is left to ask
    /// for. A read-write folder whose user confirmed every removal, so that no guard holds
    /// a removal back from being seen here.
    pub(super) fn plan(&mut self) -> SyncPlan {
        let rows = self.rows();
        let items = self.store.item_store();
        let directories = || rows.iter().filter(|row| row.kind == ItemKind::Directory);
        let input = SyncPlanInput {
            folder_visibility: rows.iter().filter(|row| row.onedrive.is_some()).map(|row| (row.local_item_id, items.find_folder_visibility(row.local_item_id).unwrap())).collect(),
            ignored_items: rows.iter().filter(|row| items.is_ignored(row.local_item_id).unwrap()).map(|row| row.local_item_id).collect(),
            name_refusals: Default::default(),
            items_open_for_writing: Default::default(),
            quiet_until: Default::default(),
            items_with_running_step: Default::default(),
            destinations_of_running_steps: Default::default(),
            pinned_items: Default::default(),
            is_listing_under_way: items.find_listing_under_way().unwrap().is_some(),
            is_onedrive_full: false,
            largest_file_size: 250 << 30,
            computer_name: "pc".to_owned(),
            places_taken_in_onedrive: Default::default(),
            items_left_by_uploads: Default::default(),
            temporary_names: Default::default(),
            own_upload_contents: Default::default(),
            now: 0,
            next_tries: Default::default(),
            sending_refusals: Default::default(),
            items_left_by_start_up_check: Default::default(),
            onedrive_item_count_below: directories().map(|row| (row.local_item_id, items.list_below_in_onedrive(row.local_item_id).unwrap().len() as u64)).collect(),
            shown_item_count_in: directories().map(|row| (row.local_item_id, items.list_children(row.local_item_id).unwrap().len() as u64)).collect(),
            items: rows.clone(),
        };
        make_sync_plan(&input)
    }

    /// The promise of the check, asked of the plan: nothing was removed or moved through
    /// the mount, so no step towards OneDrive is a removal or a move.
    pub(super) fn assert_plan_sends_no_removal_or_move(&mut self, said: &str) {
        let plan = self.plan();
        for step in &plan.steps {
            let is_removal_or_move = matches!(step, SyncStep::OneDrive(OneDriveStep::SendMove { .. } | OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. }));
            assert!(!is_removal_or_move, "{said}: the plan sends what nobody did through the mount: {step:?}");
        }
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
                let is_set_aside = directory.as_os_str().is_empty() && entry.file_name().to_string_lossy().starts_with(crate::folder::disk::ASIDE_ENTRY_PREFIX);
                if crate::folder::disk::daemon_owned(&entry.file_name()) || is_set_aside {
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
        self.assert_rows_say_the_disk_but(&[]);
    }

    /// [`assert_rows_say_the_disk`](Self::assert_rows_say_the_disk), but for the paths the
    /// check said it left: what is at those has no row, or a row that is as it was.
    pub(super) fn assert_rows_say_the_disk_but(&mut self, left: &[PathBuf]) {
        for rel in self.walk_disk() {
            if left.iter().any(|left| rel.starts_with(left)) {
                continue;
            }
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
            // A row the check left as it is, and what is below it, says what it said.
            if left.iter().any(|left| path.starts_with(left)) {
                continue;
            }
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
        self.assert_settled_but(&[]);
    }

    /// [`assert_settled`](Self::assert_settled), but for the paths the check said it left:
    /// a second run leaves the same.
    pub(super) fn assert_settled_but(&mut self, left: &[PathBuf]) {
        self.assert_rows_say_the_disk_but(left);
        let before = self.rows();
        let again = self.check();
        assert!(found(&again).iter().all(|(finding, _)| *finding == DiskFinding::NothingChanged), "a second run found {:?}", found(&again));
        // A path may be left for two reasons: it is one path.
        let mut left_again: Vec<PathBuf> = again.unresolved().iter().map(|left| left.path.clone()).collect();
        left_again.dedup();
        assert_eq!(left_again, left);
        assert_eq!(self.rows(), before, "a second run wrote");
    }
}

pub(super) fn changed_here(size: u64, change_count: u64) -> Option<ContentVersion> {
    Some(ContentVersion::ChangedHere { size, mtime: MTIME, mtime_nsec: 0, change_count })
}

pub(super) fn content(row: &Item) -> Option<ContentVersion> {
    row.local.as_ref().and_then(|local| local.content.clone())
}

/// A folder with one of each thing the rows can say of a path, each as the disk has it.
pub(super) fn folder_of_each() -> Folder {
    let mut f = folder();
    f.onedrive_directory("docs", "D");
    f.copy("docs/a.txt", b"aaa", "A");
    f.not_downloaded("docs/remote.bin", 9, "R");
    f.own_directory("mine");
    f.own_file("mine/own.txt", b"own");
    f.own_file("top.txt", b"top");
    f
}

/// What the check left, by path.
pub(super) fn left(findings: &DiskFindings) -> Vec<PathBuf> {
    let mut left: Vec<PathBuf> = findings.unresolved().iter().map(|left| left.path.clone()).collect();
    left.dedup();
    left
}

/// The rows of the items OneDrive has, each with its binding: what no check may make less.
pub(super) fn bindings(rows: &[Item]) -> Vec<(LocalItemId, OneDriveItemReference)> {
    rows.iter().filter_map(|row| Some((row.local_item_id, row.onedrive_item.clone()?))).collect()
}

/// An item of OneDrive is at most not shown any more: after a check every binding there
/// was is still its item's, nothing of such an item is to be sent as removed or moved, and
/// an item that is still shown is where it was.
pub(super) fn assert_no_item_of_onedrive_is_removed_or_moved(before: &[Item], after: &[Item]) {
    for row in before.iter().filter(|row| row.onedrive_item.is_some()) {
        let now = after.iter().find(|now| now.local_item_id == row.local_item_id).unwrap_or_else(|| panic!("the row of an item of OneDrive is gone: {row:?}"));
        assert_eq!(now.onedrive_item, row.onedrive_item, "a binding changed");
        assert_eq!(now.onedrive, row.onedrive, "what OneDrive has changed");
        let place = |row: &Item| row.local.as_ref().and_then(|local| local.place.clone());
        match &now.local {
            Some(_) => {
                assert_eq!(place(now), place(row), "an item of OneDrive was moved: {row:?}");
                assert_eq!(now.synced_place, row.synced_place, "{row:?}");
            }
            // Not shown any more: nothing is agreed on, so nothing is to be sent — unless
            // it was not shown before either.
            None if row.local.is_some() => assert_eq!((now.synced_place.clone(), now.synced_content.clone(), now.unsent_since), (None, None, None), "a removal is to be sent: {now:?}"),
            None => assert_eq!(now, row),
        }
    }
}

#[test]
fn a_folder_that_agrees_with_its_rows_has_nothing_changed() {
    let mut f = folder_of_each();
    let before = f.rows();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!(findings.unresolved(), []);
    assert_eq!(findings.list_held_items(), []);
    assert!(!findings.has_unread_part());
    assert_eq!(f.rows(), before);
    f.assert_rows_say_the_disk();
}

#[test]
fn the_names_the_daemon_uses_for_itself_are_nothing_of_the_folder() {
    let mut f = folder_of_each();
    f.make_directory(crate::folder::disk::HOLDING);
    f.write(format!("{}/in.txt", crate::folder::disk::HOLDING), b"x");
    f.write(format!("{}9", crate::folder::disk::ASIDE_ENTRY_PREFIX), b"set aside");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
}
