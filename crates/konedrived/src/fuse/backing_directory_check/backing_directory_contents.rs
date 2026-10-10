//! Everything the backing directory holds, read once ([`BackingDirectoryContents`]): each
//! directory's entries with what the filesystem and their marks say of them
//! ([`BackingDirectoryEntry`]).
//!
//! The disk is read here and nowhere else in the check, a level of directories at a time on
//! several threads: one look at each entry and its marks. Nothing is decided here. What is
//! not read is said, never taken for nothing: a directory that cannot be read, one deeper
//! than the rows go, a name that is neither a file nor a directory. The names the daemon
//! uses for itself are not the folder's and are left out: an entry that was set aside
//! among them, which is no row's.
//!
//! An entry is found by its directory and its name, and by nothing else: there is no way
//! here from a mark to the entries that carry it.
//!
//! The reading of one directory ([`DirectoryScan`]) and the putting together of all of them
//! ([`BackingDirectoryContents::gather`]) are apart: the contents are made from whatever
//! reads a level of directories, the disk or values a test gives.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use konedrive_tree::ItemKind;

use super::disk_finding::{CopyWholeness, DiskContent, UnresolvedReason};
use super::mark_change::EntryLinks;
use super::UnresolvedPath;
use crate::folder::disk::{daemon_owned, ASIDE_ENTRY_PREFIX};
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
    /// Whether it is marked as the whole content of the item of its id mark.
    pub(super) wholeness: CopyWholeness,
    /// How many names its file has.
    pub(super) links: EntryLinks,
    /// Of a file. A directory's say nothing.
    pub(super) file: DiskContent,
}

/// One directory as it was read.
#[derive(Default)]
pub(super) struct DirectoryScan {
    /// By name.
    pub(super) entries: Vec<BackingDirectoryEntry>,
    /// The names that are no file and no directory, or that could not be looked at.
    pub(super) unknown_names: Vec<(OsString, UnresolvedReason)>,
    /// Why the directory could not be read to its end.
    pub(super) unreadable: Option<String>,
}

pub(super) struct BackingDirectoryContents {
    entries: Vec<BackingDirectoryEntry>,
    /// The entries in each entry, by name: none in a file, and none in a directory that was
    /// not read.
    inside: Vec<Range<usize>>,
    /// The entries in the backing directory itself.
    inside_root: Range<usize>,
    /// The directories whose entries are not known, each with why.
    unread: HashMap<Option<EntryIndex>, UnresolvedReason>,
    /// The names that are no file and no directory, or that could not be looked at.
    unknown_names: HashMap<(Option<EntryIndex>, OsString), UnresolvedReason>,
    unresolved: Vec<UnresolvedPath>,
}

