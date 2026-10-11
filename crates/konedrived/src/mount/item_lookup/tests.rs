use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{mpsc, Barrier};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileType, INodeNo};
use konedrive_tree::{ContentVersion, ExclusionReason, ItemPlace, LocalVersionChange, PlanFault, Reason};

use super::*;
use crate::folder::disk::{make_aside_entry_name, TEMPORARY_NAME_PREFIX};
use crate::sync_parts::running_steps::{RunningStep, StepClass};
use crate::mount::item_attributes::tests::shown_item;
use crate::mount::item_download_states::{DownloadAsker, DownloadKey};
use crate::mount::item_fixture::{ItemFixture, ONEDRIVE_MTIME, START_TIME};

const ROOT: LocalItemId = LocalItemId::ROOT;

/// The lookup over the fixture's leaves.
fn lookup(fixture: &ItemFixture) -> Arc<ItemLookup> {
    Arc::new(ItemLookup::new(
        Arc::clone(&fixture.read_connections),
        Arc::clone(&fixture.place_changes),
        Arc::clone(&fixture.item_handles),
        Arc::clone(&fixture.download_states),
        Arc::clone(&fixture.access_mode_switch),
        Arc::clone(&fixture.upload_rules),
        Arc::clone(&fixture.running_steps),
        Arc::clone(&fixture.published_plan),
        Arc::clone(&fixture.backing_directory),
        START_TIME,
    ))
}

/// Asks `lookup` on the fixture's mount-like thread, as a thread of the mount asks.
fn ask<T: Send + 'static>(fixture: &ItemFixture, lookup: &Arc<ItemLookup>, question: impl FnOnce(&ItemLookup) -> T + Send + 'static) -> T {
    let lookup = Arc::clone(lookup);
    fixture.run_on_mount_like_thread(move || question(&lookup))
}

fn find_entry(fixture: &ItemFixture, lookup: &Arc<ItemLookup>, directory: LocalItemId, name: &str) -> Result<ShownEntry, Errno> {
    let name = OsString::from(name);
    ask(fixture, lookup, move |lookup| lookup.find_entry(directory, &name))
}

fn find_attributes(fixture: &ItemFixture, lookup: &Arc<ItemLookup>, node: LocalItemId, fh: Option<Fh>) -> Result<FileAttr, Errno> {
    ask(fixture, lookup, move |lookup| lookup.find_attributes(node, fh))
}

fn find_xattr(fixture: &ItemFixture, lookup: &Arc<ItemLookup>, node: LocalItemId, name: &'static str) -> Result<String, Errno> {
    ask(fixture, lookup, move |lookup| lookup.find_xattr(node, OsStr::new(name))).map(|value| String::from_utf8(value).unwrap())
}

fn list_names(fixture: &ItemFixture, lookup: &Arc<ItemLookup>, directory: LocalItemId) -> Result<Vec<OsString>, Errno> {
    let listed = ask(fixture, lookup, move |lookup| lookup.list_entries(directory))?;
    Ok(listed.into_iter().map(|item| item.local.unwrap().place.unwrap().name).collect())
}

/// Writes the row of a file made here at `name` in `directory`, as a make of the mount
/// does, and nothing on disk.
fn make_row(fixture: &ItemFixture, directory: LocalItemId, name: OsString, size: u64) -> LocalItemId {
    let content = Some(ContentVersion::ChangedHere { size, mtime: START_TIME, mtime_nsec: 0, change_count: 1 });
    let place = ItemPlace { parent_local_item_id: directory, name };
    fixture.write_rows(move |store| store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place, content, is_name_ignored: false, replaced: None }))
}

fn at(seconds: i64) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds as u64)
}

fn errno_of<T>(answer: Result<T, Errno>) -> Option<Errno> {
    answer.err()
}

#[test]
fn a_name_is_looked_up_by_one_read_of_the_local_place() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let docs = fixture.show_onedrive_directory("docs");
    let file = fixture.show_onedrive_file("docs/a.txt", b"hello");
    let found = find_entry(&fixture, &lookup, ROOT, "docs").unwrap();
    assert_eq!((found.item.local_item_id, found.attributes.kind, found.attributes.ino), (docs.local_item_id, FileType::Directory, INodeNo(docs.local_item_id.as_i64() as u64)));
    let found = find_entry(&fixture, &lookup, docs.local_item_id, "a.txt").unwrap();
    assert_eq!(found.item, file, "the row as the rows have it");
    assert_eq!((found.attributes.ino, found.attributes.kind, found.attributes.size, found.attributes.mtime, found.attributes.blocks), (INodeNo(file.local_item_id.as_i64() as u64), FileType::RegularFile, 5, at(ONEDRIVE_MTIME), 0));
    assert_eq!(errno_of(find_entry(&fixture, &lookup, docs.local_item_id, "missing")), Some(Errno::ENOENT));
    assert_eq!(errno_of(find_entry(&fixture, &lookup, ROOT, "a.txt")), Some(Errno::ENOENT), "a name is looked up in its directory alone");
    assert_eq!(errno_of(find_entry(&fixture, &lookup, docs.local_item_id, "A.TXT")), Some(Errno::ENOENT), "and as it was given");
    // The local place alone says where a name is: an item moved here and not sent, and
    // one OneDrive has that the folder does not show.
    fixture.move_here(file.local_item_id, "b.txt");
    assert_eq!(errno_of(find_entry(&fixture, &lookup, docs.local_item_id, "a.txt")), Some(Errno::ENOENT));
    assert_eq!(find_entry(&fixture, &lookup, ROOT, "b.txt").unwrap().item.local_item_id, file.local_item_id);
    fixture.list_onedrive_file("docs/listed.txt", b"new in OneDrive");
    assert_eq!(errno_of(find_entry(&fixture, &lookup, docs.local_item_id, "listed.txt")), Some(Errno::ENOENT));
    // A read-only folder shows no write bit.
    assert_eq!(find_entry(&fixture, &lookup, ROOT, "b.txt").unwrap().attributes.perm, 0o644);
    fixture.access_mode_switch.switch_to_read_only();
    assert_eq!(find_entry(&fixture, &lookup, ROOT, "b.txt").unwrap().attributes.perm, 0o444);
}

