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
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;

use konedrive_tree::{ItemKind, ItemPlace, LocalItemId};

/// One change of the folder to announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemAnnouncement {
    /// `item` is at `place` now and was not shown before.
    Appeared { item: LocalItemId, place: ItemPlace },
    /// `item` was at `former_place` and is not there any more.
    Vanished { item: LocalItemId, former_place: ItemPlace, kind: ItemKind },
    /// The content or the times of the file `item`, at `place`, changed.
    ContentChanged { item: LocalItemId, place: ItemPlace },
}

/// The announcements of one step: a move is a vanishing and an appearing.
pub(super) fn describe_move(item: LocalItemId, kind: ItemKind, former_place: ItemPlace, place: ItemPlace) -> Vec<ItemAnnouncement> {
    todo!()
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
    todo!()
}
