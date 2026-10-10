use std::ffi::OsString;
use std::path::Path;

use konedrive_tree::ItemKind;

use super::{BackingDirectoryContents, BackingDirectoryEntry, DirectoryScan};
use crate::folder::disk::{ASIDE_ENTRY_PREFIX, HOLDING};
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::backing_directory_check::disk_finding::{CopyWholeness, DiskContent, UnresolvedReason};
use crate::fuse::backing_directory_check::mark_change::EntryLinks;

/// What a test says the backing directory has at one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Listed {
    /// A file of 3 bytes, last modified at 100.
    File,
    Directory,
    /// A directory whose entries cannot be read.
    UnreadableDirectory,
    /// A symbolic link, a FIFO: no file and no directory.
    Special,
}

fn entry_of(name: OsString, listed: Listed) -> Option<BackingDirectoryEntry> {
    let kind = match listed {
        Listed::File => ItemKind::File,
        Listed::Directory | Listed::UnreadableDirectory => ItemKind::Directory,
        Listed::Special => return None,
    };
    Some(BackingDirectoryEntry { name, kind, onedrive_id: None, wholeness: CopyWholeness::NotWhole, links: EntryLinks::One, file: DiskContent { size: 3, mtime: 100, mtime_nsec: 0, ctag: None } })
}

/// The contents of a backing directory that has `listed`, each at its path, with no disk
/// read.
fn contents_of(listed: &[(&str, Listed)]) -> BackingDirectoryContents {
    BackingDirectoryContents::gather(|level| {
        level
            .iter()
            .map(|directory| {
                if listed.iter().any(|(path, what)| Path::new(path) == directory && *what == Listed::UnreadableDirectory) {
                    return DirectoryScan { unreadable: Some("Permission denied".to_owned()), ..DirectoryScan::default() };
                }
                let mut scan = DirectoryScan::default();
                for (path, what) in listed.iter().filter(|(path, _)| Path::new(path).parent() == Some(directory)) {
                    let name = Path::new(path).file_name().unwrap().to_owned();
                    match entry_of(name.clone(), *what) {
                        Some(entry) => scan.entries.push(entry),
                        None => scan.unknown_names.push((name, UnresolvedReason::NotFileOrDirectory)),
                    }
                }
                scan.entries.sort_by(|a, b| a.name.cmp(&b.name));
                scan
            })
            .collect()
    })
}

#[test]
fn an_entry_is_found_by_its_directory_and_its_name() {
    let contents = contents_of(&[("b", Listed::Directory), ("b/z.txt", Listed::File), ("b/a.txt", Listed::File), ("a.txt", Listed::File), ("b/sub", Listed::Directory)]);
    let b = contents.find_inside(None, "b".as_ref()).unwrap();
    let names = |directory| contents.list_inside(directory).map(|index| contents.entry(index).name.clone()).collect::<Vec<_>>();
    assert_eq!(names(None), ["a.txt", "b"], "by name");
    assert_eq!(names(Some(b)), ["a.txt", "sub", "z.txt"]);
    assert_eq!(contents.entry(contents.find_inside(Some(b), "sub".as_ref()).unwrap()).kind, ItemKind::Directory);
    assert_eq!(contents.find_inside(Some(b), "none".as_ref()), None);
    assert_eq!(contents.find_inside(None, "z.txt".as_ref()), None, "a name is of its directory");
    assert!(!contents.has_unread_part() && contents.list_unresolved().is_empty());
}

#[test]
fn what_was_not_read_is_said_and_is_not_taken_for_nothing() {
    let contents = contents_of(&[("locked", Listed::UnreadableDirectory), ("locked/in.txt", Listed::File), ("link", Listed::Special), ("docs", Listed::Directory), ("docs/fifo", Listed::Special)]);
    let (locked, docs) = (contents.find_inside(None, "locked".as_ref()).unwrap(), contents.find_inside(None, "docs".as_ref()).unwrap());
    assert!(contents.list_inside(Some(locked)).is_empty(), "nothing in a directory that was not read is an entry");
    assert!(contents.is_unread(Some(locked)) && !contents.is_unread(Some(docs)) && !contents.is_unread(None));
    assert!(contents.has_unread_part());
    assert!(contents.has_unknown_name(None, "link".as_ref()) && contents.has_unknown_name(Some(docs), "fifo".as_ref()) && !contents.has_unknown_name(None, "fifo".as_ref()));
    let mut left: Vec<&Path> = contents.list_unresolved().iter().map(|left| left.path.as_path()).collect();
    left.sort();
    assert_eq!(left, [Path::new("docs/fifo"), Path::new("link"), Path::new("locked")]);
}

