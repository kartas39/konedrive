//! Everything the backing directory holds, read once ([`BackingDirectoryContents`]): each
//! directory's entries with what the filesystem and their marks say of them
//! ([`BackingDirectoryEntry`]), and which entries carry each OneDrive item id.
//!
//! The disk is read here and nowhere else in the check, a level of directories at a time on
//! several threads: one look at each entry and its marks. Nothing is decided here. What is
//! not read is said, never taken for nothing: a directory that cannot be read, one deeper
//! than the rows go, a name that is neither a file nor a directory. The names the daemon
//! uses for itself are not the folder's and are left out.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use konedrive_tree::ItemKind;

use super::disk_finding::{CopyWholeness, DiskContent, UnresolvedReason};
use super::UnresolvedPath;
use crate::folder::disk::daemon_owned;
use crate::fuse::backing_directory::{BackingDirectory, DiskMarks};

/// How many threads read the disk at most: the measurement the check is built on found no
/// gain beyond eight.
const MOST_THREADS: usize = 8;

/// Which entry of the contents: its number in the order they were read, a directory before
/// what is in it.
pub(super) type EntryIndex = usize;

/// What the disk says of one directory entry: its name, its kind, the marks it carries, and
/// for a file what tells its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BackingDirectoryEntry {
    pub(super) name: OsString,
    pub(super) kind: ItemKind,
    /// The OneDrive item id the entry is marked with.
    pub(super) onedrive_id: Option<String>,
    pub(super) wholeness: CopyWholeness,
    /// Of a file. A directory's say nothing.
    pub(super) file: DiskContent,
}

/// One directory as it was read.
#[derive(Default)]
struct DirectoryScan {
    entries: Vec<BackingDirectoryEntry>,
    /// The names that are no file and no directory, or that could not be looked at.
    unknown_names: Vec<(OsString, UnresolvedReason)>,
    /// Why the directory could not be read to its end.
    unreadable: Option<String>,
}

pub(super) struct BackingDirectoryContents {
    entries: Vec<BackingDirectoryEntry>,
    /// The directory each entry is in: `None` for the backing directory itself.
    parents: Vec<Option<EntryIndex>>,
    /// The entries in each entry, by name: none in a file, and none in a directory that was
    /// not read.
    inside: Vec<Range<usize>>,
    /// The entries in the backing directory itself.
    inside_root: Range<usize>,
    by_onedrive_id: HashMap<String, Vec<EntryIndex>>,
    /// The directories whose entries are not known.
    unread: HashSet<Option<EntryIndex>>,
    unknown_names: HashSet<(Option<EntryIndex>, OsString)>,
    unresolved: Vec<UnresolvedPath>,
}

fn read_one(backing_directory: &BackingDirectory, rel: &Path) -> DirectoryScan {
    let mut read = DirectoryScan::default();
    let listed = match backing_directory.read_directory(rel) {
        Ok(listed) => listed,
        Err(e) => return DirectoryScan { unreadable: Some(e.to_string()), ..read },
    };
    for listed in listed {
        let listed = match listed {
            Ok(listed) => listed,
            Err(e) => return DirectoryScan { unreadable: Some(e.to_string()), ..DirectoryScan::default() },
        };
        let name = listed.file_name();
        if daemon_owned(&name) {
            continue;
        }
        let metadata = match listed.metadata() {
            Ok(metadata) => metadata,
            Err(e) => {
                read.unknown_names.push((name, UnresolvedReason::Unreadable(e.to_string())));
                continue;
            }
        };
        let kind = match (metadata.is_file(), metadata.is_dir()) {
            (true, _) => ItemKind::File,
            (_, true) => ItemKind::Directory,
            _ => {
                read.unknown_names.push((name, UnresolvedReason::NotFileOrDirectory));
                continue;
            }
        };
        let marks = DiskMarks::read(&listed.path());
        let onedrive_id = marks.onedrive_id();
        let wholeness = if onedrive_id.is_none() || marks.is_whole() { CopyWholeness::Whole } else { CopyWholeness::Partial };
        let ctag = if kind == ItemKind::File && onedrive_id.is_some() { backing_directory.read_ctag_mark(&rel.join(&name)) } else { None };
        let file = DiskContent { size: metadata.size(), mtime: metadata.mtime(), mtime_nsec: metadata.mtime_nsec() as u32, ctag };
        read.entries.push(BackingDirectoryEntry { name, kind, onedrive_id, wholeness, file });
    }
    read.entries.sort_by(|a, b| a.name.cmp(&b.name));
    read
}

/// The directories of one level, each read: on several threads when there are enough of
/// them to be worth it.
fn read_level(backing_directory: &BackingDirectory, level: &[PathBuf]) -> Vec<DirectoryScan> {
    let threads = std::thread::available_parallelism().map_or(1, usize::from).min(MOST_THREADS).min(level.len() / 4);
    if threads < 2 {
        return level.iter().map(|rel| read_one(backing_directory, rel)).collect();
    }
    let next = AtomicUsize::new(0);
    let mut read: Vec<Option<DirectoryScan>> = level.iter().map(|_| None).collect();
    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(rel) = level.get(at) else { return mine };
                        mine.push((at, read_one(backing_directory, rel)));
                    }
                })
            })
            .collect();
        for reader in readers {
            // A reader that panicked read nothing of its directories: they are said to be
            // unreadable below, not taken for empty.
            for (at, directory) in reader.join().unwrap_or_default() {
                read[at] = Some(directory);
            }
        }
    });
    read.into_iter().map(|directory| directory.unwrap_or_else(|| DirectoryScan { unreadable: Some("its reader failed".to_owned()), ..DirectoryScan::default() })).collect()
}

