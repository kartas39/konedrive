use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::{Change, Kind, Placement, Row, TreeStore};

use super::{DirectoryContents, DirectoryEntry, DirectorySyncState, EntrySource, EntryState, FileSyncState};
use crate::folder::disk::HOLDING;
use crate::fuse::backing_directory::{BackingDirectory, EntryKind};
use crate::fuse::directory::Directory;

pub(in crate::fuse) fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> Row {
    Row { id: id.into(), parent_id: parent.map(str::to_owned), name: name.into(), kind, size: 1, mtime: 0, etag: Some("e".into()), ctag: Some("c".into()), quickxor: None, mime: None, placement: Placement::Placed }
}

/// A detection of the OneDrive id `id`, which the base has as `name` in the root.
pub(in crate::fuse) fn of(kind: OutboxKind, id: &str, name: &str, rel: &str, to: (Option<&str>, Option<&str>)) -> Detection {
    Detection {
        kind,
        item_id: Some(id.into()),
        object: None,
        rel: PathBuf::from(rel),
        base: Some(Base { etag: Some("e".into()), ctag: Some("c".into()), parent: Some("R".into()), name: Some(name.into()) }),
        target_parent: to.0.map(str::to_owned),
        target_name: to.1.map(str::to_owned),
        same_content: kind == OutboxKind::Move,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    }
}

/// A drive with a directory `docs` that holds `c.txt`, and two files in its root, `a.txt` and
/// `b.txt`, with `detections` waiting in the outbox.
pub(in crate::fuse) fn drive(store: &mut TreeStore, detections: Vec<Detection>) {
    let changes = [
        Change::Root(row("R", None, "root", Kind::Folder)),
        Change::Upsert(row("D", Some("R"), "docs", Kind::Folder)),
        Change::Upsert(row("A", Some("R"), "a.txt", Kind::File)),
        Change::Upsert(row("B", Some("R"), "b.txt", Kind::File)),
        Change::Upsert(row("C", Some("D"), "c.txt", Kind::File)),
    ];
    store.commit_page(&changes, "next").unwrap();
    let ops: Vec<OutboxOp> = detections.into_iter().map(OutboxOp::Record).collect();
    store.outbox_apply(&ops, 0).unwrap();
}

/// The contents of the [`drive`], with `detections` waiting in the outbox, over a backing
/// directory that holds nothing. The store is filled before the directory contents have it:
/// they read what their own connection wrote only once.
pub(in crate::fuse) fn directory_contents(detections: Vec<Detection>) -> (tempfile::TempDir, DirectoryContents) {
    let mut store = TreeStore::in_memory().unwrap();
    drive(&mut store, detections);
    let dir = tempfile::tempdir().unwrap();
    let backing_directory = Arc::new(BackingDirectory::open(dir.path()).unwrap());
    (dir, DirectoryContents::new(store, backing_directory))
}

/// The whole downloaded copy of the OneDrive file `id`, with `content`, at `rel` in the
/// backing directory `dir`.
pub(in crate::fuse) fn download(dir: &Path, contents: &DirectoryContents, rel: &str, id: &str, content: &[u8]) {
    contents.make_marked_directories(Path::new(rel).parent().unwrap()).unwrap();
    fs::write(dir.join(rel), content).unwrap();
    contents.backing_directory().mark_downloaded_copy(Path::new(rel), id).unwrap();
}

fn docs() -> Directory {
    Directory { id: Some("D".into()), rel: PathBuf::from("docs") }
}

fn names(contents: &DirectoryContents, directory: &Directory) -> Vec<String> {
    let mut names: Vec<String> = contents.list_entry_sources(directory).iter().map(|source| source.name().to_owned()).collect();
    names.sort();
    names
}

/// The OneDrive id and the name of an entry that is in OneDrive.
fn in_onedrive(entry: Option<DirectoryEntry>) -> Option<(String, String)> {
    let entry = entry?;
    Some((entry.onedrive_id()?.to_owned(), entry.name))
}

fn pair(id: &str, name: &str) -> Option<(String, String)> {
    Some((id.to_owned(), name.to_owned()))
}

/// The state of the file `name` in the root.
fn file_state(contents: &DirectoryContents, name: &str) -> FileSyncState {
    match contents.find_entry(&contents.root_directory(), name).unwrap().state {
        EntryState::File(file) => file,
        EntryState::Directory(directory) => panic!("{name} is a directory: {directory:?}"),
    }
}

