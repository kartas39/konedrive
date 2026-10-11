//! What a step remembers beyond the versions: what it keeps of an item between tries and
//! across a stop of the daemon, by local item id, apart from the rows of `items`. It is no
//! source of truth for what an item is: the row is, and a fact is answered only for what it
//! was learnt for, which the rows say ([`item_memory`]).
//!
//! **Five tables**, and why more than one. `step_memory` has one row per item, every fact
//! of the item in it. Three things a step keeps are not of the item they came from, and
//! would be lost with its row:
//!
//! - an upload session the folder was about to open is of the place its empty file takes in
//!   OneDrive (`upload_session_openings`, [`upload_session_openings`]): the file that asked
//!   may be removed or renamed while the empty file still holds the name, and the next file
//!   of that name must be able to tell it is the folder's own;
//! - the address of an upload session nothing goes on with is what is left to cancel
//!   (`upload_session_cancellations`, [`upload_session_cancellations`]): it is kept until
//!   the session is cancelled, so that its empty file does not hold a name for days;
//! - the item an upload left in OneDrive with other content than was sent is left to remove
//!   also when the file's row went first (`mismatched_item_removals`,
//!   [`mismatched_item_removals`]).
//! A request that changes OneDrive is on record until it is settled ([`request_records`]).
//!
//! **Every fact names what voids it**, and the voider is something that happens for every
//! way the fact can stop being true. What a fact says of OneDrive is void when OneDrive
//! has answered again about that item: by the step's own next answer, or by a listing
//! that names the item, changed or removed — in the transaction that writes the page
//! (`onedrive_versions`, and the schema's triggers on `items` for what any writer of a
//! row voids). Each type says its voiders where it is declared ([`item_memory`],
//! [`upload_session_openings`], [`upload_session_cancellations`],
//! [`mismatched_item_removals`]). A fact that can name no such voider is not kept here:
//! the failed tries of a step towards the folder or of the rows are kept by the plan's
//! task, with the versions the step was planned from, for its run.
//!
//! **The two kinds of fact, and what each is answered by.**
//!
//! - *A fact of a local change that is not sent* — the upload session, the failed tries
//!   of what was sent with their wait, the refusal for lack of space, the refusal for good, the taken name,
//!   the confirmation of a removal — is learnt under the stamp of the row the step was
//!   planned from ([`ItemVersions::unsent_since`](crate::ItemVersions), which the caller
//!   gives), is not written when the row has another stamp or none, and is answered only
//!   while the row has that stamp ([`ItemMemory::split_by_standing`], the one decision). A
//!   row with nothing unsent answers none of them, so a commit that makes the two sides
//!   agree forgets them by that alone. Every write for an item first takes away what its
//!   row does not say any more ([`change_memory`]); the first call of a new try of an
//!   upload ([`remember_upload_session_opening`]) is such a write.
//! - *A fact of what OneDrive has* is of the binding, or of an item of OneDrive by its own
//!   id. An operation on an item's binding ([`follow_binding`], [`drop_facts_of_binding`],
//!   called by the operations on bindings in their transaction) drops every fact of an
//!   unsent change from the items it changes — each was an answer about the item as it was
//!   bound —, the upload session to the cancellations, and the temporary name goes with
//!   the binding. The item an upload left is void once the folder has that item, which the
//!   schema holds whoever writes the row ([`mismatched_item_removals`]), and once a
//!   listing says it has another content or is gone. A name found taken is of both kinds:
//!   it is answered while the item is the unsent change at the place that was sent, and
//!   is void once OneDrive answers again about the name ([`onedrive_answers`]). When a directory
//!   gives its binding up, what OneDrive answered about sending items into it — a
//!   refusal, a taken name, the session of a new file — is dropped from those items, and
//!   the openings and empty files in it are with the row that has the binding.
//!
//! **What else holds**, and who holds it:
//!
//! - *An address always has somewhere to go.* An upload session is the item's or its address
//!   is among the cancellations, never neither: a session taken away because the row does
//!   not say it, one given for an item whose row went or is another unsent change, one
//!   replaced by another, one whose item's binding changed. The one exception is said by
//!   the caller: a session that ended ([`forget_ended_upload_session`], which the commit
//!   of an upload calls with the address it is told). The reader of the sessions at a
//!   place answers a session as the item's only where the item's memory answers it too.
//! - *A fact does not outlive its item.* The schema's trigger `step_memory_item_gone`, run
//!   when a row of `items` is deleted, whoever deletes it ([`schema`]).
//! - *What a confirmed request ends where the stamp does not say it* is forgotten with its
//!   commit ([`forget_confirmed_request`]): the item may be unsent still, changed again
//!   while the request ran. And the same bytes told by their hash go on being the bytes a
//!   fact holds ([`carry_content`]).
//! - *A crash loses nothing that costs the user.* Every operation is one transaction; a
//!   step writes an opening before the request that opens a session, the session before its
//!   first byte, and a temporary name before the request that takes it.

use std::ffi::OsStr;

use konedrive_reason::Reason;
use rusqlite::types::Value;
use rusqlite::{Connection, Transaction};

use crate::item_store::find_item;
use crate::item_store::refuse;
use crate::model::{ContentVersion, Item, ItemPlace, LocalItemId, OneDriveItemReference};
use crate::{TreeError, TreeStore};

/// What the steps remember, read through the reader.
mod memory_reader;
/// What is remembered of one item, as values, and whether its row still says it.
mod item_memory;
/// A row of `step_memory`.
mod memory_row;
/// What an answer of OneDrive about an item voids of what is remembered.
mod onedrive_answers;
/// The items uploads left with other content whose file's row went, until each is removed.
mod mismatched_item_removals;
/// The items OneDrive refused, with their reasons, listed and counted.
mod refusal_reasons;
/// The requests that changed OneDrive and are not settled yet.
mod request_records;
/// The cancels of upload sessions that are left to send.
mod upload_session_cancellations;
/// The upload sessions the folder was about to open, by place and time.
mod upload_session_openings;

pub(crate) use item_memory::ConfirmedRequest;
pub use item_memory::{
    is_content_of, FactStanding, FailedTries, FailedTryStanding, ItemMemory, ItemMemoryParts, MismatchedItem, MismatchedItemStanding, Refusal, RefusedVersion, RemovalConfirmation, SpaceFit, SpaceRefusal,
    StepFact, TakenName, TemporaryName, UnsentRemoval, UploadSession, UploadSessionAddress, UploadSessionStanding, UploadTag,
};
pub use refusal_reasons::ItemRefusalReason;
pub use request_records::{RecordedRequest, RequestRecord};
pub use upload_session_openings::{OpeningTry, OpeningWindow, UPLOAD_SESSION_OPENING_KEPT};

