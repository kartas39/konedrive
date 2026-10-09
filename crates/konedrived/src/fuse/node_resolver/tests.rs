use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::outbox::{Detection, OutboxKind};

use crate::fuse::node_table::NodeId;
use super::NodeResolver;
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::directory_contents::tests::{directory_contents, of};
use crate::fuse::directory_contents::{DirectoryContents, DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};

/// The node resolver of the drive the directory contents' tests have (`docs` with `c.txt`, and
/// `a.txt` and `b.txt` in the root), over a backing directory that holds nothing.
fn node_resolver(detections: Vec<Detection>) -> (tempfile::TempDir, Arc<DirectoryContents>, NodeResolver) {
    let (dir, contents) = directory_contents(detections);
    let contents = Arc::new(contents);
    let backing_directory = Arc::new(BackingDirectory::open(dir.path()).unwrap());
    (dir, Arc::clone(&contents), NodeResolver::new(contents, backing_directory))
}

/// The entry of the OneDrive id `id`, at the path the store has it.
fn in_onedrive(contents: &DirectoryContents, id: &str) -> DirectoryEntry {
    contents.describe(contents.find_row(id).unwrap(), &contents.find_path(id).unwrap())
}

/// The local-only entry `name` in the root.
fn local_only(contents: &DirectoryContents, name: &str) -> DirectoryEntry {
    DirectoryEntry::from_local_only(contents.find_local_only(&contents.find_root_directory(), name).unwrap())
}

fn id(found: Option<(DirectoryEntry, PathBuf)>) -> Option<(Option<String>, PathBuf)> {
    found.map(|(entry, rel)| (entry.onedrive_id().map(str::to_owned), rel))
}

#[test]
fn an_entry_gets_one_node_id_and_keeps_it_when_it_is_renamed() {
    let (_dir, contents, node_resolver) = node_resolver(vec![of(OutboxKind::Move, "A", "a.txt", "docs/b.txt", (Some("D"), Some("b.txt")))]);
    let docs = node_resolver.assign_node_id(&in_onedrive(&contents, "D"), NodeId::ROOT);
    let renamed = contents.find_entry_at(Path::new("docs/b.txt")).unwrap();
    let file = node_resolver.assign_node_id(&renamed, docs);
    assert!(file != docs && file != NodeId::ROOT && node_resolver.assign_node_id(&renamed, docs) == file);
    assert_eq!(id(node_resolver.find_entry(file)), Some((Some("A".into()), PathBuf::from("docs/b.txt"))));
    assert_eq!(node_resolver.find_node_id("A"), Some(file));
}

/// A node that was told another place moves to where a lookup finds it, when the mount
/// shows it there.
#[test]
fn a_node_looked_up_at_the_place_the_mount_shows_it_at_moves_there() {
    let (_dir, contents, node_resolver) = node_resolver(vec![of(OutboxKind::Move, "A", "a.txt", "docs/b.txt", (Some("D"), Some("b.txt")))]);
    let docs = node_resolver.assign_node_id(&in_onedrive(&contents, "D"), NodeId::ROOT);
    let file = node_resolver.assign_node_id(&contents.find_entry_at(Path::new("docs/b.txt")).unwrap(), docs);
    node_resolver.move_node(file, NodeId::ROOT, "a.txt");
    assert_eq!(node_resolver.assign_node_id(&contents.find_entry_at(Path::new("docs/b.txt")).unwrap(), docs), file);
    assert_eq!(id(node_resolver.find_entry(file)), Some((Some("A".into()), PathBuf::from("docs/b.txt"))));
}

/// A lookup read the name before a rename, and gives the node its id after it: the entry it
/// read is of a place the mount no longer shows the file at, and the node stays where it is.
#[test]
fn a_lookup_that_read_a_name_before_its_rename_leaves_the_node_at_its_new_place() {
    let (_dir, contents, node_resolver) = node_resolver(vec![of(OutboxKind::Move, "A", "a.txt", "docs/b.txt", (Some("D"), Some("b.txt")))]);
    let docs = node_resolver.assign_node_id(&in_onedrive(&contents, "D"), NodeId::ROOT);
    let file = node_resolver.assign_node_id(&contents.find_entry_at(Path::new("docs/b.txt")).unwrap(), docs);
    let read_before = contents.describe(contents.find_row("A").unwrap(), Path::new("a.txt"));
    assert_eq!(node_resolver.assign_node_id(&read_before, NodeId::ROOT), file);
    assert_eq!(id(node_resolver.find_entry(file)), Some((Some("A".into()), PathBuf::from("docs/b.txt"))));
    assert!(matches!(node_resolver.find_entry(file), Some((entry, _)) if entry.name == "b.txt"));
}