/// The state of the directory `name` in the root.
fn directory_state(contents: &DirectoryContents, name: &str) -> DirectorySyncState {
    match contents.find_entry(&contents.root_directory(), name).unwrap().state {
        EntryState::Directory(directory) => directory,
        EntryState::File(file) => panic!("{name} is a file: {file:?}"),
    }
}

#[test]
fn a_onedrive_file_with_nothing_on_disk_is_remote_only() {
    let (dir, contents) = directory_contents(Vec::new());
    assert!(matches!(file_state(&contents, "a.txt"), FileSyncState::RemoteOnly(row) if row.id == "A"));
    // A file of the user's own at its path is not its downloaded copy.
    fs::write(dir.path().join("a.txt"), b"mine").unwrap();
    assert!(matches!(file_state(&contents, "a.txt"), FileSyncState::RemoteOnly(_)));
}

#[test]
fn a_onedrive_file_with_its_downloaded_copy_is_synced() {
    let (dir, contents) = directory_contents(Vec::new());
    download(dir.path(), &contents, "a.txt", "A", b"whole");
    assert!(matches!(file_state(&contents, "a.txt"), FileSyncState::Synced(row, copy) if row.id == "A" && copy.size == 5 && copy.kind == EntryKind::File));
}

#[test]
fn a_onedrive_file_whose_copy_has_an_unsent_change_is_modified_here() {
    let (dir, contents) = directory_contents(vec![of(OutboxKind::Update, "A", "a.txt", "a.txt", (Some("R"), Some("a.txt")))]);
    download(dir.path(), &contents, "a.txt", "A", b"changed");
    assert!(matches!(file_state(&contents, "a.txt"), FileSyncState::ModifiedHere(row, copy) if row.id == "A" && copy.size == 7));
    // The other files are not.
    download(dir.path(), &contents, "b.txt", "B", b"whole");
    assert!(matches!(file_state(&contents, "b.txt"), FileSyncState::Synced(..)));
}

/// An unsent change of content with no copy on disk shows nothing of the user's own.
#[test]
fn a_onedrive_file_with_an_unsent_change_and_no_copy_is_remote_only() {
    let (_dir, contents) = directory_contents(vec![of(OutboxKind::Update, "A", "a.txt", "a.txt", (Some("R"), Some("a.txt")))]);
    assert!(matches!(file_state(&contents, "a.txt"), FileSyncState::RemoteOnly(_)));
}

#[test]
fn a_file_made_here_is_local_only() {
    let (dir, contents) = directory_contents(Vec::new());
    fs::write(dir.path().join("own.txt"), b"mine").unwrap();
    let FileSyncState::LocalOnly(entry) = file_state(&contents, "own.txt") else { panic!("own.txt is in OneDrive") };
    assert_eq!((entry.on_disk.metadata.kind, entry.on_disk.metadata.size), (EntryKind::File, 4));
    assert_eq!((entry.place.above.as_deref(), entry.place.rel()), (Some("R"), PathBuf::from("own.txt")));
}

#[test]
fn a_onedrive_directory_is_synced_whether_or_not_the_disk_has_it() {
    let (dir, contents) = directory_contents(Vec::new());
    assert!(matches!(directory_state(&contents, "docs"), DirectorySyncState::Synced(row) if row.id == "D"));
    fs::create_dir(dir.path().join("docs")).unwrap();
    assert!(matches!(directory_state(&contents, "docs"), DirectorySyncState::Synced(row) if row.id == "D"));
}

#[test]
fn a_directory_made_here_is_local_only() {
    let (dir, contents) = directory_contents(Vec::new());
    fs::create_dir(dir.path().join("new")).unwrap();
    let DirectorySyncState::LocalOnly(entry) = directory_state(&contents, "new") else { panic!("new is in OneDrive") };
    assert_eq!((entry.on_disk.metadata.kind, entry.place.name.as_str()), (EntryKind::Directory, "new"));
}