pub(crate) use mismatched_item_removals::list_removals as list_mismatched_item_removals;
pub(crate) use onedrive_answers::{
    forget_mismatched_item_listed_changed, forget_removal_confirmations_above, forget_taken_names_onedrive_lacks, forget_temporary_name_unless_at, forget_temporary_names_of_items_onedrive_lacks,
};
pub(crate) use refusal_reasons::{count_refusal_reasons, count_space_refusals, list_refusal_reasons, list_space_refusal_reasons};
pub(crate) use request_records::list_records as list_request_records;
pub(crate) use upload_session_cancellations::list_cancellations as list_upload_session_cancellations;
pub(crate) use upload_session_openings::{forget_openings_at, forget_openings_before, list_opening_windows, settle_answered_opening};

use memory_row::{folded_name_value, list_memories, read_memory, write_memory};
use mismatched_item_removals::{find_removal, forget_removal, is_item_of_folder, leave_removal, list_files_of};
use upload_session_cancellations::{abandon, abandon_address, forget_cancellation, list_cancellations_at, move_empty_files};
use upload_session_openings::{forget_opening_of, move_openings, remember_opening};

/// What a store is created with for what the steps remember: the five tables, each as its
/// module has it, and the trigger `step_memory_item_gone`, which holds "a fact does not
/// outlive its item" whoever deletes a row of `items`; and the triggers of
/// `mismatched_item_removals`, which hold that an item the folder has is no item an upload
/// left. Each table says what the trigger
/// does to it; `step_memory` comes last, after the tables that keep something of its row
/// past the item took it.
pub(crate) fn schema() -> String {
    format!(
        "{}{}{}{}{}
         CREATE TRIGGER step_memory_item_gone AFTER DELETE ON items BEGIN{}{}{}{}{}
         END;",
        memory_row::schema(),
        upload_session_openings::schema(),
        upload_session_cancellations::schema(),
        mismatched_item_removals::schema(),
        request_records::schema(),
        request_records::ITEM_GONE_STATEMENTS,
        upload_session_cancellations::ITEM_GONE_STATEMENTS,
        mismatched_item_removals::item_gone_statements(),
        upload_session_openings::ITEM_GONE_STATEMENTS,
        memory_row::item_gone_statements(),
    ) + &onedrive_answers::taken_name_triggers()
}

/// An item of OneDrive an upload left with other content than was sent, as it is found by
/// its own reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MismatchedItemUpload {
    pub mismatched_item: MismatchedItem,
    /// The file whose upload left it, whose step removes it before the file is sent again;
    /// none when that file's row went, and the item is left to remove
    /// ([`StepMemory::list_mismatched_item_removals`]).
    pub file: Option<LocalItemId>,
}

/// An item that waits until a time: the next try of its step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedWait {
    pub item: LocalItemId,
    /// Unix seconds.
    pub until: i64,
}

/// A name OneDrive answered is taken, with the item that was sent under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemTakenName {
    pub item: LocalItemId,
    pub taken_name: TakenName,
}

/// An upload session whose empty file holds a place in OneDrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmptyFileSession {
    /// The session an item's step goes on with: the item's memory answers it.
    /// `expires_at` is when OneDrive ends the session by itself, Unix seconds, `None` when
    /// it did not say: what the step of another item that waits for the name waits until,
    /// also when this item has no step that would learn that the session ended.
    Continued { item: LocalItemId, address: UploadSessionAddress, expires_at: Option<i64> },
    /// A session nothing goes on with, to be cancelled: its cancel is left to send, or its
    /// item's row does not say it any more and the next write for that item, or the next
    /// opening at the place, leaves the cancel.
    Abandoned { address: UploadSessionAddress },
}

/// What the steps remember, for one job of the store's thread: borrowed from the store as
/// [`ItemStore`](crate::ItemStore) is ([`TreeStore::step_memory`]). Each operation is one
/// transaction.
pub struct StepMemory<'a> {
    store: &'a mut TreeStore,
}

impl TreeStore {
    /// What the steps remember beyond the versions.
    pub fn step_memory(&mut self) -> StepMemory<'_> {
        StepMemory { store: self }
    }
}