#[test]
fn a_name_that_is_not_utf8_is_looked_up_as_it_was_given() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let name = OsString::from_vec(b"caf\xe9.txt".to_vec());
    assert!(name.to_str().is_none());
    let made = make_row(&fixture, ROOT, name.clone(), 3);
    fs::write(fixture.folder().join(&name), b"abc").unwrap();
    let asked = name.clone();
    let found = ask(&fixture, &lookup, move |lookup| lookup.find_entry(ROOT, &asked)).unwrap();
    assert_eq!((found.item.local_item_id, found.attributes.size), (made, 3));
    assert!(found.attributes.blocks > 0, "its backing file is found by the same bytes");
    assert_eq!(list_names(&fixture, &lookup, ROOT).unwrap(), [name], "and listed by them");
    // Other bytes are another name.
    let other = OsString::from_vec(b"caf\xe8.txt".to_vec());
    assert_eq!(errno_of(ask(&fixture, &lookup, move |lookup| lookup.find_entry(ROOT, &other))), Some(Errno::ENOENT));
    // No attribute has a name that is not UTF-8.
    let odd_attribute = OsString::from_vec(b"user.\xff".to_vec());
    assert_eq!(errno_of(ask(&fixture, &lookup, move |lookup| lookup.find_xattr(made, &odd_attribute))), Some(Errno::ENODATA));
}

/// Every reading request by `node` is stale.
fn assert_node_is_stale(fixture: &ItemFixture, lookup: &Arc<ItemLookup>, node: LocalItemId, expected_row: NodeRow) {
    assert_eq!(ask(fixture, lookup, move |lookup| lookup.find_node_row(node)), Ok(expected_row));
    assert_eq!(errno_of(ask(fixture, lookup, move |lookup| lookup.find_shown_item(node))), Some(Errno::ESTALE));
    assert_eq!(errno_of(find_attributes(fixture, lookup, node, None)), Some(Errno::ESTALE));
    assert_eq!(errno_of(find_entry(fixture, lookup, node, "name")), Some(Errno::ESTALE), "a lookup in it");
    assert_eq!(errno_of(list_names(fixture, lookup, node)), Some(Errno::ESTALE), "a reading of it");
    assert_eq!(errno_of(find_xattr(fixture, lookup, node, XATTR_STATE)), Some(Errno::ESTALE));
    assert_eq!(errno_of(ask(fixture, lookup, move |lookup| lookup.list_xattr_names(node))), Some(Errno::ESTALE));
}

#[test]
fn a_node_whose_row_is_gone_is_stale() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let own = fixture.make_own_file("own.txt", b"abc");
    assert!(find_attributes(&fixture, &lookup, own.local_item_id, None).is_ok());
    assert_eq!(fixture.remove_here(own.local_item_id), None, "an item OneDrive never had leaves no row");
    assert_node_is_stale(&fixture, &lookup, own.local_item_id, NodeRow::Gone);
    assert_node_is_stale(&fixture, &lookup, LocalItemId::new(987_654), NodeRow::Gone);
}

#[test]
fn a_node_whose_row_has_no_local_version_is_stale() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    // Removed here, the removal not sent: the row stays for it.
    let file = fixture.show_onedrive_file("a.txt", b"hello");
    assert!(fixture.remove_here(file.local_item_id).is_some());
    assert_node_is_stale(&fixture, &lookup, file.local_item_id, NodeRow::NotShown);
    // An item of OneDrive the folder does not show.
    let listed = fixture.list_onedrive_file("listed.txt", b"new in OneDrive");
    assert_node_is_stale(&fixture, &lookup, listed.local_item_id, NodeRow::NotShown);
    let listed_directory = fixture.list_onedrive_directory("listed");
    assert_node_is_stale(&fixture, &lookup, listed_directory.local_item_id, NodeRow::NotShown);
}

#[test]
fn each_case_of_a_node_row_has_its_errno() {
    let (file, directory) = (shown_item(7, ItemKind::File, None), shown_item(8, ItemKind::Directory, None));
    assert_eq!(decide_node_errno(&NodeRow::Shown(file.clone())), None);
    assert_eq!(decide_node_errno(&NodeRow::Shown(directory.clone())), None);
    assert_eq!(decide_node_errno(&NodeRow::NotShown), Some(Errno::ESTALE), "to the kernel the node is gone either way");
    assert_eq!(decide_node_errno(&NodeRow::Gone), Some(Errno::ESTALE));
    // A lookup in a node asks more of it: a directory.
    assert_eq!(decide_directory_errno(&NodeRow::Shown(directory)), None);
    assert_eq!(decide_directory_errno(&NodeRow::Shown(file)), Some(Errno::ENOTDIR));
    assert_eq!(decide_directory_errno(&NodeRow::NotShown), Some(Errno::ESTALE));
    assert_eq!(decide_directory_errno(&NodeRow::Gone), Some(Errno::ESTALE));
}

