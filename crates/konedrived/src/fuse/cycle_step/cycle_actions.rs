//! What one change from OneDrive does to the backing directory: a decision, and nothing
//! else. [`list_cycle_actions`] is a function of the change and of what the backing
//! directory holds at its two paths ([`BackingDirectoryFacts`]), read before it; the cycle
//! step performs the actions it gives, in their order.

use std::path::PathBuf;

use konedrive_tree::{Kind, Row};

use super::onedrive_change::OneDriveChange;
use crate::fuse::inotify_bridge::Announcement;

/// What the backing directory holds at the path a file or directory of OneDrive comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::fuse) enum NewPathContents {
    /// Nothing: the path is free.
    #[default]
    Nothing,
    /// A file or directory that is marked with no OneDrive id: the user's own, which is
    /// moved aside.
    UnmarkedEntry,
    /// Anything else: an entry marked with a OneDrive id, or what is neither a file nor a
    /// directory. It stays where it is, and nothing is moved onto it.
    OtherEntry,
}

/// What the backing directory holds at the two paths of a change, read before the change is
/// decided on. A change that has no old path, or no new one, is given the default of that
/// fact, which it does not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::fuse) struct BackingDirectoryFacts {
    /// Whether the old path holds what the backing directory has of the file or directory
    /// that moved: the file's downloaded copy, or the directory itself.
    pub(in crate::fuse) holds_at_old_path: bool,
    pub(in crate::fuse) new_path: NewPathContents,
}

/// One thing a change from OneDrive does. The first four are done at once, in the order
/// they are given; a removed directory is left for the end of the step, when what was below
/// it went; an announcement is kept until the cycle is committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) enum CycleAction {
    /// The user's own at `path`, where something of OneDrive comes, in the directory with
    /// the OneDrive id `parent_id`, is moved aside to a conflict copy's name.
    MoveLocalOnlyEntryAside { path: PathBuf, parent_id: Option<String> },
    /// A directory moved: the outbox's rows of what the user changed below it are found by
    /// place, and follow it.
    RebaseOutboxRows { from: PathBuf, to: PathBuf },
    /// The downloaded copy of the OneDrive file `id` at `path` is removed: the file is gone,
    /// or its content is another now.
    RemoveDownloadedCopy { path: PathBuf, onedrive_id: String },
    /// What the backing directory holds at `from` moves to `to` with its file or directory.
    /// Not done when a [`CycleAction::MoveLocalOnlyEntryAside`] of the same change left the
    /// user's own in the way: nothing is moved onto it.
    MoveInBackingDirectory { from: PathBuf, to: PathBuf },
    /// The directory with the OneDrive id `id` is gone from OneDrive: the directory at `path`
    /// is removed when nothing is left in it, and stays as a local-only directory otherwise.
    RemoveDirectoryIfEmpty { onedrive_id: String, path: PathBuf },
    /// Whoever watches the folder is told.
    Announce(Announcement),
}

fn announce_vanished(onedrive_id: &str, old: &Row, was: &PathBuf) -> CycleAction {
    CycleAction::Announce(Announcement::NameVanished { path: was.clone(), kind: old.kind, onedrive_id: onedrive_id.to_owned(), parent_id: old.parent_id.clone() })
}

fn announce_appeared(new: &Row, now: &PathBuf) -> CycleAction {
    CycleAction::Announce(Announcement::NameAppeared { path: now.clone(), kind: new.kind, parent_id: new.parent_id.clone() })
}

fn move_aside(new: &Row, now: &PathBuf, facts: &BackingDirectoryFacts) -> Option<CycleAction> {
    (facts.new_path == NewPathContents::UnmarkedEntry).then(|| CycleAction::MoveLocalOnlyEntryAside { path: now.clone(), parent_id: new.parent_id.clone() })
}

/// What `change` does, for what the backing directory holds at its paths (`facts`).
///
/// - What vanished: a file's downloaded copy is removed, a directory is removed if empty,
///   and the name is announced as vanished.
/// - What appeared: the user's own at its path is moved aside, and the name is announced.
/// - What moved: the user's own at the new path is moved aside; a directory's rows are
///   rebased; a file whose content is another too loses its downloaded copy, and anything
///   else the backing directory holds at the old path moves along, unless the new path
///   holds what stays; both names are announced.
/// - What stayed: a file whose content is another loses its downloaded copy, and a file is
///   announced as shown anew. A directory that stayed does nothing.
pub(in crate::fuse) fn list_cycle_actions(change: &OneDriveChange, facts: &BackingDirectoryFacts) -> Vec<CycleAction> {
    let mut actions = Vec::new();
    match change {
        OneDriveChange::Vanished { onedrive_id, old, was } => {
            actions.push(match old.kind {
                Kind::File => CycleAction::RemoveDownloadedCopy { path: was.clone(), onedrive_id: onedrive_id.clone() },
                Kind::Folder => CycleAction::RemoveDirectoryIfEmpty { onedrive_id: onedrive_id.clone(), path: was.clone() },
            });
            actions.push(announce_vanished(onedrive_id, old, was));
        }
        OneDriveChange::Appeared { new, now, .. } => {
            actions.extend(move_aside(new, now, facts));
            actions.push(announce_appeared(new, now));
        }
        OneDriveChange::Moved { onedrive_id, old, was, new, now } => {
            actions.extend(move_aside(new, now, facts));
            if new.kind == Kind::Folder {
                actions.push(CycleAction::RebaseOutboxRows { from: was.clone(), to: now.clone() });
            }
            if change.has_new_content() {
                actions.push(CycleAction::RemoveDownloadedCopy { path: was.clone(), onedrive_id: onedrive_id.clone() });
            } else if facts.holds_at_old_path && facts.new_path != NewPathContents::OtherEntry {
                actions.push(CycleAction::MoveInBackingDirectory { from: was.clone(), to: now.clone() });
            }
            actions.push(announce_vanished(onedrive_id, old, was));
            actions.push(announce_appeared(new, now));
        }
        OneDriveChange::Stayed { onedrive_id, new, at, .. } => {
            if change.has_new_content() {
                actions.push(CycleAction::RemoveDownloadedCopy { path: at.clone(), onedrive_id: onedrive_id.clone() });
            }
            if new.kind == Kind::File {
                actions.push(CycleAction::Announce(Announcement::FileShownChanged { path: at.clone(), onedrive_id: onedrive_id.clone() }));
            }
        }
    }
    actions
}

/// A directory gone from OneDrive, which the end of the step removes if it is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) struct RemovedDirectory {
    pub(in crate::fuse) onedrive_id: String,
    pub(in crate::fuse) path: PathBuf,
}

/// The directories gone from OneDrive in the order they are removed: deepest first, so that
/// one goes once what was below it went. Directories of one depth keep their order.
pub(in crate::fuse) fn order_deepest_first(mut removed_directories: Vec<RemovedDirectory>) -> Vec<RemovedDirectory> {
    removed_directories.sort_by_key(|directory| std::cmp::Reverse(directory.path.components().count()));
    removed_directories
}

#[cfg(test)]
mod tests;