impl BackingDirectoryContents {
    /// Reads the whole backing directory.
    pub(super) fn read(backing_directory: &BackingDirectory) -> Self {
        let mut contents = BackingDirectoryContents {
            entries: Vec::new(),
            parents: Vec::new(),
            inside: Vec::new(),
            inside_root: 0..0,
            by_onedrive_id: HashMap::new(),
            unread: HashSet::new(),
            unknown_names: HashSet::new(),
            unresolved: Vec::new(),
        };
        // The directories of one depth, each with its path: the backing directory itself first.
        let mut level: Vec<(Option<EntryIndex>, PathBuf)> = vec![(None, PathBuf::new())];
        let mut depth = 0;
        while !level.is_empty() {
            let paths: Vec<PathBuf> = level.iter().map(|(_, rel)| rel.clone()).collect();
            let mut next_level = Vec::new();
            for ((directory, rel), read) in level.into_iter().zip(read_level(backing_directory, &paths)) {
                if let Some(reason) = read.unreadable {
                    contents.unread.insert(directory);
                    contents.unresolved.push(UnresolvedPath { path: rel, reason: UnresolvedReason::Unreadable(reason) });
                    continue;
                }
                for (name, reason) in read.unknown_names {
                    contents.unresolved.push(UnresolvedPath { path: rel.join(&name), reason });
                    contents.unknown_names.insert((directory, name));
                }
                let first = contents.entries.len();
                for entry in read.entries {
                    let index = contents.entries.len();
                    if let Some(id) = &entry.onedrive_id {
                        contents.by_onedrive_id.entry(id.clone()).or_default().push(index);
                    }
                    if entry.kind == ItemKind::Directory {
                        // An entry of this directory is one level deeper than it.
                        if depth + 1 < konedrive_fs::MAX_DEPTH {
                            next_level.push((Some(index), rel.join(&entry.name)));
                        } else {
                            contents.unread.insert(Some(index));
                            contents.unresolved.push(UnresolvedPath { path: rel.join(&entry.name), reason: UnresolvedReason::TooDeep });
                        }
                    }
                    contents.entries.push(entry);
                    contents.parents.push(directory);
                    contents.inside.push(0..0);
                }
                let range = first..contents.entries.len();
                match directory {
                    Some(directory) => contents.inside[directory] = range,
                    None => contents.inside_root = range,
                }
            }
            level = next_level;
            depth += 1;
        }
        contents
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn entry(&self, index: EntryIndex) -> &BackingDirectoryEntry {
        &self.entries[index]
    }

    /// The directory the entry is in: `None` for the backing directory itself.
    pub(super) fn parent(&self, index: EntryIndex) -> Option<EntryIndex> {
        self.parents[index]
    }

    /// The entries in `directory`, by name.
    pub(super) fn list_inside(&self, directory: Option<EntryIndex>) -> Range<usize> {
        match directory {
            Some(directory) => self.inside[directory].clone(),
            None => self.inside_root.clone(),
        }
    }

    /// The entry of this name in `directory`.
    pub(super) fn find_inside(&self, directory: Option<EntryIndex>, name: &OsStr) -> Option<EntryIndex> {
        let inside = self.list_inside(directory);
        self.entries[inside.clone()].binary_search_by(|entry| entry.name.as_os_str().cmp(name)).ok().map(|at| inside.start + at)
    }

    /// The entries of `kind` marked with the OneDrive item id `onedrive_id`, in the order
    /// they were read.
    pub(super) fn list_marked(&self, onedrive_id: &str, kind: ItemKind) -> impl Iterator<Item = EntryIndex> + '_ {
        self.by_onedrive_id.get(onedrive_id).into_iter().flatten().copied().filter(move |index| self.entries[*index].kind == kind)
    }

    /// Whether what is in `directory` is not known: it could not be read, or is too deep.
    pub(super) fn is_unread(&self, directory: Option<EntryIndex>) -> bool {
        self.unread.contains(&directory)
    }

    /// Whether `directory` holds this name as something that is no file and no directory,
    /// or that could not be looked at.
    pub(super) fn has_unknown_name(&self, directory: Option<EntryIndex>, name: &OsStr) -> bool {
        !self.unknown_names.is_empty() && self.unknown_names.contains(&(directory, name.to_owned()))
    }

    /// The path of the entry, relative to the backing directory.
    pub(super) fn resolve_path(&self, index: EntryIndex) -> PathBuf {
        let mut names = vec![self.entries[index].name.as_os_str()];
        let mut at = self.parents[index];
        while let Some(directory) = at {
            names.push(self.entries[directory].name.as_os_str());
            at = self.parents[directory];
        }
        names.iter().rev().collect()
    }

    /// The paths that were not read, each with why.
    pub(super) fn list_unresolved(&self) -> &[UnresolvedPath] {
        &self.unresolved
    }
}