#[test]
fn a_lookup_in_a_node_that_is_no_shown_directory_has_the_directorys_errno() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let file = fixture.show_onedrive_file("a.txt", b"hello");
    assert_eq!(errno_of(find_entry(&fixture, &lookup, file.local_item_id, "name")), Some(Errno::ENOTDIR));
    assert_eq!(list_names(&fixture, &lookup, file.local_item_id), Ok(Vec::new()), "a node that is no directory lists nothing, as today");
    // The directory's errno comes before the name's: a hidden name in a node that is gone.
    assert_eq!(errno_of(find_entry(&fixture, &lookup, LocalItemId::new(987_654), ".konedrive-aside-7")), Some(Errno::ESTALE));
}

#[test]
fn a_node_whose_row_is_gone_is_answered_from_the_file_of_the_handle_the_request_carries() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let written = fixture.make_own_file("written.txt", b"abc");
    let read = fixture.make_own_file("read.txt", b"abcdefg");
    let other = fixture.make_own_file("other.txt", b"z");
    let open = |path: &str| Arc::new(fs::File::open(fixture.disk_path(path)).unwrap());
    let (writer, _) = fixture.item_handles.add_writer(written.local_item_id, open("written.txt"));
    let reader = fixture.item_handles.add_file(OpenItemFile::ReadingBackingFile { item: read.local_item_id, backing_file: open("read.txt") });
    let (others_writer, _) = fixture.item_handles.add_writer(other.local_item_id, open("other.txt"));
    let not_on_disk = fixture.item_handles.add_file(OpenItemFile::ReadingNotOnDisk { item: written.local_item_id, content: written.local.clone().unwrap().content.unwrap() });
    let directory = fixture.item_handles.add_directory();
    // Removed while the programs hold them open: the rows are gone, the files are not.
    fixture.remove_here(written.local_item_id);
    fixture.remove_here(read.local_item_id);
    let attributes = find_attributes(&fixture, &lookup, written.local_item_id, Some(writer)).unwrap();
    assert_eq!((attributes.ino, attributes.size, attributes.kind, attributes.mtime), (INodeNo(written.local_item_id.as_i64() as u64), 3, FileType::RegularFile, at(START_TIME)), "by fstat, under the node id asked");
    let attributes = find_attributes(&fixture, &lookup, read.local_item_id, Some(reader)).unwrap();
    assert_eq!((attributes.ino, attributes.size), (INodeNo(read.local_item_id.as_i64() as u64), 7), "a reader's backing file too");
    // No handle, a handle with no file, a handle of another item, a directory's: stale.
    for fh in [None, Some(not_on_disk), Some(others_writer), Some(directory), Some(Fh(987_654))] {
        assert_eq!(errno_of(find_attributes(&fixture, &lookup, written.local_item_id, fh)), Some(Errno::ESTALE), "{fh:?}");
    }
    // A row that is there and shows nothing is answered the same way.
    let bound = fixture.show_onedrive_file("bound.txt", b"hello");
    fixture.land_copy(bound.local_item_id, b"hello");
    let bound_reader = fixture.item_handles.add_file(OpenItemFile::ReadingBackingFile { item: bound.local_item_id, backing_file: open("bound.txt") });
    assert!(fixture.remove_here(bound.local_item_id).is_some());
    assert_eq!(find_attributes(&fixture, &lookup, bound.local_item_id, Some(bound_reader)).unwrap().size, 5);
}

#[test]
fn a_row_at_a_name_of_the_daemons_own_is_not_shown() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    // A row at a temporary name of an exchange: a row's place, which the mount hides.
    let temporary_name = format!("{TEMPORARY_NAME_PREFIX}7");
    let hidden = make_row(&fixture, ROOT, temporary_name.clone().into(), 1);
    assert_eq!(errno_of(find_entry(&fixture, &lookup, ROOT, &temporary_name)), Some(Errno::ENOENT));
    // The names of entries no row says are not looked up at all.
    let aside_name = make_aside_entry_name(hidden).to_string_lossy().into_owned();
    for name in [aside_name.as_str(), ".konedrive-holding", ".konedrive-new-1"] {
        assert_eq!(errno_of(find_entry(&fixture, &lookup, ROOT, name)), Some(Errno::ENOENT), "{name}");
    }
    assert_eq!(list_names(&fixture, &lookup, ROOT), Ok(Vec::new()));
    // By its node id the row is an item like any other: a handle outlives a rename.
    assert!(find_attributes(&fixture, &lookup, hidden, None).is_ok());
}

#[test]
fn a_directory_lists_what_the_rows_show_in_it_and_reads_no_disk() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let docs = fixture.show_onedrive_directory("docs");
    fixture.show_onedrive_file("docs/b.txt", b"b");
    fixture.show_onedrive_file("docs/a.txt", b"a");
    fixture.show_onedrive_directory("docs/sub");
    fixture.show_onedrive_file("docs/sub/deep.txt", b"deep");
    // Nothing of it is on disk; and what is on disk with no row is not the folder's.
    assert!(!fixture.disk_path("docs").exists());
    fixture.write_disk_file("stray.txt", b"no row says this");
    assert_eq!(list_names(&fixture, &lookup, docs.local_item_id).unwrap(), ["a.txt", "b.txt", "sub"], "by name, the directory's own entries alone");
    assert_eq!(list_names(&fixture, &lookup, ROOT).unwrap(), ["docs"]);
    let listed = ask(&fixture, &lookup, move |lookup| lookup.list_entries(ROOT)).unwrap();
    assert_eq!(listed, [docs], "the rows, as the rows have them");
}

