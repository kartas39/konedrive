//! What a step remembers beyond the versions: what it keeps of an item between tries and
//! across a stop of the daemon, by local item id, apart from the rows of `items`. It is no
//! source of truth for what an item is: the row is, and a fact is answered only while the
//! row still says what the fact was learnt for ([`item_memory`]).
//!
//! **Four tables**, and why more than one. `step_memory` has one row per item, every fact
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
//!
//! **What holds of every fact**, and who holds it:
//!
//! - *A fact is answered for what it was learnt for, and for nothing else.* A reader
//!   answers what the row still says ([`item_memory`]). Every write for an item first takes
//!   away what its row does not say any more ([`change_memory`]); the first call of a new
//!   try of an upload ([`remember_upload_session_opening`]) is such a write. A fact that
//!   the row does not say when it is given is not written, which the writer answers
//!   ([`FactStanding`]).
//! - *An address always has somewhere to go.* An upload session is the item's or its address
//!   is among the cancellations, never neither: a session taken away because the row does
//!   not say it, one given for an item whose row went or whose bytes changed, one replaced
//!   by another, one whose item's binding moved. The one exception is said by the caller:
//!   a session that ended ([`forget_ended_upload_session`], [`forget_confirmed_request`]).
//!   The reader of the sessions at a place answers a session as the item's only where the
//!   item's memory answers it too.
//! - *A fact does not outlive its item.* The schema's trigger `step_memory_item_gone`, run
//!   when a row of `items` is deleted, whoever deletes it ([`schema`]).
//! - *What OneDrive answered a request is of the binding the request was made for*
//!   ([`follow_binding`], called by the operations on bindings in their transaction). When
//!   an item's binding moves, its upload session, its temporary name, a refusal, its failed
//!   tries and the confirmation of a removal are dropped from both items, the session to
//!   the cancellations. When a directory gives its binding up, what OneDrive answered about
//!   sending items into it — a refusal, a taken name, the session of a new file — is
//!   dropped from those items, and the openings and empty files in it are with the row
//!   that has the binding. The item an upload left and a refusal for lack of space stay
//!   with the item whose content they are about.
//! - *What a confirmed request leaves nothing of is forgotten with its commit*
//!   ([`forget_confirmed_request`], [`carry_content`], called by the commits of the steps
//!   in their transaction).
//! - *A crash loses nothing that costs the user.* Every operation is one transaction; a
//!   step writes an opening before the request that opens a session, the session before its
//!   first byte, and a temporary name before the request that takes it.

use std::ffi::OsStr;

use konedrive_reason::Reason;
use rusqlite::types::Value;
use rusqlite::{Connection, Transaction};

use super::item_row::find_item;
use super::refuse;
use crate::model::{ContentVersion, Item, ItemPlace, LocalItemId, OneDriveItemId};
use crate::{TreeError, TreeStore};

/// What is remembered of one item, as values, and whether its row still says it.
mod item_memory;
/// A row of `step_memory`.
mod memory_row;
/// The items uploads left with other content whose file's row went, until each is removed.
mod mismatched_item_removals;
/// The items OneDrive refused, with their reasons, listed and counted.
mod refusal_reasons;
/// The cancels of upload sessions that are left to send.
mod upload_session_cancellations;
/// The upload sessions the folder was about to open, by place and time.
mod upload_session_openings;

pub(crate) use item_memory::ConfirmedRequest;
pub use item_memory::{
    is_content_of, FactStanding, FailedTries, ItemMemory, ItemMemoryParts, MismatchedItem, Refusal, RefusedVersion, RemovalConfirmation, SpaceFit, SpaceRefusal, StepFact, TakenName,
    TemporaryName, UnsentRemoval, UploadSession, UploadSessionAddress, UploadSessionStanding, UploadTag,
};
pub use refusal_reasons::ItemRefusalReason;
pub use upload_session_openings::{OpeningTry, OpeningWindow, UPLOAD_SESSION_OPENING_KEPT};

pub(crate) use mismatched_item_removals::{forget_removal as forget_mismatched_item_removal, list_removals as list_mismatched_item_removals};
pub(crate) use refusal_reasons::{count_refusal_reasons, count_space_refusals, list_refusal_reasons, list_space_refusal_reasons};
pub(crate) use upload_session_cancellations::list_cancellations as list_upload_session_cancellations;
pub(crate) use upload_session_openings::{forget_openings_at, forget_openings_before, list_opening_windows, settle_answered_opening};