impl StepMemory<'_> {
    /// Everything remembered of `item` that its row still says, as one value; nothing for
    /// an item with nothing kept and for an id with no row.
    pub fn find_item_memory(&self, item: LocalItemId) -> Result<ItemMemory, TreeError> {
        find_item_memory(&self.store.conn, item)
    }

    /// The items whose step towards OneDrive waits until a time, soonest first, at most
    /// `limit` of them. Found by an index; an item whose row is not the unsent change the
    /// tries were of any more — another, or none — is not among them.
    pub fn list_timed_waits(&self, limit: usize) -> Result<Vec<TimedWait>, TreeError> {
        list_timed_waits(&self.store.conn, limit)
    }

    /// The items OneDrive refuses for good, each with its reason, by local item id, after
    /// the item `after` when one is given, at most `limit` of them: those refused for
    /// `reason`, whatever its detail, or for any reason when none is given. Found by an
    /// index; an item whose row does not say what was refused any more is not among them.
    pub fn list_refusal_reasons(&self, reason: Option<&Reason>, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ItemRefusalReason>, TreeError> {
        list_refusal_reasons(&self.store.conn, reason, after, limit)
    }

    /// How many items [`list_refusal_reasons`](Self::list_refusal_reasons) lists for
    /// `reason` in all. Every row of the index is read.
    pub fn count_refusal_reasons(&self, reason: Option<&Reason>) -> Result<u64, TreeError> {
        count_refusal_reasons(&self.store.conn, reason)
    }

    /// The files OneDrive refused as too big for the space it has, each with the reason
    /// `too-big`, what the file needs and the free space, by local item id, after the item
    /// `after` when one is given, at most `limit` of them. Found by an index; a file whose
    /// bytes are not the refused ones any more is not among them.
    pub fn list_space_refusal_reasons(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ItemRefusalReason>, TreeError> {
        list_space_refusal_reasons(&self.store.conn, after, limit)
    }

    /// How many files [`list_space_refusal_reasons`](Self::list_space_refusal_reasons)
    /// lists in all. Every row of the index is read.
    pub fn count_space_refusals(&self) -> Result<u64, TreeError> {
        count_space_refusals(&self.store.conn)
    }

    /// The items whose removal the user confirmed, by local item id, after the item `after`
    /// when one is given, at most `limit` of them: what a plan is given so that the guard
    /// against a mass removal leaves them out. Found by an index; an item whose row is not
    /// the removal that was confirmed any more is not among them.
    pub fn list_items_with_confirmed_removal(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<LocalItemId>, TreeError> {
        list_items_with_confirmed_removal(&self.store.conn, after, limit)
    }

    /// The names OneDrive answered are taken in `directory`, each with the item that was
    /// sent there, by local item id: what a plan treats as one name with its holder's.
    pub fn list_taken_names(&self, directory: LocalItemId) -> Result<Vec<ItemTakenName>, TreeError> {
        list_taken_names(&self.store.conn, directory)
    }

    /// The upload sessions whose empty file holds `place` in OneDrive, as OneDrive compares
    /// names: those items' steps go on with, and those nothing goes on with.
    pub fn list_empty_file_sessions(&self, place: &ItemPlace) -> Result<Vec<EmptyFileSession>, TreeError> {
        list_empty_file_sessions(&self.store.conn, place)
    }

    /// The times between which an empty file at `place` may be this folder's own: one
    /// window for each upload session the folder was about to open there whose address
    /// never came. Never between two windows.
    pub fn list_opening_windows(&self, place: &ItemPlace) -> Result<Vec<OpeningWindow>, TreeError> {
        list_opening_windows(&self.store.conn, place)
    }

    /// Up to `limit` upload sessions whose cancel is left to send, the one abandoned first
    /// first.
    pub fn list_upload_session_cancellations(&self, limit: usize) -> Result<Vec<UploadSessionAddress>, TreeError> {
        list_upload_session_cancellations(&self.store.conn, limit)
    }

    /// Up to `limit` items uploads left in OneDrive with other content than was sent whose
    /// file's row went before they were removed, the one left first first, each with its
    /// drive: to be removed while they are still what the upload left
    /// ([`MismatchedItem::is_unchanged_since_upload`]). An item the folder has — a row is
    /// bound to it, and the folder shows it or the two sides agreed on a place of it — is
    /// never among them.
    pub fn list_mismatched_item_removals(&self, limit: usize) -> Result<Vec<MismatchedItem>, TreeError> {
        list_mismatched_item_removals(&self.store.conn, limit)
    }

    /// Whether the item `onedrive_item` of OneDrive is one an upload of this folder left
    /// with other content than was sent, and of which file: what a plan asks of an item a
    /// listing brought before it shows it, merges a file with it or sets a file aside for
    /// it. `None` for an item no upload left, and for one the folder has by now.
    pub fn find_mismatched_item(&self, onedrive_item: &OneDriveItemReference) -> Result<Option<MismatchedItemUpload>, TreeError> {
        find_mismatched_item(&self.store.conn, onedrive_item)
    }

    /// The step of `item` is about to open the upload session of a new file at `place`: kept
    /// before the request, at the time `now`. The first call of a new try: an upload session
    /// the item's row does not say any more is abandoned here, and so is one of another
    /// item whose empty file holds `place` and whose row does not say it. A session the
    /// item's row still says stays. Refused for an id with no row.
    pub fn remember_upload_session_opening(&mut self, item: LocalItemId, place: &ItemPlace, now: i64) -> Result<OpeningTry, TreeError> {
        self.write(|tx| remember_upload_session_opening(tx, item, place, now))
    }

    /// The try `opening_try` of `item` got a certain answer, and left no empty file.
    pub fn settle_answered_opening(&mut self, item: LocalItemId, opening_try: OpeningTry) -> Result<(), TreeError> {
        self.write(|tx| settle_answered_opening(tx, item, opening_try))
    }

    /// The openings kept at `place` are resolved, every one: the empty file was removed, the
    /// name was found free, or what holds it is not this folder's.
    pub fn forget_upload_session_openings_at(&mut self, place: &ItemPlace) -> Result<(), TreeError> {
        self.write(|tx| forget_openings_at(tx, place))
    }

    /// The openings that name no file and whose latest try was before `time` go
    /// ([`UPLOAD_SESSION_OPENING_KEPT`] before now). How many.
    pub fn forget_upload_session_openings_before(&mut self, time: i64) -> Result<usize, TreeError> {
        self.write(|tx| forget_openings_before(tx, time))
    }

    /// OneDrive opened `session` for `item`, whose step was planned from a row stamped
    /// `unsent_since`: given before its first byte is sent. After the call the address is
    /// the item's or among the cancellations. *Continued*: the session is the item's from
    /// now on, and a session of another address it had is abandoned. *Abandoned*: the
    /// item's row went, or has another stamp or none, or the bytes the session sends are
    /// not the file's any more; nothing is kept of it with the item, its cancel is left to
    /// send, and the step sends nothing to it. The opening kept for the item goes either
    /// way: the address tells the empty file from now on.
    pub fn remember_upload_session(&mut self, item: LocalItemId, unsent_since: Option<u64>, session: &UploadSession) -> Result<UploadSessionStanding, TreeError> {
        self.write(|tx| remember_upload_session(tx, item, unsent_since, session))
    }

    /// The session of `item` at `address` got as far as `next_offset`, and ends by itself at
    /// `expires_at`. *Abandoned* when the item's row does not say that session any more, or
    /// the item has none at that address, or no row: nothing is written, the cancel of the
    /// session at `address` is left to send, and the step sends nothing more to it.
    /// *OfAnotherItem*, with nothing written and nothing queued, when `address` is the
    /// session another item's memory keeps: the caller named the wrong item, and that
    /// session is not this call's to cancel.
    pub fn advance_upload_session(&mut self, item: LocalItemId, address: &UploadSessionAddress, next_offset: u64, expires_at: Option<i64>) -> Result<UploadSessionStanding, TreeError> {
        self.write(|tx| advance_upload_session(tx, item, address, next_offset, expires_at))
    }

    /// The session of `item` is given up before it ended: its cancel is left to send. For a
    /// session that ended, [`forget_ended_upload_session`](Self::forget_ended_upload_session).
    pub fn abandon_upload_session(&mut self, item: LocalItemId) -> Result<(), TreeError> {
        self.write(|tx| abandon_upload_session(tx, item))
    }

    /// The session of `item` at `address` ended — OneDrive answered its last fragment,
    /// answered that it does not have the session, or the time it said the session ends at
    /// has passed — and is forgotten, with nothing left to cancel. As
    /// [`advance_upload_session`](Self::advance_upload_session), it acts on the session the
    /// item's memory keeps and on no other: another session the item has stays, a session
    /// another item keeps stays, and a cancel that was left of `address` before stays for
    /// the loop that sends the cancels, which is answered that the session is gone and
    /// forgets it ([`forget_upload_session_cancellation`](Self::forget_upload_session_cancellation)).
    /// For a session given up, [`abandon_upload_session`](Self::abandon_upload_session).
    pub fn forget_ended_upload_session(&mut self, item: LocalItemId, address: &UploadSessionAddress) -> Result<(), TreeError> {
        self.write(|tx| forget_ended_upload_session(tx, item, address))
    }

    /// The session at `address` was cancelled, or found gone: its cancel is not left to send
    /// any more.
    pub fn forget_upload_session_cancellation(&mut self, address: &UploadSessionAddress) -> Result<(), TreeError> {
        self.write(|tx| forget_upload_session_cancellation(tx, address))
    }

    /// An upload of `item` left `upload` in OneDrive with other content than was sent: kept
    /// before its removal is asked. *OfFolder*, with nothing kept, for an item the folder
    /// has: its removal is not asked. An item an earlier upload of the same file left, kept
    /// until now, is left to remove by itself
    /// ([`list_mismatched_item_removals`](Self::list_mismatched_item_removals)). Refused
    /// for an id with no row.
    pub fn remember_mismatched_item(&mut self, item: LocalItemId, upload: &MismatchedItem) -> Result<MismatchedItemStanding, TreeError> {
        self.write(|tx| remember_mismatched_item(tx, item, upload))
    }

    /// The item `onedrive_item`, which an upload left, is not this folder's to remove any
    /// more (changed by another, or adopted): forgotten wherever it is kept. OneDrive has
    /// it still, and a row a listing made of it stays. For one removed or found gone,
    /// [`forget_removed_mismatched_item`](Self::forget_removed_mismatched_item).
    pub fn forget_mismatched_item(&mut self, onedrive_item: &OneDriveItemReference) -> Result<(), TreeError> {
        self.write(|tx| forget_mismatched_item(tx, onedrive_item))
    }

    /// OneDrive answered that the item `onedrive_item`, which an upload left, is removed
    /// or gone: forgotten, and in the same transaction the row a listing made of it loses
    /// its OneDrive version ([`mismatched_item_removals::forget_removed_item`]).
    pub fn forget_removed_mismatched_item(&mut self, onedrive_item: &OneDriveItemReference) -> Result<(), TreeError> {
        self.write(|tx| mismatched_item_removals::forget_removed_item(tx, onedrive_item))
    }

    /// `item` goes through `temporary_name` in OneDrive: kept before the request that takes
    /// the name, and answered for the row that has the item's binding until OneDrive
    /// confirmed the item at another place or removed, or its step forgets it. Refused for
    /// an id with no row.
    pub fn remember_temporary_name(&mut self, item: LocalItemId, temporary_name: &TemporaryName) -> Result<(), TreeError> {
        self.write(|tx| remember_temporary_name(tx, item, temporary_name))
    }

    /// A try of the step of `item` towards OneDrive, planned from a row stamped
    /// `unsent_since`, failed for `reason`; the next is due at `next_try_at`, or at once.
    /// *Counted*, with how many tries of that unsent change failed, this one among them.
    /// *ChangeGone*, with nothing written, when the row has another stamp or none: the
    /// tries of one piece of work are not counted on to the next. Only a step towards
    /// OneDrive gives its tries here: the tries of a step towards the folder or of the rows
    /// are the plan's task's, for its run. Refused for an id with no row.
    pub fn remember_failed_try(&mut self, item: LocalItemId, unsent_since: Option<u64>, reason: &Reason, next_try_at: Option<i64>) -> Result<FailedTryStanding, TreeError> {
        self.write(|tx| remember_failed_try(tx, item, unsent_since, reason, next_try_at))
    }

    /// Every item that waits until a time is due now, with its count and its reason kept:
    /// the user asked for a refresh, or the network is back. How many.
    pub fn clear_next_tries(&mut self) -> Result<usize, TreeError> {
        self.write(|tx| clear_next_tries(tx))
    }

    /// OneDrive refused the content of `item`, sent by a step planned from a row stamped
    /// `unsent_since`, for lack of space. Not written when the row has another stamp or
    /// none, or the bytes that were refused are not the file's any more. Refused for an id
    /// with no row.
    pub fn remember_space_refusal(&mut self, item: LocalItemId, unsent_since: Option<u64>, space_refusal: &SpaceRefusal) -> Result<FactStanding, TreeError> {
        self.write(|tx| remember_space_refusal(tx, item, unsent_since, space_refusal))
    }

    /// OneDrive has `free_space` bytes free, as a quota read said: the refusal for lack of
    /// space of every file that fits now is forgotten, and the rest are too big for this
    /// much. How many were forgotten.
    pub fn forget_fitting_space_refusals(&mut self, free_space: u64) -> Result<usize, TreeError> {
        self.write(|tx| forget_fitting_space_refusals(tx, free_space))
    }

    /// The user confirmed `removals`, which a mass removal held: written of each whose row
    /// is still that removal, with the stamp it was held with, and of no other. Those that
    /// were not written, each a row that is another removal by now, or none.
    pub fn remember_removals_confirmed(&mut self, removals: &[UnsentRemoval]) -> Result<Vec<UnsentRemoval>, TreeError> {
        self.write(|tx| remember_removals_confirmed(tx, removals))
    }

    /// OneDrive refuses for good what a step planned from a row stamped `unsent_since` sent
    /// of `item`. Not written when the row has another stamp or none, or does not say what
    /// was refused any more: its version at that place, or a removal. Refused for an id
    /// with no row, and for the root.
    pub fn remember_refusal(&mut self, item: LocalItemId, unsent_since: Option<u64>, refusal: &Refusal) -> Result<FactStanding, TreeError> {
        self.write(|tx| remember_refusal(tx, item, unsent_since, refusal))
    }

    /// Every refusal for good whose reason is `reason`, whatever its detail, is forgotten:
    /// the account signed in again, and what was not allowed may be now. How many.
    pub fn forget_refusals_for(&mut self, reason: &Reason) -> Result<usize, TreeError> {
        self.write(|tx| forget_refusals_for(tx, reason))
    }

    /// OneDrive answered that the name `item` was sent under, by a step planned from a row
    /// stamped `unsent_since`, is taken. Not written when the row has another stamp or
    /// none, or the item's local place is not the one that was sent any more. Refused for
    /// an id with no row.
    pub fn remember_taken_name(&mut self, item: LocalItemId, unsent_since: Option<u64>, taken_name: &TakenName) -> Result<FactStanding, TreeError> {
        self.write(|tx| remember_taken_name(tx, item, unsent_since, taken_name))
    }

    /// A step is about to send `record`'s request: written before the request leaves,
    /// over a record its item had ([`request_records`]). Refused for an id with no row.
    pub fn remember_request_record(&mut self, record: &RequestRecord) -> Result<(), TreeError> {
        self.write(|tx| request_records::remember_record(tx, record))
    }

    /// The request of `item` is settled by its own step: its record goes.
    pub fn forget_request_record(&mut self, item: LocalItemId) -> Result<(), TreeError> {
        self.write(|tx| request_records::forget_record(tx, item))
    }

    /// `record`, read some time ago, is settled: it goes when it is still its item's record.
    pub fn forget_request_record_as_read(&mut self, record: &RequestRecord) -> Result<bool, TreeError> {
        self.write(|tx| request_records::forget_record_as_read(tx, record))
    }

    /// `fact` of `item` is forgotten: its step is done with it. Nothing for an id with no
    /// row.
    pub fn forget_fact(&mut self, item: LocalItemId, fact: StepFact) -> Result<(), TreeError> {
        self.write(|tx| forget_fact(tx, item, fact))
    }

    /// Runs `work` in a transaction of its own and commits what it did; an error leaves the
    /// tables as they were.
    fn write<T>(&mut self, work: impl FnOnce(&Transaction<'_>) -> Result<T, TreeError>) -> Result<T, TreeError> {
        let tx = self.store.conn.transaction()?;
        let done = work(&tx)?;
        tx.commit()?;
        Ok(done)
    }
}

/// [`StepMemory::find_item_memory`], inside the caller's transaction.
pub(crate) fn find_item_memory(conn: &Connection, item: LocalItemId) -> Result<ItemMemory, TreeError> {
    let Some(row) = find_item(conn, item)? else { return Ok(ItemMemory::default()) };
    Ok(read_memory(conn, item)?.split_by_standing(&row).said)
}

/// [`StepMemory::list_timed_waits`], inside the caller's transaction.
pub(crate) fn list_timed_waits(conn: &Connection, limit: usize) -> Result<Vec<TimedWait>, TreeError> {
    // The tries are asked the stamp and nothing more, so the rows are asked in the
    // statement.
    let mut statement = conn.prepare_cached(
        "SELECT local_item_id, next_try_at FROM step_memory INDEXED BY step_memory_next_try
          WHERE next_try_at IS NOT NULL
            AND unsent_since = (SELECT unsent_since FROM items WHERE items.local_item_id = step_memory.local_item_id)
          ORDER BY next_try_at, local_item_id LIMIT ?1",
    )?;
    let waits = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| Ok(TimedWait { item: LocalItemId::new(row.get(0)?), until: row.get(1)? }))?;
    Ok(waits.collect::<Result<Vec<_>, _>>()?)
}

/// How many rows of an index a bounded reader reads at a time.
const PORTION: usize = 256;

/// The items whose row of `step_memory` meets `condition`, which an index holds, and whose
/// row of `items` still says the fact `pick` takes from what is said: by local item id,
/// after the item `after` when one is given, at most `limit` of them, each with that fact.
/// The index is read in portions until enough rows say their fact or it ends.
fn list_said_facts<T>(conn: &Connection, condition: &str, after: Option<LocalItemId>, limit: usize, pick: impl Fn(ItemMemory) -> Option<T>) -> Result<Vec<(LocalItemId, T)>, TreeError> {
    let mut facts = Vec::new();
    let mut after = after.map_or(0, LocalItemId::as_i64);
    let sql = format!("{condition} AND local_item_id > ?1 ORDER BY local_item_id LIMIT ?2");
    while facts.len() < limit {
        let portion = list_memories(conn, &sql, &[after.into(), (PORTION as i64).into()])?;
        let is_last_portion = portion.len() < PORTION;
        for (item, memory) in portion {
            after = item.as_i64();
            if facts.len() == limit {
                break;
            }
            let said = find_item(conn, item)?.map(|row| memory.split_by_standing(&row).said);
            facts.extend(said.and_then(&pick).map(|fact| (item, fact)));
        }
        if is_last_portion {
            break;
        }
    }
    Ok(facts)
}

/// [`StepMemory::list_items_with_confirmed_removal`], inside the caller's transaction.
pub(crate) fn list_items_with_confirmed_removal(conn: &Connection, after: Option<LocalItemId>, limit: usize) -> Result<Vec<LocalItemId>, TreeError> {
    let confirmed = list_said_facts(conn, "is_removal_confirmed IS NOT NULL", after, limit, |said| said.removal_confirmation)?;
    Ok(confirmed.into_iter().map(|(item, _)| item).collect())
}

/// [`StepMemory::list_taken_names`], inside the caller's transaction.
pub(crate) fn list_taken_names(conn: &Connection, directory: LocalItemId) -> Result<Vec<ItemTakenName>, TreeError> {
    let mut names = Vec::new();
    for (item, memory) in list_memories(conn, "taken_parent_id = ?1 ORDER BY local_item_id", &[directory.as_i64().into()])? {
        let said = find_item(conn, item)?.map(|row| memory.split_by_standing(&row).said);
        names.extend(said.and_then(|said| said.taken_name).map(|taken_name| ItemTakenName { item, taken_name }));
    }
    Ok(names)
}

/// The rows of `step_memory` that hold an upload session whose empty file is at `place`.
fn list_memories_with_empty_file_at(conn: &Connection, place: &ItemPlace) -> Result<Vec<(LocalItemId, ItemMemory)>, TreeError> {
    let at_place = "session_empty_file_parent_id = ?1 AND session_empty_file_folded_name = ?2 ORDER BY local_item_id";
    list_memories(conn, at_place, &[place.parent_local_item_id.as_i64().into(), folded_name_value(&place.name)])
}

/// [`StepMemory::list_empty_file_sessions`], inside the caller's transaction.
pub(crate) fn list_empty_file_sessions(conn: &Connection, place: &ItemPlace) -> Result<Vec<EmptyFileSession>, TreeError> {
    let mut sessions = Vec::new();
    for (item, memory) in list_memories_with_empty_file_at(conn, place)? {
        let Some(session) = memory.upload_session.clone() else { continue };
        let is_said = find_item(conn, item)?.is_some_and(|row| memory.split_by_standing(&row).said.upload_session.is_some());
        sessions.push(if is_said { EmptyFileSession::Continued { item, address: session.address, expires_at: session.expires_at } } else { EmptyFileSession::Abandoned { address: session.address } });
    }
    sessions.extend(list_cancellations_at(conn, place)?.into_iter().map(|address| EmptyFileSession::Abandoned { address }));
    Ok(sessions)
}

/// The one way a memory is changed by a write for its item: what is kept of `item` is read,
/// what its row does not say any more is taken away — an upload session to the
/// cancellations —, `change` is applied to the rest, and the result is written. `None`,
/// with nothing kept, for an id with no row.
fn change_memory<T>(conn: &Connection, item: LocalItemId, change: impl FnOnce(&Item, &mut ItemMemory) -> Result<T, TreeError>) -> Result<Option<T>, TreeError> {
    let kept = read_memory(conn, item)?;
    let Some(row) = find_item(conn, item)? else {
        write_memory(conn, item, &ItemMemory::default())?;
        return Ok(None);
    };
    let mut parts = kept.clone().split_by_standing(&row);
    if let Some(session) = &parts.gone.upload_session {
        abandon(conn, session)?;
    }
    let done = change(&row, &mut parts.said)?;
    if parts.said != kept {
        write_memory(conn, item, &parts.said)?;
    }
    Ok(Some(done))
}

/// What the row of `item` does not say any more is taken away, and nothing else changes.
fn take_away_what_is_gone(conn: &Connection, item: LocalItemId) -> Result<(), TreeError> {
    change_memory(conn, item, |_, _| Ok(()))?;
    Ok(())
}

/// [`change_memory`] for a fact that is written: refused for an id with no row.
fn write_fact<T>(conn: &Connection, item: LocalItemId, change: impl FnOnce(&Item, &mut ItemMemory) -> Result<T, TreeError>) -> Result<T, TreeError> {
    change_memory(conn, item, change)?.ok_or_else(|| refuse(format!("item {item} has no row: nothing is remembered of it")))
}

/// A fact of an unsent change, learnt by a step planned from a row stamped `unsent_since`,
/// is written into `memory` by `write`, when `row` still says it: the row has that stamp,
/// and what the fact is asked beside the stamp holds, by the readers' own decision. Nothing
/// is written otherwise. `memory` is what the row still says, so every fact of an unsent
/// change in it is of this stamp already.
fn learn_of_unsent_change(row: &Item, memory: &mut ItemMemory, unsent_since: Option<u64>, write: impl FnOnce(&mut ItemMemory)) -> FactStanding {
    if unsent_since.is_none() || row.unsent_since != unsent_since {
        return FactStanding::VersionGone;
    }
    let mut learnt = memory.clone();
    write(&mut learnt);
    learnt.unsent_since = unsent_since;
    if learnt.clone().split_by_standing(row).said != learnt {
        return FactStanding::VersionGone;
    }
    *memory = learnt;
    FactStanding::StillSaid
}

/// [`StepMemory::remember_upload_session_opening`], inside the caller's transaction.
pub(crate) fn remember_upload_session_opening(conn: &Connection, item: LocalItemId, place: &ItemPlace, now: i64) -> Result<OpeningTry, TreeError> {
    for id in [item, place.parent_local_item_id] {
        if find_item(conn, id)?.is_none() {
            return Err(refuse(format!("item {id} has no row: no opening of an upload session is kept for it")));
        }
    }
    // The first write of a new try: for the item, and for every item whose session's empty
    // file may be what holds the name.
    take_away_what_is_gone(conn, item)?;
    for (holder, _) in list_memories_with_empty_file_at(conn, place)? {
        take_away_what_is_gone(conn, holder)?;
    }
    remember_opening(conn, item, place, now)
}

/// [`StepMemory::remember_upload_session`], inside the caller's transaction.
pub(crate) fn remember_upload_session(conn: &Connection, item: LocalItemId, unsent_since: Option<u64>, session: &UploadSession) -> Result<UploadSessionStanding, TreeError> {
    // An empty file is at a place only while the directory of that place has a row.
    let has_directory = |place: &ItemPlace| Ok::<bool, TreeError>(find_item(conn, place.parent_local_item_id)?.is_some());
    let mut session = session.clone();
    if !session.empty_file_place.as_ref().map(has_directory).transpose()?.unwrap_or(true) {
        session.empty_file_place = None;
    }
    let kept = change_memory(conn, item, |row, memory| {
        let earlier = memory.upload_session.clone();
        if learn_of_unsent_change(row, memory, unsent_since, |memory| memory.upload_session = Some(session.clone())) == FactStanding::VersionGone {
            return Ok(UploadSessionStanding::Abandoned);
        }
        if let Some(earlier) = earlier.filter(|earlier| earlier.address != session.address) {
            abandon(conn, &earlier)?;
        }
        Ok(UploadSessionStanding::Continued)
    })?;
    // No row, another unsent change, or other bytes: the address is nobody's, and is kept
    // to be cancelled.
    let standing = kept.unwrap_or(UploadSessionStanding::Abandoned);
    if standing == UploadSessionStanding::Abandoned {
        abandon(conn, &session)?;
    }
    // The address is known and kept, with the item or among the cancellations.
    forget_opening_of(conn, item)?;
    Ok(standing)
}

/// [`StepMemory::advance_upload_session`], inside the caller's transaction.
pub(crate) fn advance_upload_session(conn: &Connection, item: LocalItemId, address: &UploadSessionAddress, next_offset: u64, expires_at: Option<i64>) -> Result<UploadSessionStanding, TreeError> {
    if let Some((holder, _)) = list_memories(conn, "session_address = ?1", &[address.as_str().to_owned().into()])?.into_iter().find(|(holder, _)| *holder != item) {
        tracing::warn!("the upload session is item {holder}'s, not item {item}'s: it is not advanced, and not cancelled");
        return Ok(UploadSessionStanding::OfAnotherItem);
    }
    let advanced = change_memory(conn, item, |_, memory| match memory.upload_session.as_mut().filter(|session| &session.address == address) {
        Some(session) => {
            (session.next_offset, session.expires_at) = (next_offset, expires_at);
            Ok(UploadSessionStanding::Continued)
        }
        None => Ok(UploadSessionStanding::Abandoned),
    })?;
    let standing = advanced.unwrap_or(UploadSessionStanding::Abandoned);
    if standing == UploadSessionStanding::Abandoned {
        // Kept with its place when the item's memory had it until now; alone otherwise.
        abandon_address(conn, address, None)?;
    }
    Ok(standing)
}

/// [`StepMemory::abandon_upload_session`], inside the caller's transaction.
pub(crate) fn abandon_upload_session(conn: &Connection, item: LocalItemId) -> Result<(), TreeError> {
    change_memory(conn, item, |_, memory| memory.upload_session.take().map_or(Ok(()), |session| abandon(conn, &session)))?;
    Ok(())
}

/// The upload session of `item` that `has_ended` says ended is taken out of what is kept of
/// the item, before anything looks at whether the row still says it: a session that ended
/// is not cancelled, whatever the file's content is by now.
fn take_ended_session(conn: &Connection, item: LocalItemId, has_ended: impl FnOnce(&UploadSession) -> bool) -> Result<(), TreeError> {
    let mut kept = read_memory(conn, item)?;
    if kept.upload_session.as_ref().is_some_and(has_ended) {
        kept.upload_session = None;
        write_memory(conn, item, &kept)?;
    }
    Ok(())
}

/// [`StepMemory::forget_ended_upload_session`], inside the caller's transaction.
pub(crate) fn forget_ended_upload_session(conn: &Connection, item: LocalItemId, address: &UploadSessionAddress) -> Result<(), TreeError> {
    take_ended_session(conn, item, |session| &session.address == address)?;
    take_away_what_is_gone(conn, item)
}

/// [`StepMemory::forget_upload_session_cancellation`], inside the caller's transaction. A
/// session at `address` that an item's memory still keeps though its row does not say it —
/// answered as abandoned by [`list_empty_file_sessions`] before any write queued its
/// cancel — goes with it.
pub(crate) fn forget_upload_session_cancellation(conn: &Connection, address: &UploadSessionAddress) -> Result<(), TreeError> {
    forget_cancellation(conn, address)?;
    for (item, _) in list_memories(conn, "session_address = ?1", &[address.as_str().to_owned().into()])? {
        take_away_what_is_gone(conn, item)?;
    }
    forget_cancellation(conn, address)
}

/// [`StepMemory::remember_mismatched_item`], inside the caller's transaction.
pub(crate) fn remember_mismatched_item(conn: &Connection, item: LocalItemId, upload: &MismatchedItem) -> Result<MismatchedItemStanding, TreeError> {
    let is_of_folder = is_item_of_folder(conn, &upload.item)?;
    write_fact(conn, item, |_, memory| {
        if is_of_folder {
            return Ok(MismatchedItemStanding::OfFolder);
        }
        // The item an earlier upload of the file left is in OneDrive still: it is left to
        // remove by itself, as it is when the file's row goes.
        if let Some(earlier) = memory.mismatched_item.replace(upload.clone()).filter(|earlier| earlier.item != upload.item) {
            if !is_item_of_folder(conn, &earlier.item)? {
                leave_removal(conn, &earlier)?;
            }
        }
        Ok(MismatchedItemStanding::LeftToRemove)
    })
}

/// [`StepMemory::find_mismatched_item`], inside the caller's transaction.
pub(crate) fn find_mismatched_item(conn: &Connection, onedrive_item: &OneDriveItemReference) -> Result<Option<MismatchedItemUpload>, TreeError> {
    if is_item_of_folder(conn, onedrive_item)? {
        return Ok(None);
    }
    if let Some(file) = list_files_of(conn, onedrive_item)?.into_iter().next() {
        return Ok(read_memory(conn, file)?.mismatched_item.map(|mismatched_item| MismatchedItemUpload { mismatched_item, file: Some(file) }));
    }
    Ok(find_removal(conn, onedrive_item)?.map(|mismatched_item| MismatchedItemUpload { mismatched_item, file: None }))
}

/// [`StepMemory::forget_mismatched_item`], inside the caller's transaction.
pub(crate) fn forget_mismatched_item(conn: &Connection, onedrive_item: &OneDriveItemReference) -> Result<(), TreeError> {
    for file in list_files_of(conn, onedrive_item)? {
        change_memory(conn, file, |_, memory| {
            memory.mismatched_item = None;
            Ok(())
        })?;
    }
    forget_removal(conn, onedrive_item)
}

/// [`StepMemory::remember_temporary_name`], inside the caller's transaction.
pub(crate) fn remember_temporary_name(conn: &Connection, item: LocalItemId, temporary_name: &TemporaryName) -> Result<(), TreeError> {
    check_name(&temporary_name.name)?;
    write_fact(conn, item, |_, memory| {
        memory.temporary_name = Some(temporary_name.clone());
        Ok(())
    })
}

/// [`StepMemory::remember_failed_try`], inside the caller's transaction.
pub(crate) fn remember_failed_try(conn: &Connection, item: LocalItemId, unsent_since: Option<u64>, reason: &Reason, next_try_at: Option<i64>) -> Result<FailedTryStanding, TreeError> {
    write_fact(conn, item, |row, memory| {
        // `memory` is what the row still says: tries kept in it are of this unsent change.
        let count = memory.failed_tries.as_ref().map_or(0, |tries| tries.count).saturating_add(1);
        let tries = FailedTries { count, next_try_at, last_failure: reason.clone() };
        Ok(match learn_of_unsent_change(row, memory, unsent_since, |memory| memory.failed_tries = Some(tries)) {
            FactStanding::StillSaid => FailedTryStanding::Counted(count),
            FactStanding::VersionGone => FailedTryStanding::ChangeGone,
        })
    })
}

/// [`StepMemory::clear_next_tries`], inside the caller's transaction.
pub(crate) fn clear_next_tries(conn: &Connection) -> Result<usize, TreeError> {
    Ok(conn.prepare_cached("UPDATE step_memory SET next_try_at = NULL WHERE next_try_at IS NOT NULL")?.execute([])?)
}

/// [`StepMemory::remember_space_refusal`], inside the caller's transaction.
pub(crate) fn remember_space_refusal(conn: &Connection, item: LocalItemId, unsent_since: Option<u64>, space_refusal: &SpaceRefusal) -> Result<FactStanding, TreeError> {
    write_fact(conn, item, |row, memory| Ok(learn_of_unsent_change(row, memory, unsent_since, |memory| memory.space_refusal = Some(space_refusal.clone()))))
}

/// [`StepMemory::forget_fitting_space_refusals`], inside the caller's transaction.
pub(crate) fn forget_fitting_space_refusals(conn: &Connection, free_space: u64) -> Result<usize, TreeError> {
    let mut forgotten = 0;
    for (item, _) in list_memories(conn, "space_refusal_size IS NOT NULL", &[])? {
        change_memory(conn, item, |_, memory| {
            match memory.space_refusal.as_mut().map(|space_refusal| (space_refusal.decide_fit(free_space), space_refusal)) {
                Some((SpaceFit::StillTooBig, space_refusal)) => space_refusal.free_space = free_space,
                Some((SpaceFit::Fits, _)) => {
                    memory.space_refusal = None;
                    forgotten += 1;
                }
                None => {}
            }
            Ok(())
        })?;
    }
    Ok(forgotten)
}

/// [`StepMemory::remember_removals_confirmed`], inside the caller's transaction.
pub(crate) fn remember_removals_confirmed(conn: &Connection, removals: &[UnsentRemoval]) -> Result<Vec<UnsentRemoval>, TreeError> {
    let mut not_written = Vec::new();
    for removal in removals {
        let standing = change_memory(conn, removal.item, |row, memory| {
            let confirmation = RemovalConfirmation { unsent_since: removal.unsent_since };
            Ok(learn_of_unsent_change(row, memory, Some(removal.unsent_since), |memory| memory.removal_confirmation = Some(confirmation)))
        })?;
        if standing != Some(FactStanding::StillSaid) {
            not_written.push(*removal);
        }
    }
    Ok(not_written)
}

/// [`StepMemory::remember_refusal`], inside the caller's transaction.
pub(crate) fn remember_refusal(conn: &Connection, item: LocalItemId, unsent_since: Option<u64>, refusal: &Refusal) -> Result<FactStanding, TreeError> {
    if item == LocalItemId::ROOT {
        return Err(refuse("nothing of the root of the folder is sent, and nothing of it is refused".to_owned()));
    }
    write_fact(conn, item, |row, memory| Ok(learn_of_unsent_change(row, memory, unsent_since, |memory| memory.refusal = Some(refusal.clone()))))
}

/// [`StepMemory::forget_refusals_for`], inside the caller's transaction.
pub(crate) fn forget_refusals_for(conn: &Connection, reason: &Reason) -> Result<usize, TreeError> {
    let mut forgotten = 0;
    for (item, kept) in list_memories(conn, "refusal_reason IS NOT NULL", &[])? {
        if kept.refusal.is_some_and(|refusal| refusal.reason.key() == reason.key()) {
            // By the stored reason, also where the row does not say the refusal any more.
            change_memory(conn, item, |_, memory| {
                memory.refusal = None;
                Ok(())
            })?;
            forgotten += 1;
        }
    }
    Ok(forgotten)
}

/// [`StepMemory::remember_taken_name`], inside the caller's transaction.
pub(crate) fn remember_taken_name(conn: &Connection, item: LocalItemId, unsent_since: Option<u64>, taken_name: &TakenName) -> Result<FactStanding, TreeError> {
    check_name(&taken_name.holder_name)?;
    write_fact(conn, item, |row, memory| Ok(learn_of_unsent_change(row, memory, unsent_since, |memory| memory.taken_name = Some(taken_name.clone()))))
}

/// [`StepMemory::forget_fact`], inside the caller's transaction.
pub(crate) fn forget_fact(conn: &Connection, item: LocalItemId, fact: StepFact) -> Result<(), TreeError> {
    change_memory(conn, item, |_, memory| {
        match fact {
            StepFact::TemporaryName => memory.temporary_name = None,
            StepFact::FailedTries => memory.failed_tries = None,
            StepFact::SpaceRefusal => memory.space_refusal = None,
            StepFact::RemovalConfirmation => memory.removal_confirmation = None,
            StepFact::Refusal => memory.refusal = None,
            StepFact::TakenName => memory.taken_name = None,
        }
        Ok(())
    })?;
    Ok(())
}

/// A name a step made up or OneDrive gave is kept only when it is not the empty text: an
/// empty one would read back as no name at all.
fn check_name(name: &OsStr) -> Result<(), TreeError> {
    if name.is_empty() {
        return Err(refuse("the empty text is no name to remember".to_owned()));
    }
    Ok(())
}

/// What is kept of `item` is changed by `change`, whatever its row says: for what is
/// decided by something other than the row's versions. What `change` gives back.
fn change_kept_memory<T>(conn: &Connection, item: LocalItemId, change: impl FnOnce(&mut ItemMemory) -> T) -> Result<T, TreeError> {
    let kept = read_memory(conn, item)?;
    let mut memory = kept.clone();
    let done = change(&mut memory);
    if memory != kept {
        write_memory(conn, item, &memory)?;
    }
    Ok(done)
}

/// OneDrive confirmed `request` of `item`, an item OneDrive has: called by the commit of
/// the step in its transaction, once nothing can make it answer that the rows changed. What
/// the request ends where the stamp does not say it is forgotten
/// ([`ItemMemory::forget_confirmed_request`]); the upload session the commit was told ended
/// is forgotten as one that ended, with no cancel; and then what the row does not say any
/// more is taken away, as by every write for the item.
pub(crate) fn forget_confirmed_request(conn: &Connection, item: LocalItemId, request: ConfirmedRequest<'_>) -> Result<(), TreeError> {
    change_kept_memory(conn, item, |memory| memory.forget_confirmed_request(request))?;
    match request {
        ConfirmedRequest::Upload { ended_session: Some(address) } => forget_ended_upload_session(conn, item, address),
        _ => take_away_what_is_gone(conn, item),
    }
}

/// The bytes of the file `item` that `from` told are told by `to` from now on: called by
/// the commit of the step that computed their hash, in its transaction. The facts that hold
/// that content go on being said ([`ItemMemory::carry_content`]).
pub(crate) fn carry_content(conn: &Connection, item: LocalItemId, from: &ContentVersion, to: &ContentVersion) -> Result<(), TreeError> {
    change_kept_memory(conn, item, |memory| memory.carry_content(from, to))
}

/// An operation is done on the binding of `item`: every fact of an unsent change is dropped
/// from it, its upload session to the cancellations ([`ItemMemory::drop_facts_of_binding`]).
/// The temporary name it had is returned, and is not the item's any more: the caller says
/// which row it is with from now on.
fn drop_facts_of_item_binding(conn: &Connection, item: LocalItemId) -> Result<Option<TemporaryName>, TreeError> {
    let (session, temporary_name) = change_kept_memory(conn, item, ItemMemory::drop_facts_of_binding)?;
    if let Some(session) = session {
        abandon(conn, &session)?;
    }
    Ok(temporary_name)
}

/// `item`, which had no binding, is given one: inside the transaction of the operation that
/// gives it. What was learnt of the item as one OneDrive did not have is dropped. The
/// temporary name stays: remembered before the request that made the item under it, it
/// says where OneDrive has the item that is this row's from now on, until the commit of
/// that request, a later one or a listing says otherwise.
pub(crate) fn drop_facts_of_binding(conn: &Connection, item: LocalItemId) -> Result<(), TreeError> {
    let temporary_name = drop_facts_of_item_binding(conn, item)?;
    change_kept_memory(conn, item, |memory| memory.temporary_name = temporary_name)
}

/// The binding of `from` is `to`'s from now on: what is remembered follows, inside the
/// transaction of the operation that moves the binding, before the row of `from` goes if it
/// does.
///
/// - An operation was done on the binding of both items: every fact of an unsent change is
///   dropped from both, an upload session to the cancellations
///   ([`ItemMemory::drop_facts_of_binding`]). The temporary name `from` had is `to`'s: it
///   says under which name OneDrive has the item of that binding. When `from` had none,
///   `to` keeps its own where the row of `from` says that OneDrive has the item under
///   that very name: a new item made in OneDrive under a temporary name, whose answer was
///   lost, is bound here to the row a listing made of it. The temporary name of `to` is
///   dropped where OneDrive has the item of `from` under another name: it was the name of
///   another item.
/// - What OneDrive answered about sending an item into the directory `from` is dropped from
///   that item, the upload session of a new file in it to the cancellations
///   ([`ItemMemory::drop_answers_of_directory`]).
/// - What is of the directory of OneDrive is with `to`: the openings and the empty files of
///   abandoned sessions in it.
///
/// The item an upload left stays with the file whose upload left it only while that file
/// can still remove it: the schema takes it to the removals when the file's row goes,
/// comes to be left here or gets a binding, and it is void once the folder has it
/// ([`mismatched_item_removals`]).
pub(crate) fn follow_binding(conn: &Connection, from: LocalItemId, to: LocalItemId) -> Result<(), TreeError> {
    let temporary_name = drop_facts_of_item_binding(conn, from)?;
    let onedrive_place = find_item(conn, from)?.and_then(|row| row.onedrive).and_then(|onedrive| onedrive.place);
    let own_temporary_name = drop_facts_of_item_binding(conn, to)?.filter(|own| onedrive_place.as_ref().is_some_and(|place| own.is_at(place)));
    let temporary_name = temporary_name.or(own_temporary_name);
    if temporary_name.is_some() {
        change_kept_memory(conn, to, |memory| memory.temporary_name = temporary_name)?;
    }
    let in_directory: [Value; 1] = [from.as_i64().into()];
    for condition in ["session_empty_file_parent_id = ?1", "taken_parent_id = ?1", "refused_parent_id = ?1"] {
        for (item, _) in list_memories(conn, condition, &in_directory)? {
            if let Some(session) = change_kept_memory(conn, item, |memory| memory.drop_answers_of_directory(from))? {
                abandon(conn, &session)?;
            }
        }
    }
    move_openings(conn, from, to)?;
    move_empty_files(conn, from, to)
}

#[cfg(test)]
mod tests;