#[test]
fn a_directory_lists_neither_hidden_names_nor_what_the_folder_does_not_show_and_lists_ignored_names() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    // Ignored names are the folder's: left here, never sent, and shown.
    fixture.make_own_file("notes.tmp", b"ignored by *.tmp");
    fixture.make_own_directory("cache");
    fixture.make_own_file("cache/inside.txt", b"below an ignored directory");
    fixture.make_own_file("kept.txt", b"sent");
    // Hidden: a row at a temporary name.
    make_row(&fixture, ROOT, format!("{TEMPORARY_NAME_PREFIX}3").into(), 1);
    // Not shown: removed here and not sent; in OneDrive and not shown yet.
    let removed = fixture.show_onedrive_file("removed.txt", b"x");
    fixture.remove_here(removed.local_item_id);
    fixture.list_onedrive_file("listed.txt", b"y");
    assert_eq!(list_names(&fixture, &lookup, ROOT).unwrap(), ["cache", "kept.txt", "notes.tmp"]);
    let cache = fixture.find_item_at("cache").unwrap();
    assert_eq!(list_names(&fixture, &lookup, cache.local_item_id).unwrap(), ["inside.txt"]);
    assert!(find_entry(&fixture, &lookup, ROOT, "notes.tmp").is_ok() && find_entry(&fixture, &lookup, cache.local_item_id, "inside.txt").is_ok());
}

#[test]
fn the_attributes_of_a_file_open_for_writing_are_read_from_its_writers_file() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let file = fixture.show_onedrive_file("a.txt", b"hello");
    let node = file.local_item_id;
    fixture.land_copy(node, b"hello");
    // The first writer's open: the row first, then the handle; bytes go to disk with no
    // row touched per write.
    fixture.write_rows(move |store| store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: node }));
    let backing_file = Arc::new(fs::OpenOptions::new().append(true).open(fixture.disk_path("a.txt")).unwrap());
    let (writer, _) = fixture.item_handles.add_writer(node, Arc::clone(&backing_file));
    (&*backing_file).write_all(b" world, appended").unwrap();
    fixture.set_disk_mtime("a.txt", 1_650_000_000);
    let row = fixture.find_item(node).unwrap();
    assert_eq!(row.local.as_ref().unwrap().content.as_ref().unwrap().size(), 5, "the row keeps the size of before the open until the last close");
    let on_disk = fs::metadata(fixture.disk_path("a.txt")).unwrap();
    for attributes in [find_attributes(&fixture, &lookup, node, None).unwrap(), find_attributes(&fixture, &lookup, node, Some(writer)).unwrap(), find_entry(&fixture, &lookup, ROOT, "a.txt").unwrap().attributes] {
        assert_eq!((attributes.size, attributes.mtime, attributes.blocks), (21, at(1_650_000_000), on_disk.blocks()), "the backing file's, never the stale row's: an outdated size loses appended data");
    }
    // The writer's file is asked by its handle: a rename of the backing file between the
    // row and the answer changes nothing, and no place change is waited for.
    fs::rename(fixture.disk_path("a.txt"), fixture.disk_path("elsewhere")).unwrap();
    assert_eq!(find_attributes(&fixture, &lookup, node, None).unwrap().size, 21);
    fs::rename(fixture.disk_path("elsewhere"), fixture.disk_path("a.txt")).unwrap();
    // Once the writer is gone the row speaks again: the last close wrote it first.
    fixture.write_rows(move |store| store.item_store().apply(&LocalVersionChange::ClosedChanged { item: node, size: 21, mtime: 1_650_000_000, mtime_nsec: 0 }));
    fixture.item_handles.remove_writer(writer);
    let attributes = find_attributes(&fixture, &lookup, node, None).unwrap();
    assert_eq!((attributes.size, attributes.mtime, attributes.blocks), (21, at(1_650_000_000), on_disk.blocks()));
}

#[test]
fn a_file_on_disk_has_the_blocks_of_its_backing_file_and_one_not_on_disk_none() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let content = vec![7u8; 64 * 1024];
    let file = fixture.show_onedrive_file("big.bin", &content);
    let attributes = find_attributes(&fixture, &lookup, file.local_item_id, None).unwrap();
    assert_eq!((attributes.size, attributes.blocks), (65_536, 0), "not on disk: nothing is read of the disk");
    fixture.land_copy(file.local_item_id, &content);
    let on_disk = fs::metadata(fixture.disk_path("big.bin")).unwrap();
    assert!(on_disk.blocks() > 0);
    let attributes = find_attributes(&fixture, &lookup, file.local_item_id, None).unwrap();
    assert_eq!((attributes.size, attributes.blocks, attributes.mtime), (65_536, on_disk.blocks(), at(ONEDRIVE_MTIME)), "the blocks are the disk's, the size and the time the row's");
    // A row that says on disk over a missing file: no blocks; the open says so, not the
    // attributes. And over a directory: not the item's entry.
    fs::remove_file(fixture.disk_path("big.bin")).unwrap();
    assert_eq!(find_attributes(&fixture, &lookup, file.local_item_id, None).map(|attributes| (attributes.size, attributes.blocks)), Ok((65_536, 0)));
    fs::create_dir(fixture.disk_path("big.bin")).unwrap();
    assert_eq!(find_attributes(&fixture, &lookup, file.local_item_id, None).map(|attributes| (attributes.size, attributes.blocks)), Ok((65_536, 0)));
}

