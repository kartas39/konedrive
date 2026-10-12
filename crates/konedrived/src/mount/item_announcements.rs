//! What a step towards the folder changed, told to the kernel through the mount: the
//! inotify bridge's announcements by local item id and former name.
//!
//! **State owned:** none: the values the bridge is given, and the function that answers
//! the bridge's own lookup while a name is being announced. The queue, the announcement
//! in progress and the thread are `item_inotify_bridge`'s.
//!
//! **Answers for:** design §9, last paragraph: the bridge tells the kernel of what a step
//! changed "by local item id and the former name, with no table to look the node up in".
//! The former place is taken by the step before its commit: the rows give the new place
//! only (the map's E12).
//!
//! **Tested without the rest:**
// test: a_name_that_is_vanishing_is_answered_with_the_item_that_had_it
// test: a_name_that_is_appearing_is_not_there_yet_for_the_bridge
// test: any_other_name_is_answered_as_for_everyone
// test: an_item_moved_is_one_vanishing_and_one_appearing
// test: each_announcement_is_one_system_call_at_the_path_of_its_place
// test: an_announcement_whose_directory_or_item_the_rows_no_longer_show_is_no_system_call
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;
use std::path::PathBuf;

use konedrive_tree::{ItemKind, ItemPlace, LocalItemId};

/// One change of the folder to announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemAnnouncement {
    /// `item` is at `place` now and was not shown before.
    Appeared { item: LocalItemId, place: ItemPlace },
    /// `item` was at `former_place` and is not there any more.
    Vanished { item: LocalItemId, former_place: ItemPlace, kind: ItemKind },
    /// The content or the times of the file `item`, at `place`, changed.
    ContentChanged { item: LocalItemId, place: ItemPlace },
    /// What the mount answers as the sync mark or the pin of `item`, a file or a
    /// directory at `place`, changed (`user.konedrive.sync`, `user.konedrive.pin`): a step
    /// towards OneDrive began or ended for it, the plan's answer for it turned to or from
    /// a wait for the user, a pin was set or taken off. Its content did not change:
    /// nothing the kernel cached of it is dropped.
    MarksChanged { item: LocalItemId, place: ItemPlace },
}

/// The announcements of one step: a move is a vanishing and an appearing.
pub(crate) fn describe_move(item: LocalItemId, kind: ItemKind, former_place: ItemPlace, place: ItemPlace) -> Vec<ItemAnnouncement> {
    vec![ItemAnnouncement::Vanished { item, former_place, kind }, ItemAnnouncement::Appeared { item, place }]
}

impl ItemAnnouncement {
    /// The item the announcement is of.
    pub(super) fn item(&self) -> LocalItemId {
        match self {
            ItemAnnouncement::Appeared { item, .. } | ItemAnnouncement::Vanished { item, .. } | ItemAnnouncement::ContentChanged { item, .. } | ItemAnnouncement::MarksChanged { item, .. } => *item,
        }
    }

    /// The place the announcement names: where the item is now, or where it was.
    pub(super) fn place(&self) -> &ItemPlace {
        match self {
            ItemAnnouncement::Appeared { place, .. } | ItemAnnouncement::ContentChanged { place, .. } | ItemAnnouncement::MarksChanged { place, .. } => place,
            ItemAnnouncement::Vanished { former_place, .. } => former_place,
        }
    }
}

/// The system call the bridge's thread makes by path, through the mount, to have the
/// kernel tell its watchers of one announcement. Each path is relative to the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AnnouncementSystemCall {
    /// `mkdir`: a directory appeared.
    Mkdir(PathBuf),
    /// `mknod` of a regular file: a file appeared.
    Mknod(PathBuf),
    /// `rmdir`: a directory vanished.
    Rmdir(PathBuf),
    /// `unlink`: a file vanished.
    Unlink(PathBuf),
    /// `utimensat` with the time of now: the content or the times of a file changed, or
    /// the marks of a file or a directory.
    Utimensat(PathBuf),
}

/// The system call of `announcement`, given what the rows say now: `directory_path`, the
/// path of the directory its place is in (`ItemReader::find_path`; `None` when the folder
/// does not show that directory any more), and `kind_now`, the kind of the item when the
/// folder still shows it. `None` when there is nothing to make the call at or of: a name
/// in a directory that is gone went with it for the kernel, and an item that is gone
/// again has a later announcement of its own. Pure.
///
/// An appearing takes its kind from the rows, since the announcement carries none; a
/// vanishing from the announcement, since the rows may not have the item any more.
pub(super) fn decide_system_call(announcement: &ItemAnnouncement, directory_path: Option<PathBuf>, kind_now: Option<ItemKind>) -> Option<AnnouncementSystemCall> {
    let path = directory_path?.join(&announcement.place().name);
    Some(match (announcement, kind_now) {
        (ItemAnnouncement::Vanished { kind: ItemKind::Directory, .. }, _) => AnnouncementSystemCall::Rmdir(path),
        (ItemAnnouncement::Vanished { kind: ItemKind::File, .. }, _) => AnnouncementSystemCall::Unlink(path),
        (ItemAnnouncement::Appeared { .. }, Some(ItemKind::Directory)) => AnnouncementSystemCall::Mkdir(path),
        (ItemAnnouncement::Appeared { .. }, Some(ItemKind::File)) => AnnouncementSystemCall::Mknod(path),
        (ItemAnnouncement::ContentChanged { .. }, Some(ItemKind::File)) => AnnouncementSystemCall::Utimensat(path),
        // A touch of a file or of a directory alike: the mount answers the bridge's own `utimensat` from the rows and writes none (`ItemKernelRequests::queue_set_modified_time` asks whose thread it is before any job, whatever the kind), and the bridge drops no cached page for it (`ItemInotifyBridge::perform_queued`).
        (ItemAnnouncement::MarksChanged { .. }, Some(_)) => AnnouncementSystemCall::Utimensat(path),
        (ItemAnnouncement::Appeared { .. } | ItemAnnouncement::MarksChanged { .. }, None) | (ItemAnnouncement::ContentChanged { .. }, None | Some(ItemKind::Directory)) => return None,
    })
}

/// What the bridge's own lookup of `name` in `parent` is answered while `in_progress` is
/// being announced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AnnouncedName {
    /// The name is vanishing: answered as the item that had it, of this kind, with no
    /// lifetime, so that the kernel can tell its watchers.
    Vanishing { item: LocalItemId, kind: ItemKind },
    /// The name is appearing: not there yet.
    Appearing,
}

/// The answer to the bridge's thread for the one name it is announcing; `None` for any
/// other name, which is answered from the rows as for everyone. Pure. Called by lookup
/// when the request comes from the bridge's own thread.
pub(super) fn find_announced_name(in_progress: Option<&ItemAnnouncement>, parent: LocalItemId, name: &OsStr) -> Option<AnnouncedName> {
    let in_progress = in_progress?;
    let place = in_progress.place();
    if place.parent_local_item_id != parent || place.name != name {
        return None;
    }
    match in_progress {
        ItemAnnouncement::Vanished { item, kind, .. } => Some(AnnouncedName::Vanishing { item: *item, kind: *kind }),
        ItemAnnouncement::Appeared { .. } => Some(AnnouncedName::Appearing),
        // The file is there before and after: its touch is answered from its row.
        ItemAnnouncement::ContentChanged { .. } | ItemAnnouncement::MarksChanged { .. } => None,
    }
}

#[cfg(test)]
mod tests;