/// Whether `name`, in the directory `rel`, is a name the daemon uses for itself: nothing of
/// the folder. An entry that was set aside is in the root alone.
// When `folder::disk::is_rowless_entry_name` is filled in, it is this rule for the root.
fn is_daemons_own_name(rel: &Path, name: &OsStr) -> bool {
    daemon_owned(name) || (rel.as_os_str().is_empty() && name.as_bytes().starts_with(ASIDE_ENTRY_PREFIX.as_bytes()))
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
        if is_daemons_own_name(rel, &name) {
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
        // Marks that could not be read say nothing of the entry: it is not taken for an
        // unmarked one. Only an attribute that is not there is no mark.
        let marks = match DiskMarks::try_read(&listed.path()) {
            Ok(marks) => marks,
            Err(e) => {
                read.unknown_names.push((name, UnresolvedReason::Unreadable(e.to_string())));
                continue;
            }
        };
        let onedrive_id = marks.onedrive_id();
        let wholeness = if marks.is_whole() { CopyWholeness::Whole } else { CopyWholeness::NotWhole };
        // A directory's count of links is of the directories in it.
        let links = if kind == ItemKind::File && metadata.nlink() > 1 { EntryLinks::Several } else { EntryLinks::One };
        let ctag = match kind == ItemKind::File && onedrive_id.is_some() {
            true => match backing_directory.try_read_ctag_mark(&rel.join(&name)) {
                Ok(ctag) => ctag,
                // A cTag that is not known would compare as the row's own.
                Err(e) => {
                    read.unknown_names.push((name, UnresolvedReason::Unreadable(e.to_string())));
                    continue;
                }
            },
            false => None,
        };
        let file = DiskContent { size: metadata.size(), mtime: metadata.mtime(), mtime_nsec: metadata.mtime_nsec() as u32, ctag };
        read.entries.push(BackingDirectoryEntry { name, kind, onedrive_id, wholeness, links, file });
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
        Self::gather(|level| read_level(backing_directory, level))
    }

    /// The contents as `read_level` reads them: it is given the paths of the directories of
    /// one depth, the backing directory itself first, and answers each as it was read.
    pub(super) fn gather(mut read_level: impl FnMut(&[PathBuf]) -> Vec<DirectoryScan>) -> Self {
        let mut contents = BackingDirectoryContents {
            entries: Vec::new(),
            inside: Vec::new(),
            inside_root: 0..0,
            unread: HashMap::new(),
            unknown_names: HashMap::new(),
            unresolved: Vec::new(),
        };
        // The directories of one depth, each with its path: the backing directory itself first.
        let mut level: Vec<(Option<EntryIndex>, PathBuf)> = vec![(None, PathBuf::new())];
        let mut depth = 0;
        while !level.is_empty() {
            let paths: Vec<PathBuf> = level.iter().map(|(_, rel)| rel.clone()).collect();
            let mut next_level = Vec::new();
            for ((directory, rel), read) in level.into_iter().zip(read_level(&paths)) {
                if let Some(reason) = read.unreadable {
                    let reason = UnresolvedReason::Unreadable(reason);
                    contents.unread.insert(directory, reason.clone());
                    contents.unresolved.push(UnresolvedPath { path: rel, reason });
                    continue;
                }
                for (name, reason) in read.unknown_names {
                    contents.unresolved.push(UnresolvedPath { path: rel.join(&name), reason: reason.clone() });
                    contents.unknown_names.insert((directory, name), reason);
                }
                let first = contents.entries.len();
                for entry in read.entries {
                    let index = contents.entries.len();
                    if entry.kind == ItemKind::Directory {
                        // An entry of this directory is one level deeper than it.
                        if depth + 1 < konedrive_fs::MAX_DEPTH {
                            next_level.push((Some(index), rel.join(&entry.name)));
                        } else {
                            contents.unread.insert(Some(index), UnresolvedReason::TooDeep);
                            contents.unresolved.push(UnresolvedPath { path: rel.join(&entry.name), reason: UnresolvedReason::TooDeep });
                        }
                    }
                    contents.entries.push(entry);
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

    pub(super) fn entry(&self, index: EntryIndex) -> &BackingDirectoryEntry {
        &self.entries[index]
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

    /// Whether what is in `directory` is not known: it could not be read, or is too deep.
    pub(super) fn is_unread(&self, directory: Option<EntryIndex>) -> bool {
        self.unread.contains_key(&directory)
    }

    /// Whether anything of the backing directory was not read: a directory, or a name
    /// that is no file and no directory or could not be looked at.
    pub(super) fn has_unread_part(&self) -> bool {
        !self.unread.is_empty() || !self.unknown_names.is_empty()
    }

    /// Whether `directory` holds this name as something that is no file and no directory,
    /// or that could not be looked at.
    pub(super) fn has_unknown_name(&self, directory: Option<EntryIndex>, name: &OsStr) -> bool {
        !self.unknown_names.is_empty() && self.unknown_names.contains_key(&(directory, name.to_owned()))
    }

    /// The paths that were not read, each with why.
    pub(super) fn list_unresolved(&self) -> &[UnresolvedPath] {
        &self.unresolved
    }
}

#[cfg(test)]
mod tests;