#[test]
fn a_directory_has_the_time_of_the_backing_directory_when_it_is_on_disk_else_the_mounts_start() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let docs = fixture.show_onedrive_directory("docs");
    let attributes = find_attributes(&fixture, &lookup, docs.local_item_id, None).unwrap();
    assert_eq!((attributes.kind, attributes.mtime, attributes.size), (FileType::Directory, at(START_TIME), 0), "not on disk: the time the mount started");
    fixture.make_directory_on_disk(docs.local_item_id);
    fixture.set_disk_mtime("docs", 1_650_000_000);
    assert_eq!(find_attributes(&fixture, &lookup, docs.local_item_id, None).unwrap().mtime, at(1_650_000_000));
    assert_eq!(find_entry(&fixture, &lookup, ROOT, "docs").unwrap().attributes.mtime, at(1_650_000_000));
    // The root is the backing directory itself, which is always on disk.
    let folder = fs::metadata(fixture.folder()).unwrap();
    let root = find_attributes(&fixture, &lookup, ROOT, None).unwrap();
    assert_eq!((root.ino, root.kind, root.mtime), (INodeNo::ROOT, FileType::Directory, UNIX_EPOCH + Duration::new(folder.mtime() as u64, folder.mtime_nsec() as u32)));
}

/// Holds a place change open on the fixture's write-operation thread until what is
/// answered is dropped or sent to.
fn hold_place_change_open(fixture: &ItemFixture) -> mpsc::Sender<()> {
    let (begun_sender, begun) = mpsc::channel();
    let (end_sender, end) = mpsc::channel::<()>();
    let place_changes = Arc::clone(&fixture.place_changes);
    fixture.write_operation_thread.run(move || {
        let change = place_changes.begin();
        begun_sender.send(()).unwrap();
        let _ = end.recv();
        change.end();
    });
    begun.recv().unwrap();
    end_sender
}

#[test]
fn a_lookup_while_a_place_change_is_under_way_does_not_wait_and_answers_no_blocks() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let file = fixture.make_own_file("a.bin", &vec![7u8; 64 * 1024]);
    let docs = fixture.make_own_directory("docs");
    fixture.set_disk_mtime("docs", 1_650_000_000);
    let blocks = fs::metadata(fixture.disk_path("a.bin")).unwrap().blocks();
    assert!(blocks > 0);
    assert_eq!(find_entry(&fixture, &lookup, ROOT, "a.bin").unwrap().attributes.blocks, blocks);
    // A rename is under way, its row not written yet: the answers come while it is, from
    // the rows, with nothing of the disk in them.
    let end = hold_place_change_open(&fixture);
    let found = find_entry(&fixture, &lookup, ROOT, "a.bin").unwrap();
    assert_eq!((found.item.local_item_id, found.attributes.size, found.attributes.blocks), (file.local_item_id, 65_536, 0), "no blocks for that one answer");
    assert_eq!(find_attributes(&fixture, &lookup, file.local_item_id, None).unwrap().blocks, 0);
    assert_eq!(find_attributes(&fixture, &lookup, docs.local_item_id, None).unwrap().mtime, at(START_TIME), "nor a directory's time");
    assert_eq!(list_names(&fixture, &lookup, ROOT).unwrap(), ["a.bin", "docs"]);
    end.send(()).unwrap();
    fixture.run_on_write_operation_thread(|| ());
    assert_eq!(find_entry(&fixture, &lookup, ROOT, "a.bin").unwrap().attributes.blocks, blocks);
    assert_eq!(find_attributes(&fixture, &lookup, docs.local_item_id, None).unwrap().mtime, at(1_650_000_000));
}

