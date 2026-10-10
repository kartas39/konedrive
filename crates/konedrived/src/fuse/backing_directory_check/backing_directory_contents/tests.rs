use std::ffi::OsString;
use std::path::{Path, PathBuf};

use konedrive_tree::ItemKind;

use super::{BackingDirectoryContents, BackingDirectoryEntry, DirectoryScan};
use crate::fuse::backing_directory_check::disk_finding::{CopyWholeness, DiskContent, UnresolvedReason};

/// What a test says the backing directory has at one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse::backing_directory_check) enum Listed {
    /// A file, marked whole with this OneDrive item id and the cTag `c-<id>` when one is
    /// given: 3 bytes, last modified at 100.
    File(Option<&'static str>),
    /// A file marked with the id and not whole.
    PartialFile(&'static str),
    Directory(Option<&'static str>),
    /// A directory whose entries cannot be read.
    UnreadableDirectory(Option<&'static str>),
    /// A symbolic link, a FIFO: no file and no directory.
    Special,
}

fn entry_of(name: OsString, listed: Listed) -> Option<BackingDirectoryEntry> {
    let (kind, onedrive_id, wholeness) = match listed {
        Listed::File(mark) => (ItemKind::File, mark, CopyWholeness::Whole),
        Listed::PartialFile(mark) => (ItemKind::File, Some(mark), CopyWholeness::Partial),
        Listed::Directory(mark) | Listed::UnreadableDirectory(mark) => (ItemKind::Directory, mark, CopyWholeness::Whole),
        Listed::Special => return None,
    };
    let ctag = onedrive_id.filter(|_| kind == ItemKind::File).map(|id| format!("c-{id}"));
    Some(BackingDirectoryEntry { name, kind, onedrive_id: onedrive_id.map(str::to_owned), wholeness, file: DiskContent { size: 3, mtime: 100, mtime_nsec: 0, ctag } })
}

/// The contents of a backing directory that has `listed`, each at its path, with no disk
/// read: what the matching and the deciding are tested over.
pub(in crate::fuse::backing_directory_check) fn contents_of(listed: &[(&str, Listed)]) -> BackingDirectoryContents {
    BackingDirectoryContents::gather(|level| {
        level
            .iter()
            .map(|directory| {
                if listed.iter().any(|(path, what)| Path::new(path) == directory && matches!(what, Listed::UnreadableDirectory(_))) {
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

/// The entry at `path`.
pub(in crate::fuse::backing_directory_check) fn index_of(contents: &BackingDirectoryContents, path: &str) -> usize {
    (0..contents.len()).find(|index| contents.resolve_path(*index) == Path::new(path)).unwrap_or_else(|| panic!("no entry at {path:?}"))
}

#[test]
fn the_contents_are_put_together_a_directory_before_what_is_in_it_and_each_directorys_entries_by_name() {
    let contents = contents_of(&[("b", Listed::Directory(Some("B"))), ("b/z.txt", Listed::File(None)), ("b/a.txt", Listed::File(Some("A"))), ("a.txt", Listed::File(Some("A"))), ("b/sub", Listed::Directory(None))]);
    let paths: Vec<PathBuf> = (0..contents.len()).map(|index| contents.resolve_path(index)).collect();
    assert_eq!(paths, ["a.txt", "b", "b/a.txt", "b/sub", "b/z.txt"].map(PathBuf::from));
    let b = index_of(&contents, "b");
    assert_eq!((contents.parent(b), contents.parent(index_of(&contents, "b/sub"))), (None, Some(b)));
    assert_eq!(contents.list_inside(None), 0..2);
    assert_eq!(contents.find_inside(Some(b), "z.txt".as_ref()), Some(index_of(&contents, "b/z.txt")));
    assert_eq!(contents.find_inside(Some(b), "none".as_ref()), None);
    assert_eq!(contents.list_marked("A", ItemKind::File).collect::<Vec<_>>(), [0, 2]);
    assert_eq!(contents.list_marked("B", ItemKind::File).count(), 0, "a mark is of an entry of its kind");
    assert!(!contents.has_unread_directory() && contents.list_unresolved().is_empty());
}

#[test]
fn what_was_not_read_is_said_and_is_not_taken_for_nothing() {
    let contents = contents_of(&[("locked", Listed::UnreadableDirectory(None)), ("locked/in.txt", Listed::File(None)), ("link", Listed::Special), ("docs", Listed::Directory(None)), ("docs/fifo", Listed::Special)]);
    let (locked, docs) = (index_of(&contents, "locked"), index_of(&contents, "docs"));
    assert_eq!(contents.len(), 2, "nothing in a directory that was not read is an entry");
    assert!(contents.is_unread(Some(locked)) && !contents.is_unread(Some(docs)) && !contents.is_unread(None));
    assert!(contents.has_unread_directory());
    assert!(matches!(contents.find_unread_reason(Some(locked)), Some(UnresolvedReason::Unreadable(_))));
    assert!(contents.has_unknown_name(None, "link".as_ref()) && contents.has_unknown_name(Some(docs), "fifo".as_ref()) && !contents.has_unknown_name(None, "fifo".as_ref()));
    let mut unknown: Vec<_> = contents.list_unknown_names().map(|(directory, name, _)| (directory, name.to_owned())).collect();
    unknown.sort();
    assert_eq!(unknown, [(None, OsString::from("link")), (Some(docs), OsString::from("fifo"))]);
    let mut left: Vec<&Path> = contents.list_unresolved().iter().map(|left| left.path.as_path()).collect();
    left.sort();
    assert_eq!(left, [Path::new("docs/fifo"), Path::new("link"), Path::new("locked")]);
}