/// The same for a local-only entry: it is where its file handle is.
#[test]
fn a_lookup_that_read_a_local_only_name_before_its_rename_leaves_the_node_at_its_new_place() {
    let (dir, contents, node_resolver) = node_resolver(Vec::new());
    fs::write(dir.path().join("made.txt"), b"mine").unwrap();
    let read_before = local_only(&contents, "made.txt");
    let node_id = node_resolver.assign_node_id(&read_before, NodeId::ROOT);
    fs::rename(dir.path().join("made.txt"), dir.path().join("kept.txt")).unwrap();
    node_resolver.move_node(node_id, NodeId::ROOT, "kept.txt");
    assert_eq!(node_resolver.assign_node_id(&read_before, NodeId::ROOT), node_id);
    assert_eq!(node_resolver.find_entry(node_id).map(|(_, path)| path), Some(PathBuf::from("kept.txt")));
}

/// A reading of a directory gives a name the node id its entry has, and leaves a node the
/// table has where it is.
#[test]
fn a_listed_name_never_moves_the_node_it_names() {
    let (_dir, contents, node_resolver) = node_resolver(Vec::new());
    let root = contents.find_root_directory();
    let listed = contents.list_names(&root).into_iter().find(|listed_name| listed_name.name() == "a.txt").unwrap();
    let file = node_resolver.assign_node_id_of_listed_name(&listed, NodeId::ROOT);
    let docs = node_resolver.assign_node_id(&in_onedrive(&contents, "D"), NodeId::ROOT);
    node_resolver.move_node(file, docs, "b.txt");
    assert_eq!(node_resolver.assign_node_id_of_listed_name(&listed, NodeId::ROOT), file);
    assert_eq!(node_resolver.find_parent(file).map(|(parent, _)| parent), Some(docs));
}

#[test]
fn the_root_is_node_one_and_a_directory() {
    let (_dir, contents, node_resolver) = node_resolver(Vec::new());
    assert_eq!(id(node_resolver.find_entry(NodeId(1))), Some((Some("R".into()), PathBuf::new())));
    assert_eq!(node_resolver.find_directory(NodeId(1)).map(|directory| (directory.onedrive_id, directory.path)), Some((Some("R".into()), PathBuf::new())));
    let docs = node_resolver.assign_node_id(&in_onedrive(&contents, "D"), NodeId::ROOT);
    let file = node_resolver.assign_node_id(&in_onedrive(&contents, "C"), docs);
    assert!(node_resolver.find_directory(file).is_none());
    assert_eq!(node_resolver.find_parent(file).map(|(node_id, directory)| (node_id, directory.onedrive_id, directory.path)), Some((docs, Some("D".into()), PathBuf::from("docs"))));
    assert!(node_resolver.find_entry(NodeId(99)).is_none() && node_resolver.find_parent(NodeId(1)).is_none());
}

/// A directory listed as its listed names gives each name the node id its entry has.
#[test]
fn the_listed_name_of_an_entry_gets_the_node_id_of_the_entry() {
    let (dir, contents, node_resolver) = node_resolver(Vec::new());
    fs::write(dir.path().join("own.txt"), b"mine").unwrap();
    let root = contents.find_root_directory();
    for listed_name in contents.list_names(&root) {
        let node_id = node_resolver.assign_node_id_of_listed_name(&listed_name, NodeId::ROOT);
        assert_eq!(node_resolver.assign_node_id(&contents.describe_listed_name(&listed_name, &root), NodeId::ROOT), node_id, "{}", listed_name.name());
        assert!(matches!(node_resolver.find_entry(node_id), Some((entry, _)) if entry.name == listed_name.name()));
    }
}

/// The entry a node id stands for comes with its sync state, as the directory contents work
/// it out.
#[test]
fn the_entry_of_a_node_id_has_its_sync_state() {
    let (dir, contents, node_resolver) = node_resolver(Vec::new());
    let file = node_resolver.assign_node_id(&in_onedrive(&contents, "A"), NodeId::ROOT);
    assert!(matches!(node_resolver.find_entry(file), Some((DirectoryEntry { state: EntryState::File(FileSyncState::RemoteOnly(_)), .. }, _))));
    crate::fuse::directory_contents::tests::download(dir.path(), &contents, "a.txt", "A", b"whole");
    assert!(matches!(node_resolver.find_entry(file), Some((DirectoryEntry { state: EntryState::File(FileSyncState::Synced(..)), .. }, _))));
}

/// A local-only entry found by its node id is at its place: in the directory its node is in.
#[test]
fn a_local_only_entry_found_by_its_node_id_has_its_place() {
    let (dir, contents, node_resolver) = node_resolver(Vec::new());
    fs::create_dir(dir.path().join("new")).unwrap();
    fs::write(dir.path().join("new/made.txt"), b"mine").unwrap();
    let new = node_resolver.assign_node_id(&local_only(&contents, "new"), NodeId::ROOT);
    let new_directory = node_resolver.find_directory(new).unwrap();
    assert_eq!((new_directory.onedrive_id.clone(), new_directory.path.clone()), (None, PathBuf::from("new")));
    let made = node_resolver.assign_node_id(&DirectoryEntry::from_local_only(contents.find_local_only(&new_directory, "made.txt").unwrap()), new);
    let (entry, rel) = node_resolver.find_entry(made).unwrap();
    let place = &entry.local_only().unwrap().place;
    assert_eq!((rel, place.directory.onedrive_id.clone(), place.path()), (PathBuf::from("new/made.txt"), None, PathBuf::from("new/made.txt")));
    let (top, _) = node_resolver.find_entry(new).unwrap();
    assert!(matches!(&top.state, EntryState::Directory(DirectorySyncState::LocalOnly(entry)) if entry.place.directory.onedrive_id.as_deref() == Some("R")));
}