/// Bounded: fixed counts, a barrier for the start, no sleep. Two files of different
/// sizes are renamed through each other's names, each rename the disk first and the row
/// second inside a place change, as a rename of the mount is; every lookup of one of the
/// names must answer one item's size with that item's blocks, or none.
#[test]
fn a_lookup_racing_renames_never_answers_the_blocks_of_another_items_file() {
    const ROUNDS: usize = 40;
    const READERS: usize = 3;
    const LOOKUPS: usize = 300;
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let x = fixture.make_own_file("a", &vec![1u8; 64 * 1024]).local_item_id;
    let y = fixture.make_own_file("b", &vec![2u8; 256 * 1024]).local_item_id;
    let synced_blocks = |path: &str| {
        let file = fs::File::open(fixture.disk_path(path)).unwrap();
        file.sync_all().unwrap();
        file.metadata().unwrap().blocks()
    };
    let facts = [(x, 64 * 1024u64, synced_blocks("a")), (y, 256 * 1024, synced_blocks("b"))];
    assert!(facts[0].2 > 0 && facts[0].2 != facts[1].2, "the two files are told apart by their blocks");

    let start = Arc::new(Barrier::new(READERS + 1));
    let (place_changes, store, changer_start) = (Arc::clone(&fixture.place_changes), fixture.store.clone(), Arc::clone(&start));
    let (changes_ended_sender, changes_ended) = mpsc::channel();
    fixture.write_operation_thread.run(move || {
        changer_start.wait();
        for _ in 0..ROUNDS {
            // Back to the start after six renames; each name is every item's in turn.
            for (item, from, to) in [(x, "a", "c"), (y, "b", "a"), (x, "c", "b"), (y, "a", "c"), (x, "b", "a"), (y, "c", "b")] {
                let change = place_changes.begin();
                change.move_entry(Path::new(from), Path::new(to), &Vec::new()).unwrap();
                let place = ItemPlace { parent_local_item_id: ROOT, name: to.into() };
                store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::Moved { item, place, is_name_ignored: false })).unwrap();
                change.end();
            }
        }
        changes_ended_sender.send(()).unwrap();
    });
    let answered_with_blocks: usize = std::thread::scope(|scope| {
        let readers: Vec<_> = (0..READERS)
            .map(|reader| {
                let (lookup, start, facts) = (Arc::clone(&lookup), Arc::clone(&start), &facts);
                scope.spawn(move || {
                    start.wait();
                    let mut answered_with_blocks = 0;
                    for round in 0..LOOKUPS {
                        let name = ["a", "b", "c"][(reader + round) % 3];
                        match lookup.find_entry(ROOT, OsStr::new(name)) {
                            Ok(found) => {
                                let (_, size, blocks) = facts.iter().find(|(item, ..)| *item == found.item.local_item_id).expect("one of the two items");
                                assert_eq!(found.attributes.size, *size);
                                assert!(found.attributes.blocks == 0 || found.attributes.blocks == *blocks, "{name}: item {} was answered {} blocks, and its file has {blocks}", found.item.local_item_id, found.attributes.blocks);
                                answered_with_blocks += usize::from(found.attributes.blocks != 0);
                            }
                            Err(errno) => assert_eq!(errno, Errno::ENOENT, "a name that is free at that moment"),
                        }
                    }
                    answered_with_blocks
                })
            })
            .collect();
        readers.into_iter().map(|reader| reader.join().unwrap()).sum()
    });
    changes_ended.recv().unwrap();
    // Everything is where it began, and a lookup at rest has its blocks.
    for (name, (item, _, blocks)) in ["a", "b"].into_iter().zip(facts) {
        let found = find_entry(&fixture, &lookup, ROOT, name).unwrap();
        assert_eq!((found.item.local_item_id, found.attributes.blocks), (item, blocks));
    }
    fixture.assert_rows_and_disk_agree();
    // Not asserted: how many answers had blocks while the renames ran.
    let _ = answered_with_blocks;
}

#[test]
fn the_state_attribute_is_the_rows_sync_state_and_whether_it_is_downloading() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let file = fixture.show_onedrive_file("a.txt", b"hello");
    let node = file.local_item_id;
    assert_eq!(find_xattr(&fixture, &lookup, node, XATTR_STATE).unwrap(), "online-only");
    let key = DownloadKey { item: node, content: file.local.clone().unwrap().content.unwrap() };
    let crate::mount::item_download_states::ItemDownloadStart::Started { run, .. } = fixture.download_states.start_or_join(&key, &DownloadAsker::Read, None) else { panic!("the download starts") };
    assert_eq!(find_xattr(&fixture, &lookup, node, XATTR_STATE).unwrap(), "hydrating");
    fixture.download_states.finish(&run, Ok(()));
    fixture.land_copy(node, b"hello");
    assert_eq!(find_xattr(&fixture, &lookup, node, XATTR_STATE).unwrap(), "hydrated");
    fixture.change_file_here(node, b"changed here");
    assert_eq!(find_xattr(&fixture, &lookup, node, XATTR_STATE).unwrap(), "hydrated", "changed here: on disk still");
    // What is no file of OneDrive has no state.
    let own = fixture.make_own_file("own.txt", b"abc");
    let docs = fixture.show_onedrive_directory("docs");
    for node in [own.local_item_id, docs.local_item_id, ROOT] {
        assert_eq!(errno_of(find_xattr(&fixture, &lookup, node, XATTR_STATE)), Some(Errno::ENODATA));
    }
}

#[test]
fn the_state_attribute_per_case() {
    let content = |ctag: &str| Some(ContentVersion::Hashed { size: 1, mtime: 1, mtime_nsec: 0, hash: None, ctag: Some(ctag.into()) });
    let binding = Some(ItemFixture::onedrive_reference_of("a"));
    let local_only = shown_item(7, ItemKind::File, content("c1"));
    let synced = Item { onedrive_item: binding.clone(), synced_content: content("c1"), ..local_only.clone() };
    let modified_here = Item { synced_content: content("c0"), ..synced.clone() };
    let remote_only = Item { content_presence: ContentPresence::NotOnDisk, ..synced.clone() };
    let directory = Item { onedrive_item: binding, ..shown_item(8, ItemKind::Directory, None) };
    let not_shown = Item { local: None, ..synced.clone() };
    for is_downloading in [false, true] {
        assert_eq!(decide_state_attribute(&local_only, is_downloading), None, "a file that is not in OneDrive has none");
        assert_eq!(decide_state_attribute(&directory, is_downloading), None);
        assert_eq!(decide_state_attribute(&not_shown, is_downloading), None);
    }
    assert_eq!(decide_state_attribute(&synced, false), Some(State::Hydrated));
    assert_eq!(decide_state_attribute(&modified_here, false), Some(State::Hydrated));
    assert_eq!(decide_state_attribute(&remote_only, false), Some(State::OnlineOnly));
    for file in [&synced, &modified_here, &remote_only] {
        assert_eq!(decide_state_attribute(file, true), Some(State::Hydrating));
    }
}