/// The states of the files below a row come with the walk: what is downloaded, and what is
/// modified here.
#[test]
fn the_files_below_a_row_come_with_their_sync_states() {
    let (dir, contents) = directory_contents(vec![of(OutboxKind::Update, "C", "c.txt", "docs/c.txt", (Some("D"), Some("c.txt")))]);
    download(dir.path(), &contents, "docs/c.txt", "C", b"changed");
    download(dir.path(), &contents, "a.txt", "A", b"whole");
    let mut states: Vec<(PathBuf, &str)> = contents
        .files_below(&contents.root_row())
        .into_iter()
        .map(|(file, at)| {
            let state = match file {
                FileSyncState::RemoteOnly(_) => "remote only",
                FileSyncState::Synced(..) => "synced",
                FileSyncState::ModifiedHere(..) => "modified here",
                FileSyncState::LocalOnly(_) => "local only",
            };
            (at, state)
        })
        .collect();
    states.sort();
    assert_eq!(states, [(PathBuf::from("a.txt"), "synced"), (PathBuf::from("b.txt"), "remote only"), (PathBuf::from("docs/c.txt"), "modified here")]);
}

#[test]
fn a_onedrive_file_is_shown_where_the_store_has_it() {
    let (_dir, contents) = directory_contents(Vec::new());
    assert_eq!(in_onedrive(contents.find_entry(&contents.root_directory(), "a.txt")), pair("A", "a.txt"));
    assert_eq!(in_onedrive(contents.find_entry(&docs(), "c.txt")), pair("C", "c.txt"));
    assert_eq!(names(&contents, &contents.root_directory()), ["a.txt", "b.txt", "docs"]);
    assert_eq!(names(&contents, &docs()), ["c.txt"]);
    assert_eq!(contents.find_path("C").as_deref(), Some(Path::new("docs/c.txt")));
    assert_eq!(in_onedrive(contents.find_entry_at(Path::new("docs/c.txt"))), pair("C", "c.txt"));
    assert!(contents.find_entry(&contents.root_directory(), "none.txt").is_none() && contents.find_entry_at(Path::new("a.txt/below")).is_none());
}

#[test]
fn the_folder_itself_is_the_drives_root() {
    let (_dir, contents) = directory_contents(Vec::new());
    assert_eq!(contents.find_root_id().as_deref(), Some("R"));
    assert_eq!(in_onedrive(contents.find_entry_at(Path::new(""))), pair("R", ""));
    assert!(matches!(contents.root_entry().state, EntryState::Directory(DirectorySyncState::Synced(_))));
    assert_eq!(contents.find_path("R").as_deref(), Some(Path::new("")));
    assert_eq!(contents.find_row("A").and_then(|a| contents.find_parent_row(&a)).map(|parent| parent.id).as_deref(), Some("R"));
    assert!(contents.find_parent_row(&contents.root_row()).is_none());
}

/// Until its move is sent the store has the file where it was; the mount shows it where the
/// user put it, and nowhere else.
#[test]
fn a_moved_file_is_shown_only_at_its_new_path() {
    let (_dir, contents) = directory_contents(vec![of(OutboxKind::Move, "A", "a.txt", "docs/new.txt", (Some("D"), Some("new.txt")))]);
    assert!(contents.find_entry(&contents.root_directory(), "a.txt").is_none());
    assert_eq!(in_onedrive(contents.find_entry(&docs(), "new.txt")), pair("A", "new.txt"));
    assert_eq!(names(&contents, &contents.root_directory()), ["b.txt", "docs"]);
    assert_eq!(names(&contents, &docs()), ["c.txt", "new.txt"]);
    assert_eq!(contents.find_path("A").as_deref(), Some(Path::new("docs/new.txt")));
    assert!(contents.find_entry_at(Path::new("a.txt")).is_none());
}

/// What the user moved to a name is what the name is, though OneDrive has another file there.
#[test]
fn a_file_moved_over_another_files_name_is_what_the_name_shows() {
    let (_dir, contents) = directory_contents(vec![of(OutboxKind::Move, "A", "a.txt", "b.txt", (Some("R"), Some("b.txt")))]);
    assert_eq!(in_onedrive(contents.find_entry(&contents.root_directory(), "b.txt")), pair("A", "b.txt"));
    assert_eq!(names(&contents, &contents.root_directory()), ["b.txt", "docs"]);
}