/// A file made here has its node id as a local-only entry. Once it is uploaded it is marked
/// with its OneDrive id, and its node stands for the file in OneDrive: programs that hold it
/// open go on with the same file.
#[test]
fn a_local_only_entry_that_got_into_onedrive_keeps_its_node_id() {
    let (dir, contents, node_resolver) = node_resolver(Vec::new());
    fs::write(dir.path().join("made.txt"), b"mine").unwrap();
    let node_id = node_resolver.assign_node_id(&local_only(&contents, "made.txt"), NodeId::ROOT);
    assert!(matches!(node_resolver.find_entry(node_id), Some((entry, _)) if entry.local_only().is_some()));
    xattr::set(dir.path().join("made.txt"), XATTR_ITEM_ID, b"A").unwrap();
    assert_eq!(id(node_resolver.find_entry(node_id)), Some((Some("A".into()), PathBuf::from("made.txt"))));
    assert_eq!(node_resolver.find_node_id("A"), Some(node_id));
}

/// The same, found from the name: the file of OneDrive is looked up where its local-only
/// entry was known.
#[test]
fn a_onedrive_file_looked_up_where_its_local_only_entry_was_known_gets_that_node_id() {
    let (dir, contents, node_resolver) = node_resolver(Vec::new());
    fs::write(dir.path().join("b.txt"), b"mine").unwrap();
    let node_id = node_resolver.assign_node_id(&local_only(&contents, "b.txt"), NodeId::ROOT);
    assert_eq!(node_resolver.assign_node_id(&in_onedrive(&contents, "B"), NodeId::ROOT), node_id);
    assert_eq!(id(node_resolver.find_entry(node_id)), Some((Some("B".into()), PathBuf::from("b.txt"))));
}

#[test]
fn a_node_id_whose_local_only_entry_is_gone_stands_for_nothing() {
    let (dir, contents, node_resolver) = node_resolver(Vec::new());
    fs::write(dir.path().join("made.txt"), b"mine").unwrap();
    let node_id = node_resolver.assign_node_id(&local_only(&contents, "made.txt"), NodeId::ROOT);
    // Another file under its name is another entry.
    fs::rename(dir.path().join("made.txt"), dir.path().join("kept.txt")).unwrap();
    fs::write(dir.path().join("made.txt"), b"other").unwrap();
    assert!(node_resolver.find_entry(node_id).is_none());
    fs::remove_file(dir.path().join("made.txt")).unwrap();
    assert!(node_resolver.find_entry(node_id).is_none());
}

#[test]
fn a_node_id_whose_onedrive_file_was_removed_here_stands_for_nothing() {
    let (_dir, contents, node_resolver) = node_resolver(vec![of(OutboxKind::Delete, "B", "b.txt", "b.txt", (None, None))]);
    let node_id = node_resolver.assign_node_id(&contents.describe(contents.find_row("B").unwrap(), Path::new("b.txt")), NodeId::ROOT);
    assert!(node_resolver.find_entry(node_id).is_none());
}

/// A local-only file saved over a OneDrive file: the file's node stands for that file of
/// OneDrive from then on, at its name.
#[test]
fn a_local_only_file_renamed_over_a_onedrive_files_name_takes_that_files_node() {
    let (dir, contents, node_resolver) = node_resolver(Vec::new());
    fs::write(dir.path().join("a.tmp"), b"new").unwrap();
    let entry = local_only(&contents, "a.tmp");
    let saved = entry.local_only().unwrap();
    let node_id = node_resolver.assign_node_id(&entry, NodeId::ROOT);
    fs::rename(dir.path().join("a.tmp"), dir.path().join("a.txt")).unwrap();
    node_resolver.move_node_onto_onedrive_file(node_id, NodeId::ROOT, "a.txt", &saved.on_disk.handle, "A");
    assert_eq!(node_resolver.find_node_id("A"), Some(node_id));
    assert_eq!(id(node_resolver.find_entry(node_id)), Some((Some("A".into()), PathBuf::from("a.txt"))));
}

#[test]
fn a_local_only_entry_whose_node_is_dropped_gets_a_new_node_id_if_it_is_ever_seen_again() {
    let (dir, contents, node_resolver) = node_resolver(Vec::new());
    fs::write(dir.path().join("made.txt"), b"mine").unwrap();
    let entry = local_only(&contents, "made.txt");
    let node_id = node_resolver.assign_node_id(&entry, NodeId::ROOT);
    node_resolver.drop_node(&entry.local_only().unwrap().on_disk.handle);
    assert!(node_resolver.find_entry(node_id).is_none() && node_resolver.find_parent(node_id).is_none());
    assert_ne!(node_resolver.assign_node_id(&entry, NodeId::ROOT), node_id);
}