#[test]
fn the_item_id_attribute_is_the_bindings_and_absent_without_one() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let docs = fixture.show_onedrive_directory("docs");
    let file = fixture.show_onedrive_file("docs/a.txt", b"hello");
    assert_eq!(find_xattr(&fixture, &lookup, file.local_item_id, XATTR_ITEM_ID).unwrap(), file.onedrive_item.unwrap().item_id.as_str());
    assert_eq!(find_xattr(&fixture, &lookup, docs.local_item_id, XATTR_ITEM_ID).unwrap(), docs.onedrive_item.unwrap().item_id.as_str());
    assert_eq!(find_xattr(&fixture, &lookup, ROOT, XATTR_ITEM_ID).unwrap(), crate::mount::item_fixture::ROOT_ONEDRIVE_ID);
    let own = fixture.make_own_file("own.txt", b"abc");
    assert_eq!(errno_of(find_xattr(&fixture, &lookup, own.local_item_id, XATTR_ITEM_ID)), Some(Errno::ENODATA));
    assert_eq!(errno_of(find_xattr(&fixture, &lookup, file.local_item_id, "user.other")), Some(Errno::ENODATA), "an attribute that is none of the mount's");
}

#[test]
fn the_pin_attribute_is_the_rows_pin() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let docs = fixture.show_onedrive_directory("docs");
    let inside = fixture.show_onedrive_file("docs/a.txt", b"hello");
    let outside = fixture.show_onedrive_file("b.txt", b"hello");
    for node in [docs.local_item_id, inside.local_item_id, outside.local_item_id] {
        assert_eq!(errno_of(find_xattr(&fixture, &lookup, node, XATTR_PIN)), Some(Errno::ENODATA), "absent on what no pin holds for");
    }
    let pinned = docs.local_item_id;
    fixture.write_rows(move |store| store.item_store().set_item_pin(pinned, true));
    assert_eq!(find_xattr(&fixture, &lookup, docs.local_item_id, XATTR_PIN).unwrap(), "1");
    assert_eq!(find_xattr(&fixture, &lookup, inside.local_item_id, XATTR_PIN).unwrap(), "1", "a directory's pin holds for what is in it");
    assert_eq!(errno_of(find_xattr(&fixture, &lookup, outside.local_item_id, XATTR_PIN)), Some(Errno::ENODATA));
}

#[test]
fn the_folders_own_attributes_are_the_roots_alone() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let docs = fixture.show_onedrive_directory("docs");
    for name in [XATTR_ROOT, XATTR_DRIVE] {
        assert_eq!(errno_of(find_xattr(&fixture, &lookup, ROOT, name)), Some(Errno::ENODATA), "a folder that carries none");
    }
    assert_eq!(ask(&fixture, &lookup, |lookup| lookup.list_xattr_names(ROOT)).unwrap(), [XATTR_ITEM_ID]);
    // What the folder's registration wrote on the backing directory.
    xattr::set(fixture.folder(), XATTR_ROOT, b"1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d").unwrap();
    xattr::set(fixture.folder(), XATTR_DRIVE, b"D-AbC").unwrap();
    assert_eq!(find_xattr(&fixture, &lookup, ROOT, XATTR_ROOT).unwrap(), "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d");
    assert_eq!(find_xattr(&fixture, &lookup, ROOT, XATTR_DRIVE).unwrap(), "D-AbC", "as it was written, not as the rows spell the drive");
    assert_eq!(ask(&fixture, &lookup, |lookup| lookup.list_xattr_names(ROOT)).unwrap(), [XATTR_ROOT, XATTR_DRIVE, XATTR_ITEM_ID]);
    // No other node has them, whatever its backing directory carries.
    fixture.make_directory_on_disk(docs.local_item_id);
    xattr::set(fixture.disk_path("docs"), XATTR_ROOT, b"1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d").unwrap();
    assert_eq!(errno_of(find_xattr(&fixture, &lookup, docs.local_item_id, XATTR_ROOT)), Some(Errno::ENODATA));
    let node = docs.local_item_id;
    assert_eq!(ask(&fixture, &lookup, move |lookup| lookup.list_xattr_names(node)).unwrap(), [XATTR_ITEM_ID]);
}

#[test]
fn the_attributes_a_node_has_are_named_by_one_read() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let list = |node: LocalItemId| ask(&fixture, &lookup, move |lookup| lookup.list_xattr_names(node)).unwrap();
    let file = fixture.show_onedrive_file("a.txt", b"hello");
    let node = file.local_item_id;
    assert_eq!(list(node), [XATTR_ITEM_ID, XATTR_STATE]);
    fixture.write_rows(move |store| store.item_store().set_item_pin(node, true));
    assert_eq!(list(node), [XATTR_ITEM_ID, XATTR_STATE, XATTR_PIN]);
    fixture.land_copy(node, b"hello");
    fixture.change_file_here(node, b"changed here");
    assert_eq!(list(node), [XATTR_ITEM_ID, XATTR_STATE, XATTR_PIN, XATTR_SYNC]);
    let own = fixture.make_own_file("own.txt", b"abc");
    assert_eq!(list(own.local_item_id), [XATTR_SYNC], "a file made here and not sent");
    let ignored = fixture.make_own_file("notes.tmp", b"abc");
    assert!(list(ignored.local_item_id).is_empty(), "a file that is left here has none");
}