#[test]
fn a_file_removed_here_is_shown_nowhere() {
    let (_dir, contents) = directory_contents(vec![of(OutboxKind::Delete, "B", "b.txt", "b.txt", (None, None))]);
    assert!(contents.find_entry(&contents.root_directory(), "b.txt").is_none());
    assert_eq!(names(&contents, &contents.root_directory()), ["a.txt", "docs"]);
    assert!(contents.find_path("B").is_none() && contents.find_entry_at(Path::new("b.txt")).is_none());
}

/// A file the backing directory holds at the name of a OneDrive file — its downloaded copy,
/// or anything else — is not a second name.
#[test]
fn a_name_onedrive_and_the_disk_both_have_is_onedrives() {
    let (dir, contents) = directory_contents(Vec::new());
    fs::write(dir.path().join("a.txt"), b"mine").unwrap();
    assert_eq!(in_onedrive(contents.find_entry(&contents.root_directory(), "a.txt")), pair("A", "a.txt"));
    assert_eq!(names(&contents, &contents.root_directory()), ["a.txt", "b.txt", "docs"]);
}

#[test]
fn a_local_only_entry_is_listed_where_it_lies() {
    let (dir, contents) = directory_contents(Vec::new());
    fs::create_dir(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/own.txt"), b"mine").unwrap();
    fs::create_dir(dir.path().join("new")).unwrap();
    let own = contents.find_entry(&docs(), "own.txt").unwrap();
    assert!(own.name == "own.txt" && matches!(&own.state, EntryState::File(FileSyncState::LocalOnly(entry)) if entry.place.above.as_deref() == Some("D")));
    assert_eq!(names(&contents, &docs()), ["c.txt", "own.txt"]);
    assert_eq!(names(&contents, &contents.root_directory()), ["a.txt", "b.txt", "docs", "new"]);
    assert!(matches!(contents.find_entry_at(Path::new("new")).unwrap().state, EntryState::Directory(DirectorySyncState::LocalOnly(_))));
}

/// The daemon's own names, a symbolic link, and a file that is marked with a OneDrive id the
/// store has are none of them the user's own.
#[test]
fn what_is_the_daemons_or_onedrives_is_not_a_local_only_entry() {
    let (dir, contents) = directory_contents(Vec::new());
    let root = contents.root_directory();
    fs::create_dir(dir.path().join(HOLDING)).unwrap();
    std::os::unix::fs::symlink("a.txt", dir.path().join("link")).unwrap();
    fs::write(dir.path().join("copy.txt"), b"a").unwrap();
    xattr::set(dir.path().join("copy.txt"), XATTR_ITEM_ID, b"A").unwrap();
    fs::write(dir.path().join("sent.txt"), b"a").unwrap();
    xattr::set(dir.path().join("sent.txt"), XATTR_ITEM_ID, b"unknown").unwrap();
    assert!(contents.find_local_only(&root, HOLDING).is_none() && contents.find_local_only(&root, "link").is_none() && contents.find_local_only(&root, "copy.txt").is_none());
    // An id the store does not have yet: its upload is being committed.
    assert!(contents.find_local_only(&root, "sent.txt").is_some());
    assert_eq!(names(&contents, &root), ["a.txt", "b.txt", "docs", "sent.txt"]);
}

/// A directory is listed from the store's rows alone: what the backing directory holds of a
/// OneDrive file changes nothing of what is listed, and the sync state comes only once the
/// entry is made from its source.
#[test]
fn a_directory_of_onedrive_files_is_listed_from_the_stores_rows_whatever_the_disk_holds() {
    let (dir, contents) = directory_contents(Vec::new());
    let root = contents.root_directory();
    let listed = |contents: &DirectoryContents| {
        let mut sources = contents.list_entry_sources(&root);
        sources.sort_by(|a, b| a.name().cmp(b.name()));
        sources
    };
    let rows = [contents.find_row("A").unwrap(), contents.find_row("B").unwrap(), contents.find_row("D").unwrap()].map(EntrySource::InOneDrive);
    assert_eq!(listed(&contents), rows);
    download(dir.path(), &contents, "a.txt", "A", b"whole");
    assert_eq!(listed(&contents), rows);
    assert_eq!(listed(&contents).iter().map(EntrySource::kind).collect::<Vec<_>>(), [EntryKind::File, EntryKind::File, EntryKind::Directory]);
    // The entry made from a source has the state the disk gives it.
    let described = contents.describe_source(&listed(&contents)[0], &root);
    assert!(matches!(described.state, EntryState::File(FileSyncState::Synced(row, copy)) if row.id == "A" && copy.size == 5));
}

/// A local-only entry is listed as the backing directory has it, and its entry is that.
#[test]
fn a_listed_local_only_entry_carries_what_the_disk_says_of_it() {
    let (dir, contents) = directory_contents(Vec::new());
    fs::write(dir.path().join("own.txt"), b"mine").unwrap();
    let root = contents.root_directory();
    let sources = contents.list_entry_sources(&root);
    let own = sources.iter().find(|source| source.name() == "own.txt").unwrap();
    assert!(matches!(own, EntrySource::LocalOnly(entry) if entry.on_disk.metadata.size == 4) && own.kind() == EntryKind::File);
    assert!(matches!(contents.describe_source(own, &root).state, EntryState::File(FileSyncState::LocalOnly(_))));
}

#[test]
fn a_file_below_a_moved_directory_is_found_through_the_directorys_new_path() {
    let (_dir, contents) = directory_contents(vec![of(OutboxKind::Move, "D", "docs", "papers", (Some("R"), Some("papers")))]);
    assert_eq!(contents.find_path("C").as_deref(), Some(Path::new("papers/c.txt")));
    assert_eq!(in_onedrive(contents.find_entry_at(Path::new("papers/c.txt"))), pair("C", "c.txt"));
    assert!(contents.find_entry_at(Path::new("docs/c.txt")).is_none());
}

/// A directory moved into a directory that is not in OneDrive yet is found by that
/// directory's path.
#[test]
fn a_file_below_a_directory_moved_into_a_local_only_directory_is_found_below_it() {
    let (dir, contents) = directory_contents(vec![of(OutboxKind::Move, "D", "docs", "new/docs", (None, Some("docs")))]);
    fs::create_dir(dir.path().join("new")).unwrap();
    assert_eq!(contents.find_path("C").as_deref(), Some(Path::new("new/docs/c.txt")));
    assert_eq!(in_onedrive(contents.find_entry_at(Path::new("new/docs/c.txt"))), pair("C", "c.txt"));
}

#[test]
fn the_files_below_a_row_come_with_their_paths() {
    let (_dir, contents) = directory_contents(vec![of(OutboxKind::Delete, "B", "b.txt", "b.txt", (None, None))]);
    let below = |id: &str| {
        let mut files: Vec<(String, PathBuf)> = contents.files_below(&contents.find_row(id).unwrap()).into_iter().filter_map(|(file, at)| Some((file.row()?.id.clone(), at))).collect();
        files.sort();
        files
    };
    assert_eq!(below("D"), [("C".to_owned(), PathBuf::from("docs/c.txt"))]);
    assert_eq!(below("A"), [("A".to_owned(), PathBuf::from("a.txt"))]);
    // The file removed here is not in the folder.
    assert_eq!(below("R"), [("A".to_owned(), PathBuf::from("a.txt")), ("C".to_owned(), PathBuf::from("docs/c.txt"))]);
}

#[test]
fn the_directories_down_to_a_path_are_marked_with_the_ids_the_mount_shows_there() {
    let (_dir, contents) = directory_contents(vec![of(OutboxKind::Move, "D", "docs", "papers", (Some("R"), Some("papers")))]);
    contents.make_marked_directories(Path::new("papers/own")).unwrap();
    assert_eq!(contents.backing_directory().read_onedrive_id(Path::new("papers")).as_deref(), Some("D"));
    assert_eq!(contents.backing_directory().read_onedrive_id(Path::new("papers/own")), None);
}

/// A file made where the user just removed a OneDrive file is that file's new content: the
/// removal not sent yet says which file.
#[test]
fn the_onedrive_file_that_was_just_removed_from_a_path_is_found() {
    let (_dir, contents) = directory_contents(vec![of(OutboxKind::Delete, "B", "b.txt", "b.txt", (None, None)), of(OutboxKind::Delete, "D", "docs", "docs", (None, None))]);
    assert_eq!(contents.find_file_removed_at(Path::new("b.txt")).map(|row| row.id).as_deref(), Some("B"));
    // A directory is not a file, and a path nothing was removed from has none.
    assert!(contents.find_file_removed_at(Path::new("docs")).is_none() && contents.find_file_removed_at(Path::new("a.txt")).is_none());
}