#[test]
fn a_name_that_is_no_file_and_no_directory_alone_is_an_unread_part() {
    assert!(contents_of(&[("link", Listed::Special)]).has_unread_part());
}

#[test]
fn a_directory_deeper_than_the_rows_go_is_not_read_and_is_said() {
    let mut listed = Vec::new();
    let mut path = String::new();
    for depth in 0..=konedrive_fs::MAX_DEPTH {
        path = if depth == 0 { "d".to_owned() } else { format!("{path}/d") };
        listed.push(path.clone());
    }
    let listed: Vec<(&str, Listed)> = listed.iter().map(|path| (path.as_str(), Listed::Directory)).collect();
    let contents = contents_of(&listed);
    assert_eq!(contents.list_unresolved().len(), 1);
    assert_eq!(contents.list_unresolved()[0].reason, UnresolvedReason::TooDeep);
}

/// Over the disk: the marks, the names a file has, and the names the daemon uses for itself.
#[test]
fn the_disk_is_read_with_each_entrys_marks_and_links_and_without_the_daemons_own_names() {
    use konedrive_fs::placeholder::{State, XATTR_CTAG, XATTR_ITEM_ID, XATTR_STATE};
    let dir = tempfile::tempdir().unwrap();
    let at = |rel: &str| dir.path().join(rel);
    std::fs::create_dir(at("docs")).unwrap();
    std::fs::write(at("docs/copy.txt"), b"copy").unwrap();
    xattr::set(at("docs/copy.txt"), XATTR_ITEM_ID, b"A").unwrap();
    xattr::set(at("docs/copy.txt"), XATTR_STATE, State::Hydrated.as_str().as_bytes()).unwrap();
    xattr::set(at("docs/copy.txt"), XATTR_CTAG, b"c-A").unwrap();
    std::fs::write(at("own.txt"), b"own").unwrap();
    std::fs::hard_link(at("own.txt"), at("docs/link.txt")).unwrap();
    std::fs::write(at("id-alone.txt"), b"x").unwrap();
    xattr::set(at("id-alone.txt"), XATTR_ITEM_ID, b"B").unwrap();
    // The daemon's own: an entry set aside, in the root; the holding directory, which a
    // folder is not opened with.
    std::fs::write(at(&format!("{ASIDE_ENTRY_PREFIX}7")), b"aside").unwrap();
    std::fs::write(at(&format!("docs/{ASIDE_ENTRY_PREFIX}7")), b"a file of the user's").unwrap();

    let backing_directory = BackingDirectory::open(dir.path()).unwrap();
    std::fs::create_dir(at(HOLDING)).unwrap();
    let contents = BackingDirectoryContents::read(&backing_directory);
    let names = |directory| contents.list_inside(directory).map(|index| contents.entry(index).name.clone()).collect::<Vec<_>>();
    assert_eq!(names(None), ["docs", "id-alone.txt", "own.txt"]);
    let docs = contents.find_inside(None, "docs".as_ref()).unwrap();
    assert_eq!(names(Some(docs)), [OsString::from(format!("{ASIDE_ENTRY_PREFIX}7")), "copy.txt".into(), "link.txt".into()], "a name set aside is the daemon's in the root alone");

    let entry = |directory, name: &str| contents.entry(contents.find_inside(directory, name.as_ref()).unwrap()).clone();
    let copy = entry(Some(docs), "copy.txt");
    assert_eq!((copy.onedrive_id.as_deref(), copy.wholeness, copy.links, copy.file.size, copy.file.ctag.as_deref()), (Some("A"), CopyWholeness::Whole, EntryLinks::One, 4, Some("c-A")));
    let id_alone = entry(None, "id-alone.txt");
    assert_eq!((id_alone.onedrive_id.as_deref(), id_alone.wholeness), (Some("B"), CopyWholeness::NotWhole));
    assert_eq!((entry(None, "own.txt").links, entry(Some(docs), "link.txt").links), (EntryLinks::Several, EntryLinks::Several));
    assert_eq!((entry(None, "own.txt").onedrive_id, entry(None, "docs").links), (None, EntryLinks::One));
}