#[test]
fn the_sync_mark_is_read_from_the_row_the_running_steps_and_the_plans_answer() {
    let fixture = ItemFixture::new();
    let lookup = lookup(&fixture);
    let file = fixture.show_onedrive_file("a.txt", b"hello");
    let node = file.local_item_id;
    fixture.land_copy(node, b"hello");
    assert_eq!(errno_of(find_xattr(&fixture, &lookup, node, XATTR_SYNC)), Some(Errno::ENODATA), "an item that agrees has none");
    // A step that runs for an item that agrees (a download) marks nothing.
    let step = |class| RunningStep { class, item: node, other_items: Vec::new(), destination: None, sent: 0, total: 0 };
    assert!(fixture.running_steps.begin(step(StepClass::Download)));
    assert_eq!(errno_of(find_xattr(&fixture, &lookup, node, XATTR_SYNC)), Some(Errno::ENODATA));
    fixture.running_steps.end(node);
    fixture.change_file_here(node, b"changed here");
    assert_eq!(find_xattr(&fixture, &lookup, node, XATTR_SYNC).unwrap(), "pending", "unsent, and the plan has not been asked yet");
    fixture.published_plan.publish([(node, ItemAnswer::Planned(StepClass::Content))].into());
    assert_eq!(find_xattr(&fixture, &lookup, node, XATTR_SYNC).unwrap(), "pending");
    assert!(fixture.running_steps.begin(step(StepClass::Content)));
    assert_eq!(find_xattr(&fixture, &lookup, node, XATTR_SYNC).unwrap(), "uploading");
    fixture.running_steps.end(node);
    fixture.published_plan.publish([(node, ItemAnswer::Waiting(WaitReason::User(Reason::NameCharacters)))].into());
    assert_eq!(find_xattr(&fixture, &lookup, node, XATTR_SYNC).unwrap(), "blocked");
    // What is left here has none, also below an ignored directory, where neither the row
    // nor the plan says so.
    fixture.make_own_directory("cache");
    let below = fixture.make_own_file("cache/inside.txt", b"abc");
    assert!(below.unsent_since.is_some() && !below.is_name_ignored);
    assert_eq!(errno_of(find_xattr(&fixture, &lookup, below.local_item_id, XATTR_SYNC)), Some(Errno::ENODATA));
    // A directory made here is unsent like a file.
    let made = fixture.make_own_directory("made");
    assert_eq!(find_xattr(&fixture, &lookup, made.local_item_id, XATTR_SYNC).unwrap(), "pending");
}

#[test]
fn the_sync_mark_per_case() {
    let unsent = Item { unsent_since: Some(3), ..shown_item(7, ItemKind::File, None) };
    let agreeing = shown_item(7, ItemKind::File, None);
    let user = ItemAnswer::Waiting(WaitReason::User(Reason::NameCharacters));
    let time = ItemAnswer::Waiting(WaitReason::Time { until: 5, reason: Reason::NameCharacters });
    let fault = ItemAnswer::Fault(PlanFault::WaitWithoutEnd(LocalItemId::new(7)));
    let excluded = ItemAnswer::Excluded(ExclusionReason::NameIgnored);
    let planned = ItemAnswer::Planned(StepClass::Content);
    // An item that agrees has no mark, whatever runs and whatever the plan said last.
    for (has_running_step, answer) in [(false, None), (true, None), (true, Some(&user)), (false, Some(&fault))] {
        assert_eq!(decide_sync_mark(&agreeing, false, has_running_step, answer, false), None);
    }
    // Nor one that is left here: by the rows, or by the plan's answer.
    for has_running_step in [false, true] {
        assert_eq!(decide_sync_mark(&unsent, true, has_running_step, None, false), None);
        assert_eq!(decide_sync_mark(&unsent, false, has_running_step, Some(&excluded), false), None);
        assert_eq!(decide_sync_mark(&unsent, false, has_running_step, Some(&ItemAnswer::Excluded(ExclusionReason::CannotBeShown)), false), None);
    }
    // Nor one the folder does not show.
    assert_eq!(decide_sync_mark(&Item { local: None, ..unsent.clone() }, false, true, Some(&user), false), None);
    // Uploading while a step runs, before anything the plan said.
    for answer in [None, Some(&planned), Some(&user), Some(&time), Some(&fault)] {
        assert_eq!(decide_sync_mark(&unsent, false, true, answer, false), Some(SyncMark::Uploading));
    }
    // Blocked: a wait for the user, or a fault of the plan.
    assert_eq!(decide_sync_mark(&unsent, false, false, Some(&user), false), Some(SyncMark::Blocked));
    assert_eq!(decide_sync_mark(&unsent, false, false, Some(&fault), false), Some(SyncMark::Blocked));
    // Pending: everything else, an item the last plan was not given among it.
    let other_waits = [WaitReason::AnotherItem(LocalItemId::new(8)), WaitReason::ItemNotGiven(LocalItemId::new(8)), WaitReason::RunningStep, WaitReason::StartUpCheck, WaitReason::Listing, WaitReason::ItemsInDirectoryNotGiven(LocalItemId::new(8))];
    for answer in [None, Some(planned), Some(time)].into_iter().chain(other_waits.into_iter().map(|wait| Some(ItemAnswer::Waiting(wait)))) {
        assert_eq!(decide_sync_mark(&unsent, false, false, answer.as_ref(), false), Some(SyncMark::Pending), "{answer:?}");
    }
    assert_eq!((SyncMark::Pending.as_str(), SyncMark::Uploading.as_str(), SyncMark::Blocked.as_str()), ("pending", "uploading", "blocked"), "the words Dolphin's emblem reads");
}