use memory_row::{folded_name_value, list_memories, read_memory, write_memory};
use upload_session_cancellations::{abandon, abandon_address, forget_cancellation, list_cancellations_at, move_empty_files};
use upload_session_openings::{forget_opening_of, move_openings, remember_opening};

/// What a store is created with for what the steps remember: the four tables, each as its
/// module has it, and the trigger `step_memory_item_gone`, which holds "a fact does not
/// outlive its item" whoever deletes a row of `items`. Each table says what the trigger
/// does to it; `step_memory` comes last, after the tables that keep something of its row
/// past the item took it.
pub(crate) fn schema() -> String {
    format!(
        "{}{}{}{}
         CREATE TRIGGER step_memory_item_gone AFTER DELETE ON items BEGIN{}{}{}{}
         END;",
        memory_row::schema(),
        upload_session_openings::schema(),
        upload_session_cancellations::schema(),
        mismatched_item_removals::schema(),
        upload_session_cancellations::WHEN_ITEM_GOES,
        mismatched_item_removals::WHEN_ITEM_GOES,
        upload_session_openings::WHEN_ITEM_GOES,
        memory_row::WHEN_ITEM_GOES,
    )
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
    Continued { item: LocalItemId, address: UploadSessionAddress },
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

    /// The items that wait until a time, soonest first, at most `limit` of them. Found by an
    /// index.
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
    /// file's row went before they were removed, the one left first first: to be removed
    /// while they are still what the upload left
    /// ([`MismatchedItem::is_unchanged_since_upload`]).
    pub fn list_mismatched_item_removals(&self, limit: usize) -> Result<Vec<MismatchedItem>, TreeError> {
        list_mismatched_item_removals(&self.store.conn, limit)
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

    /// OneDrive opened `session` for `item`: given before its first byte is sent. After the
    /// call the address is the item's or among the cancellations. *Continued*: the session
    /// is the item's from now on, and a session of another address it had is abandoned.
    /// *Abandoned*: the item's row went, or the bytes the session sends are not the file's
    /// any more; nothing is kept of it with the item, its cancel is left to send, and the
    /// step sends nothing to it. The opening kept for the item goes either way: the address
    /// tells the empty file from now on.
    pub fn remember_upload_session(&mut self, item: LocalItemId, session: &UploadSession) -> Result<UploadSessionStanding, TreeError> {
        self.write(|tx| remember_upload_session(tx, item, session))
    }

    /// The session of `item` at `address` got as far as `next_offset`, and ends by itself at
    /// `expires_at`. *Abandoned* when the item's row does not say that session any more, or
    /// the item has none at that address, or no row: nothing is written, the cancel of the
    /// session at `address` is left to send, and the step sends nothing more to it.
    pub fn advance_upload_session(&mut self, item: LocalItemId, address: &UploadSessionAddress, next_offset: u64, expires_at: Option<i64>) -> Result<UploadSessionStanding, TreeError> {
        self.write(|tx| advance_upload_session(tx, item, address, next_offset, expires_at))
    }

    /// The session of `item` is given up before it ended: its cancel is left to send. For a
    /// session that ended, [`forget_ended_upload_session`](Self::forget_ended_upload_session).
    pub fn abandon_upload_session(&mut self, item: LocalItemId) -> Result<(), TreeError> {
        self.write(|tx| abandon_upload_session(tx, item))
    }

    /// The session at `address` ended — OneDrive answered its last fragment, or answered
    /// that it does not have the session — and is forgotten: of `item` when it is the
    /// item's, and among the cancellations. Nothing is left to cancel. Another session the
    /// item has stays. For a session given up, [`abandon_upload_session`](Self::abandon_upload_session).
    pub fn forget_ended_upload_session(&mut self, item: LocalItemId, address: &UploadSessionAddress) -> Result<(), TreeError> {
        self.write(|tx| forget_ended_upload_session(tx, item, address))
    }

    /// The session at `address` was cancelled, or found gone: its cancel is not left to send
    /// any more.
    pub fn forget_upload_session_cancellation(&mut self, address: &UploadSessionAddress) -> Result<(), TreeError> {
        self.write(|tx| forget_upload_session_cancellation(tx, address))
    }

    /// An upload of `item` left `upload` in OneDrive with other content than was sent: kept
    /// before its removal is asked. Refused for an id with no row.
    pub fn remember_mismatched_item(&mut self, item: LocalItemId, upload: &MismatchedItem) -> Result<(), TreeError> {
        self.write(|tx| remember_mismatched_item(tx, item, upload))
    }

    /// The item `item_id` of OneDrive, which an upload left and whose file's row went, was
    /// removed, found gone, or found changed by another: it is not left to remove any more.
    pub fn forget_mismatched_item_removal(&mut self, item_id: &OneDriveItemId) -> Result<(), TreeError> {
        self.write(|tx| forget_mismatched_item_removal(tx, item_id).map(|_| ()))
    }

    /// `item` goes through `temporary_name` in OneDrive: kept before the request that takes
    /// the name, and answered until its step forgets it or the item's binding moves.
    /// Refused for an id with no row.
    pub fn remember_temporary_name(&mut self, item: LocalItemId, temporary_name: &TemporaryName) -> Result<(), TreeError> {
        self.write(|tx| remember_temporary_name(tx, item, temporary_name))
    }

    /// A try of the step of `item` failed for `reason`; the next is due at `next_try_at`, or
    /// at once. The count of tries, with this one. Refused for an id with no row.
    pub fn remember_failed_try(&mut self, item: LocalItemId, reason: &Reason, next_try_at: Option<i64>) -> Result<u32, TreeError> {
        self.write(|tx| remember_failed_try(tx, item, reason, next_try_at))
    }

    /// Every item that waits until a time is due now, with its count and its reason kept:
    /// the user asked for a refresh, or the network is back. How many.
    pub fn clear_next_tries(&mut self) -> Result<usize, TreeError> {
        self.write(|tx| clear_next_tries(tx))
    }

    /// OneDrive refused the content of `item` for lack of space. Not written when the bytes
    /// that were refused are not the file's any more. Refused for an id with no row.
    pub fn remember_space_refusal(&mut self, item: LocalItemId, space_refusal: &SpaceRefusal) -> Result<FactStanding, TreeError> {
        self.write(|tx| remember_space_refusal(tx, item, space_refusal))
    }

    /// OneDrive has `free_space` bytes free, as a quota read said: the refusal for lack of
    /// space of every file that fits now is forgotten, and the rest are too big for this
    /// much. How many were forgotten.
    pub fn forget_fitting_space_refusals(&mut self, free_space: u64) -> Result<usize, TreeError> {
        self.write(|tx| forget_fitting_space_refusals(tx, free_space))
    }

    /// The user confirmed `removals`, which a mass removal held: written of each whose row
    /// is still that removal, with the stamp it was held with, and of no other. How many
    /// were written.
    pub fn remember_removals_confirmed(&mut self, removals: &[UnsentRemoval]) -> Result<usize, TreeError> {
        self.write(|tx| remember_removals_confirmed(tx, removals))
    }

    /// OneDrive refuses `item` for good. Not written when the item's row does not say what
    /// was refused any more: its version at that place, or that removal. Refused for an id
    /// with no row, and for the root.
    pub fn remember_refusal(&mut self, item: LocalItemId, refusal: &Refusal) -> Result<FactStanding, TreeError> {
        self.write(|tx| remember_refusal(tx, item, refusal))
    }

    /// Every refusal for good whose reason is `reason`, whatever its detail, is forgotten:
    /// the account signed in again, and what was not allowed may be now. How many.
    pub fn forget_refusals_for(&mut self, reason: &Reason) -> Result<usize, TreeError> {
        self.write(|tx| forget_refusals_for(tx, reason))
    }

    /// OneDrive answered that the name `item` was sent under is taken. Not written when the
    /// item's local place is not the one that was sent any more. Refused for an id with no
    /// row.
    pub fn remember_taken_name(&mut self, item: LocalItemId, taken_name: &TakenName) -> Result<FactStanding, TreeError> {
        self.write(|tx| remember_taken_name(tx, item, taken_name))
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
    let mut statement = conn.prepare_cached("SELECT local_item_id, next_try_at FROM step_memory WHERE next_try_at IS NOT NULL ORDER BY next_try_at, local_item_id LIMIT ?1")?;
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
    let confirmed = list_said_facts(conn, "removal_confirmed_unsent_since IS NOT NULL", after, limit, |said| said.removal_confirmation)?;
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
        let Some(session) = memory.upload_session else { continue };
        let is_said = find_item(conn, item)?.is_some_and(|row| session.decide_standing(&row) == FactStanding::StillSaid);
        sessions.push(if is_said { EmptyFileSession::Continued { item, address: session.address } } else { EmptyFileSession::Abandoned { address: session.address } });
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
pub(crate) fn remember_upload_session(conn: &Connection, item: LocalItemId, session: &UploadSession) -> Result<UploadSessionStanding, TreeError> {
    // An empty file is at a place only while the directory of that place has a row.
    let has_directory = |place: &ItemPlace| Ok::<bool, TreeError>(find_item(conn, place.parent_local_item_id)?.is_some());
    let mut session = session.clone();
    if !session.empty_file_place.as_ref().map(has_directory).transpose()?.unwrap_or(true) {
        session.empty_file_place = None;
    }
    let kept = change_memory(conn, item, |row, memory| {
        if session.decide_standing(row) == FactStanding::VersionGone {
            return Ok(UploadSessionStanding::Abandoned);
        }
        if let Some(earlier) = memory.upload_session.replace(session.clone()).filter(|earlier| earlier.address != session.address) {
            abandon(conn, &earlier)?;
        }
        Ok(UploadSessionStanding::Continued)
    })?;
    // No row, or other bytes: the address is nobody's, and is kept to be cancelled.
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
    forget_cancellation(conn, address)?;
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
pub(crate) fn remember_mismatched_item(conn: &Connection, item: LocalItemId, upload: &MismatchedItem) -> Result<(), TreeError> {
    write_fact(conn, item, |_, memory| {
        memory.mismatched_item = Some(upload.clone());
        Ok(())
    })
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
pub(crate) fn remember_failed_try(conn: &Connection, item: LocalItemId, reason: &Reason, next_try_at: Option<i64>) -> Result<u32, TreeError> {
    write_fact(conn, item, |_, memory| {
        let count = memory.failed_tries.as_ref().map_or(0, |tries| tries.count).saturating_add(1);
        memory.failed_tries = Some(FailedTries { count, next_try_at, last_failure: reason.clone() });
        Ok(count)
    })
}

/// [`StepMemory::clear_next_tries`], inside the caller's transaction.
pub(crate) fn clear_next_tries(conn: &Connection) -> Result<usize, TreeError> {
    Ok(conn.prepare_cached("UPDATE step_memory SET next_try_at = NULL WHERE next_try_at IS NOT NULL")?.execute([])?)
}

/// [`StepMemory::remember_space_refusal`], inside the caller's transaction.
pub(crate) fn remember_space_refusal(conn: &Connection, item: LocalItemId, space_refusal: &SpaceRefusal) -> Result<FactStanding, TreeError> {
    write_fact(conn, item, |row, memory| {
        let fact_standing = space_refusal.decide_standing(row);
        if fact_standing == FactStanding::StillSaid {
            memory.space_refusal = Some(space_refusal.clone());
        }
        Ok(fact_standing)
    })
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
pub(crate) fn remember_removals_confirmed(conn: &Connection, removals: &[UnsentRemoval]) -> Result<usize, TreeError> {
    let mut written = 0;
    for removal in removals {
        change_memory(conn, removal.item, |row, memory| {
            let confirmation = RemovalConfirmation { unsent_since: removal.unsent_since };
            if confirmation.decide_standing(row) == FactStanding::StillSaid {
                memory.removal_confirmation = Some(confirmation);
                written += 1;
            }
            Ok(())
        })?;
    }
    Ok(written)
}

/// [`StepMemory::remember_refusal`], inside the caller's transaction.
pub(crate) fn remember_refusal(conn: &Connection, item: LocalItemId, refusal: &Refusal) -> Result<FactStanding, TreeError> {
    if item == LocalItemId::ROOT {
        return Err(refuse("nothing of the root of the folder is sent, and nothing of it is refused".to_owned()));
    }
    write_fact(conn, item, |row, memory| {
        let fact_standing = refusal.decide_standing(row);
        if fact_standing == FactStanding::StillSaid {
            memory.refusal = Some(refusal.clone());
        }
        Ok(fact_standing)
    })
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
pub(crate) fn remember_taken_name(conn: &Connection, item: LocalItemId, taken_name: &TakenName) -> Result<FactStanding, TreeError> {
    check_name(&taken_name.holder_name)?;
    write_fact(conn, item, |row, memory| {
        let fact_standing = taken_name.decide_standing(row);
        if fact_standing == FactStanding::StillSaid {
            memory.taken_name = Some(taken_name.clone());
        }
        Ok(fact_standing)
    })
}

/// [`StepMemory::forget_fact`], inside the caller's transaction.
pub(crate) fn forget_fact(conn: &Connection, item: LocalItemId, fact: StepFact) -> Result<(), TreeError> {
    change_memory(conn, item, |_, memory| {
        match fact {
            StepFact::MismatchedItem => memory.mismatched_item = None,
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

/// What is kept of `item` is changed by `change`, whatever its row says, and an upload
/// session `change` gives back is abandoned: for what is decided by something other than
/// the row's versions.
fn change_kept_memory(conn: &Connection, item: LocalItemId, change: impl FnOnce(&mut ItemMemory) -> Option<UploadSession>) -> Result<(), TreeError> {
    let kept = read_memory(conn, item)?;
    let mut memory = kept.clone();
    if let Some(session) = change(&mut memory) {
        abandon(conn, &session)?;
    }
    if memory != kept {
        write_memory(conn, item, &memory)?;
    }
    Ok(())
}

/// OneDrive confirmed `request` of `item`: called by the commit of the step in its
/// transaction, after it wrote the rows. What was kept for the request is forgotten
/// ([`ItemMemory::forget_confirmed_request`]) — the upload session that sent the content
/// ended, and is not cancelled —, and then what the row does not say any more is taken
/// away, as by every write for the item.
pub(crate) fn forget_confirmed_request(conn: &Connection, item: LocalItemId, request: ConfirmedRequest<'_>) -> Result<(), TreeError> {
    change_kept_memory(conn, item, |memory| {
        memory.forget_confirmed_request(request);
        None
    })?;
    take_away_what_is_gone(conn, item)
}

/// The bytes of the file `item` that `from` told are told by `to` from now on: called by
/// the commit of the step that computed their hash, in its transaction. The facts that hold
/// that content go on being said ([`ItemMemory::carry_content`]).
pub(crate) fn carry_content(conn: &Connection, item: LocalItemId, from: &ContentVersion, to: &ContentVersion) -> Result<(), TreeError> {
    change_kept_memory(conn, item, |memory| {
        memory.carry_content(from, to);
        None
    })
}

/// The binding of `from` is `to`'s from now on: what is remembered follows, inside the
/// transaction of the operation that moves the binding, before the row of `from` goes if it
/// does.
///
/// - What OneDrive answered the requests of either item as it was bound before is dropped
///   from both, an upload session to the cancellations
///   ([`ItemMemory::drop_requests_of_binding`]).
/// - What OneDrive answered about sending an item into the directory `from` is dropped from
///   that item, the upload session of a new file in it to the cancellations
///   ([`ItemMemory::drop_answers_of_directory`]).
/// - What is of the directory of OneDrive is with `to`: the openings and the empty files of
///   abandoned sessions in it.
pub(crate) fn follow_binding(conn: &Connection, from: LocalItemId, to: LocalItemId) -> Result<(), TreeError> {
    for item in [from, to] {
        change_kept_memory(conn, item, ItemMemory::drop_requests_of_binding)?;
    }
    let in_directory: [Value; 1] = [from.as_i64().into()];
    for condition in ["session_empty_file_parent_id = ?1", "taken_parent_id = ?1", "refused_parent_id = ?1"] {
        for (item, _) in list_memories(conn, condition, &in_directory)? {
            change_kept_memory(conn, item, |memory| memory.drop_answers_of_directory(from))?;
        }
    }
    move_openings(conn, from, to)?;
    move_empty_files(conn, from, to)
}

#[cfg(test)]
mod tests;
