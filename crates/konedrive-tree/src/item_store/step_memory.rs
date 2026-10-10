//! What a step remembers beyond the versions: what it keeps of an item between tries and
//! across a stop of the daemon, by local item id, apart from the rows of `items`. It is no
//! source of truth for what an item is: the row is, and a fact is answered only while the
//! row still says what the fact was learnt for ([`item_memory`]).
//!
//! **Three tables**, and why more than one. `step_memory` has one row per item, every fact
//! of the item in it. Two things a step keeps are not of the item they came from, and would
//! be lost with its row:
//!
//! - an upload session the folder was about to open is of the place its empty file takes in
//!   OneDrive (`upload_session_openings`, [`upload_session_openings`]): the file that asked
//!   may be removed or renamed while the empty file still holds the name, and the next file
//!   of that name must be able to tell it is the folder's own;
//! - the address of an upload session nothing goes on with is what is left to cancel
//!   (`abandoned_upload_sessions`, [`abandoned_upload_sessions`]): it is kept until it is
//!   cancelled, so that the session's empty file does not hold a name for days.
//!
//! **What holds of every fact**, and who holds it:
//!
//! - *A fact is of the versions it was learnt for.* A reader answers what the row still
//!   says and nothing else. Every write for an item first takes away what its row does not
//!   say any more, an upload session among it to the abandoned ones; and a fact that the
//!   row does not say when it is given is not written, which the writer answers
//!   ([`FactStanding`]).
//! - *A fact does not outlive its item.* The schema's trigger `step_memory_item_gone`, run
//!   when a row of `items` is deleted, whoever deletes it.
//! - *A binding that moves takes with it what is OneDrive's* ([`follow_binding`], called by
//!   the operations on bindings in their transaction): the confirmation of a removal, and
//!   the openings and empty files in a directory. What was learnt of the item's own
//!   requests to OneDrive as it was bound before — its upload session, its temporary name,
//!   a refusal, its failed tries — is dropped from both items, the session to the abandoned
//!   ones: continued, it would send into an item that is not this one's any more, or make
//!   a second one. The item an upload left with other content and a file too big for the
//!   space stay with the item whose content they are about.
//! - *A crash loses nothing that costs the user.* Every operation is one transaction; a
//!   step writes an opening before the request that opens a session, the session before its
//!   first byte, and a temporary name before the request that takes it.

use std::ffi::OsStr;

use konedrive_reason::Reason;
use rusqlite::types::Value;
use rusqlite::{Connection, Transaction};

use super::item_row::find_item;
use super::refuse;
use crate::model::{Item, ItemPlace, LocalItemId};
use crate::{TreeError, TreeStore};

/// The upload sessions nothing goes on with, until each is cancelled.
mod abandoned_upload_sessions;
/// What is remembered of one item, as values, and whether its row still says it.
mod item_memory;
/// A row of `step_memory`, and the schema of the three tables.
mod memory_row;
/// The upload sessions the folder was about to open, by place and time.
mod upload_session_openings;

pub use item_memory::{
    decide_removal_confirmation_standing, FactStanding, FailedTries, ItemMemory, MismatchedItem, Refusal, SpaceFit, StepFact, TakenName, TemporaryName, SpaceRefusal, UploadSession,
    UploadSessionAddress, UploadTag,
};
pub(crate) use memory_row::schema;
pub use upload_session_openings::{OpeningRecord, OpeningWindow, UPLOAD_SESSION_OPENING_KEPT};

use abandoned_upload_sessions::{abandon, forget_abandoned, list_abandoned, list_abandoned_at, move_placeholders};
use memory_row::{folded_name_value, list_memories, read_memory, write_memory, HAS_USER_REASON};
use upload_session_openings::{forget_opening_of, forget_openings_at, forget_openings_before, list_opening_windows, move_openings, remember_opening, settle_answered_opening};

/// An item that waits until a time: the next try of its step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedWait {
    pub item: LocalItemId,
    /// Unix seconds.
    pub until: i64,
}

/// An item that is not sent until the user does something, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserReason {
    pub item: LocalItemId,
    /// The reason of a refusal for good as OneDrive answered it, or `too-big` with what the
    /// file needs and the free space.
    pub reason: Reason,
}

/// An upload session whose empty file holds a place in OneDrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceholderSession {
    /// The session an item's step goes on with.
    OfItem { item: LocalItemId, address: UploadSessionAddress },
    /// A session nothing goes on with, to be cancelled.
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

    /// The items with a reason for the user — refused for good, or too big for the space
    /// OneDrive has — by local item id, after the item `after` when one is given, at most
    /// `limit` of them. Found by an index; an item whose row does not say the fact any more
    /// is not among them.
    pub fn list_user_reasons(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<UserReason>, TreeError> {
        list_user_reasons(&self.store.conn, after, limit)
    }

    /// The names OneDrive answered are taken in `directory`, each with the item that was
    /// sent there, by local item id: what a plan treats as one name with its holder's.
    pub fn list_taken_names(&self, directory: LocalItemId) -> Result<Vec<(LocalItemId, TakenName)>, TreeError> {
        list_taken_names(&self.store.conn, directory)
    }

    /// The upload sessions whose empty file holds `place` in OneDrive, as OneDrive compares
    /// names: those of items, whatever their rows say now, and the abandoned ones.
    pub fn list_placeholder_sessions(&self, place: &ItemPlace) -> Result<Vec<PlaceholderSession>, TreeError> {
        list_placeholder_sessions(&self.store.conn, place)
    }

    /// The times between which an empty file at `place` may be this folder's own: one
    /// window for each upload session the folder was about to open there whose address
    /// never came. Never between two windows.
    pub fn list_opening_windows(&self, place: &ItemPlace) -> Result<Vec<OpeningWindow>, TreeError> {
        list_opening_windows(&self.store.conn, place)
    }

    /// Up to `limit` upload sessions nothing goes on with, the one abandoned first first:
    /// to be cancelled.
    pub fn list_abandoned_upload_sessions(&self, limit: usize) -> Result<Vec<UploadSessionAddress>, TreeError> {
        list_abandoned(&self.store.conn, limit)
    }

    /// The step of `item` is about to open the upload session of a new file at `place`: kept
    /// before the request, at the time `now`. Refused for an id with no row.
    pub fn remember_upload_session_opening(&mut self, item: LocalItemId, place: &ItemPlace, now: i64) -> Result<OpeningRecord, TreeError> {
        self.write(|tx| remember_upload_session_opening(tx, item, place, now))
    }

    /// The request of `item` that `record` was answered for got a certain answer, and left
    /// no empty file.
    pub fn settle_answered_opening(&mut self, item: LocalItemId, record: OpeningRecord) -> Result<(), TreeError> {
        self.write(|tx| settle_answered_opening(tx, item, record))
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

    /// `item` has the upload session `session` from now on: kept before its first byte is
    /// sent. The opening kept for the item goes, and a session of another address the item
    /// had is abandoned. When the file's content is not the one the session sends any more,
    /// nothing is kept of it and the session is abandoned at once. Refused for an id with
    /// no row.
    pub fn remember_upload_session(&mut self, item: LocalItemId, session: &UploadSession) -> Result<FactStanding, TreeError> {
        self.write(|tx| remember_upload_session(tx, item, session))
    }

    /// The session of `item` at `address` got as far as `next_offset`, and ends by itself at
    /// `expires_at`. Not written when the item has no such session any more.
    pub fn advance_upload_session(&mut self, item: LocalItemId, address: &UploadSessionAddress, next_offset: u64, expires_at: Option<i64>) -> Result<FactStanding, TreeError> {
        self.write(|tx| advance_upload_session(tx, item, address, next_offset, expires_at))
    }

    /// The session of `item` is given up, and kept among the abandoned ones.
    pub fn abandon_upload_session(&mut self, item: LocalItemId) -> Result<(), TreeError> {
        self.write(|tx| abandon_upload_session(tx, item))
    }

    /// The abandoned session at `address` was cancelled, or found gone.
    pub fn forget_abandoned_upload_session(&mut self, address: &UploadSessionAddress) -> Result<(), TreeError> {
        self.write(|tx| forget_abandoned(tx, address))
    }

    /// An upload of `item` left `upload` in OneDrive with other content than was sent: kept
    /// before its removal is asked. Refused for an id with no row.
    pub fn remember_mismatched_item(&mut self, item: LocalItemId, upload: &MismatchedItem) -> Result<(), TreeError> {
        self.write(|tx| remember_mismatched_item(tx, item, upload))
    }

    /// `item` goes through `temporary_name` in OneDrive: kept before the request that takes
    /// the name. Not written when the item's local place is not the one the name leads to.
    /// Refused for an id with no row.
    pub fn remember_temporary_name(&mut self, item: LocalItemId, temporary_name: &TemporaryName) -> Result<FactStanding, TreeError> {
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

    /// OneDrive refused the content of `item` for lack of space. Not written when the file's
    /// content is not the refused one any more. Refused for an id with no row.
    pub fn remember_space_refusal(&mut self, item: LocalItemId, space_refusal: &SpaceRefusal) -> Result<FactStanding, TreeError> {
        self.write(|tx| remember_space_refusal(tx, item, space_refusal))
    }

    /// OneDrive has `free_space` bytes free, as a quota read said: every file kept as too big
    /// that fits now is let go, and the rest are too big for this much. How many were let
    /// go.
    pub fn write_free_space(&mut self, free_space: u64) -> Result<usize, TreeError> {
        self.write(|tx| write_free_space(tx, free_space))
    }

    /// The user confirmed the removal of `items`, which a mass removal held: of each whose
    /// row is a removal that is not sent. How many were written.
    pub fn remember_removals_confirmed(&mut self, items: &[LocalItemId]) -> Result<usize, TreeError> {
        self.write(|tx| remember_removals_confirmed(tx, items))
    }

    /// OneDrive refuses `item` for good. Not written when the item's local version is not
    /// the refused one any more. Refused for an id with no row, and for the root.
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

    /// `fact` of `item` is forgotten: its step is done with it. An upload session forgotten
    /// here has ended — completed, or found gone — and is not cancelled
    /// ([`abandon_upload_session`](Self::abandon_upload_session) is for one given up).
    /// Nothing for an id with no row.
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
    Ok(read_memory(conn, item)?.split_by_standing(&row).0)
}

fn list_timed_waits(conn: &Connection, limit: usize) -> Result<Vec<TimedWait>, TreeError> {
    let mut statement = conn.prepare_cached("SELECT local_item_id, next_try_at FROM step_memory WHERE next_try_at IS NOT NULL ORDER BY next_try_at, local_item_id LIMIT ?1")?;
    let waits = statement.query_map([limit as i64], |row| Ok(TimedWait { item: LocalItemId::new(row.get(0)?), until: row.get(1)? }))?;
    Ok(waits.collect::<Result<Vec<_>, _>>()?)
}

/// The reason for the user in what `row` still says of `memory`: a refusal for good before
/// a file too big for the space.
fn find_user_reason(memory: ItemMemory, row: &Item) -> Option<Reason> {
    let (said, _) = memory.split_by_standing(row);
    let space_refusal = said.space_refusal.map(|space_refusal| Reason::TooBig(Some((space_refusal.content.size(), space_refusal.free_space))));
    said.refusal.map(|refusal| refusal.reason).or(space_refusal)
}

fn list_user_reasons(conn: &Connection, after: Option<LocalItemId>, limit: usize) -> Result<Vec<UserReason>, TreeError> {
    let mut reasons = Vec::new();
    let mut after = after.map_or(0, LocalItemId::as_i64);
    // Portions of the index, until enough rows still say their fact or the index ends.
    while reasons.len() < limit {
        let portion = list_memories(conn, &format!("{HAS_USER_REASON} AND local_item_id > ?1 ORDER BY local_item_id LIMIT ?2"), &[after.into(), (limit as i64).into()])?;
        let Some((last, _)) = portion.last() else { break };
        after = last.as_i64();
        let is_last_portion = portion.len() < limit;
        for (item, memory) in portion {
            let reason = find_item(conn, item)?.and_then(|row| find_user_reason(memory, &row));
            if let Some(reason) = reason.filter(|_| reasons.len() < limit) {
                reasons.push(UserReason { item, reason });
            }
        }
        if is_last_portion {
            break;
        }
    }
    Ok(reasons)
}

fn list_taken_names(conn: &Connection, directory: LocalItemId) -> Result<Vec<(LocalItemId, TakenName)>, TreeError> {
    let mut names = Vec::new();
    for (item, memory) in list_memories(conn, "taken_parent_id = ?1 ORDER BY local_item_id", &[directory.as_i64().into()])? {
        let said = find_item(conn, item)?.map(|row| memory.split_by_standing(&row).0);
        names.extend(said.and_then(|said| said.taken_name).map(|taken_name| (item, taken_name)));
    }
    Ok(names)
}

fn list_placeholder_sessions(conn: &Connection, place: &ItemPlace) -> Result<Vec<PlaceholderSession>, TreeError> {
    let at_place = "session_placeholder_parent_id = ?1 AND session_placeholder_folded_name = ?2 ORDER BY local_item_id";
    let of_items = list_memories(conn, at_place, &[place.parent_local_item_id.as_i64().into(), folded_name_value(&place.name)])?;
    let mut sessions: Vec<PlaceholderSession> =
        of_items.into_iter().filter_map(|(item, memory)| memory.upload_session.map(|session| PlaceholderSession::OfItem { item, address: session.address })).collect();
    sessions.extend(list_abandoned_at(conn, place)?.into_iter().map(|address| PlaceholderSession::Abandoned { address }));
    Ok(sessions)
}

/// The one way a memory is changed: what is kept of `item` is read, what its row does not
/// say any more is taken away — an upload session to the abandoned ones —, `change` is
/// applied to the rest, and the result is written. `None`, with nothing kept, for an id with
/// no row.
fn change_memory<T>(conn: &Connection, item: LocalItemId, change: impl FnOnce(&Item, &mut ItemMemory) -> Result<T, TreeError>) -> Result<Option<T>, TreeError> {
    let kept = read_memory(conn, item)?;
    let Some(row) = find_item(conn, item)? else {
        write_memory(conn, item, &ItemMemory::default())?;
        return Ok(None);
    };
    let (mut said, gone) = kept.clone().split_by_standing(&row);
    if let Some(session) = &gone.upload_session {
        abandon(conn, session)?;
    }
    let done = change(&row, &mut said)?;
    if said != kept {
        write_memory(conn, item, &said)?;
    }
    Ok(Some(done))
}

/// [`change_memory`] for a fact that is written: refused for an id with no row.
fn write_fact<T>(conn: &Connection, item: LocalItemId, change: impl FnOnce(&Item, &mut ItemMemory) -> Result<T, TreeError>) -> Result<T, TreeError> {
    change_memory(conn, item, change)?.ok_or_else(|| refuse(format!("item {item} has no row: nothing is remembered of it")))
}

/// [`StepMemory::remember_upload_session_opening`], inside the caller's transaction.
pub(crate) fn remember_upload_session_opening(conn: &Connection, item: LocalItemId, place: &ItemPlace, now: i64) -> Result<OpeningRecord, TreeError> {
    for id in [item, place.parent_local_item_id] {
        if find_item(conn, id)?.is_none() {
            return Err(refuse(format!("item {id} has no row: no opening of an upload session is kept for it")));
        }
    }
    remember_opening(conn, item, place, now)
}

/// [`StepMemory::remember_upload_session`], inside the caller's transaction.
pub(crate) fn remember_upload_session(conn: &Connection, item: LocalItemId, session: &UploadSession) -> Result<FactStanding, TreeError> {
    let fact_standing = write_fact(conn, item, |row, memory| {
        if session.decide_standing(row) == FactStanding::VersionGone {
            abandon(conn, session)?;
            return Ok(FactStanding::VersionGone);
        }
        if let Some(earlier) = memory.upload_session.replace(session.clone()).filter(|earlier| earlier.address != session.address) {
            abandon(conn, &earlier)?;
        }
        Ok(FactStanding::StillSaid)
    })?;
    // The address is known and kept, with the item or among the abandoned.
    forget_opening_of(conn, item)?;
    Ok(fact_standing)
}

/// [`StepMemory::advance_upload_session`], inside the caller's transaction.
pub(crate) fn advance_upload_session(conn: &Connection, item: LocalItemId, address: &UploadSessionAddress, next_offset: u64, expires_at: Option<i64>) -> Result<FactStanding, TreeError> {
    let advanced = change_memory(conn, item, |_, memory| match memory.upload_session.as_mut().filter(|session| &session.address == address) {
        Some(session) => {
            (session.next_offset, session.expires_at) = (next_offset, expires_at);
            Ok(FactStanding::StillSaid)
        }
        None => Ok(FactStanding::VersionGone),
    })?;
    Ok(advanced.unwrap_or(FactStanding::VersionGone))
}

/// [`StepMemory::abandon_upload_session`], inside the caller's transaction.
pub(crate) fn abandon_upload_session(conn: &Connection, item: LocalItemId) -> Result<(), TreeError> {
    change_memory(conn, item, |_, memory| memory.upload_session.take().map_or(Ok(()), |session| abandon(conn, &session)))?;
    Ok(())
}

/// [`StepMemory::remember_mismatched_item`], inside the caller's transaction.
pub(crate) fn remember_mismatched_item(conn: &Connection, item: LocalItemId, upload: &MismatchedItem) -> Result<(), TreeError> {
    write_fact(conn, item, |_, memory| {
        memory.mismatched_item = Some(upload.clone());
        Ok(())
    })
}

/// [`StepMemory::remember_temporary_name`], inside the caller's transaction.
pub(crate) fn remember_temporary_name(conn: &Connection, item: LocalItemId, temporary_name: &TemporaryName) -> Result<FactStanding, TreeError> {
    check_name(&temporary_name.name)?;
    write_fact(conn, item, |row, memory| {
        let fact_standing = temporary_name.decide_standing(row);
        if fact_standing == FactStanding::StillSaid {
            memory.temporary_name = Some(temporary_name.clone());
        }
        Ok(fact_standing)
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

/// [`StepMemory::write_free_space`], inside the caller's transaction.
pub(crate) fn write_free_space(conn: &Connection, free_space: u64) -> Result<usize, TreeError> {
    let mut let_go = 0;
    for (item, _) in list_memories(conn, "space_refusal_size IS NOT NULL", &[])? {
        change_memory(conn, item, |_, memory| {
            match memory.space_refusal.as_mut().map(|space_refusal| (space_refusal.decide_fit(free_space), space_refusal)) {
                Some((SpaceFit::StillTooBig, space_refusal)) => space_refusal.free_space = free_space,
                Some((SpaceFit::Fits, _)) => {
                    memory.space_refusal = None;
                    let_go += 1;
                }
                None => {}
            }
            Ok(())
        })?;
    }
    Ok(let_go)
}

/// [`StepMemory::remember_removals_confirmed`], inside the caller's transaction.
pub(crate) fn remember_removals_confirmed(conn: &Connection, items: &[LocalItemId]) -> Result<usize, TreeError> {
    let mut written = 0;
    for item in items {
        change_memory(conn, *item, |row, memory| {
            if decide_removal_confirmation_standing(row) == FactStanding::StillSaid {
                memory.is_removal_confirmed = true;
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
    if fact == StepFact::UploadSession {
        // Taken out before what the row does not say goes to the abandoned ones: a session
        // that ended is not cancelled, whatever the file's content is by now.
        let mut kept = read_memory(conn, item)?;
        if kept.upload_session.take().is_some() {
            write_memory(conn, item, &kept)?;
        }
    }
    change_memory(conn, item, |_, memory| {
        match fact {
            StepFact::UploadSession => memory.upload_session = None,
            StepFact::MismatchedItem => memory.mismatched_item = None,
            StepFact::TemporaryName => memory.temporary_name = None,
            StepFact::FailedTries => memory.failed_tries = None,
            StepFact::SpaceRefusal => memory.space_refusal = None,
            StepFact::RemovalConfirmation => memory.is_removal_confirmed = false,
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

/// The binding of `from` is `to`'s from now on: what is remembered follows, inside the
/// transaction of the operation that moves the binding, before the row of `from` goes if it
/// does.
///
/// - The confirmation of a removal goes to `to` with the binding.
/// - The upload session, the temporary name, the refusal and the failed tries of both items
///   are dropped, a session to the abandoned ones: each was learnt of a request to OneDrive
///   for the item as it was bound before.
/// - What was asked of the directory `from` was bound to is with `to`: the openings and the
///   abandoned sessions' empty files in it move, the upload sessions of new files in it are
///   abandoned, and the names OneDrive answered are taken in it are dropped.
/// - The item an upload left with other content and a file too big for the space stay.
pub(super) fn follow_binding(conn: &Connection, from: LocalItemId, to: LocalItemId) -> Result<(), TreeError> {
    let (mut of_from, mut of_to) = (read_memory(conn, from)?, read_memory(conn, to)?);
    let (kept_of_from, kept_of_to) = (of_from.clone(), of_to.clone());
    of_to.is_removal_confirmed |= of_from.is_removal_confirmed;
    of_from.is_removal_confirmed = false;
    for memory in [&mut of_from, &mut of_to] {
        if let Some(session) = memory.upload_session.take() {
            abandon(conn, &session)?;
        }
        (memory.temporary_name, memory.refusal, memory.failed_tries) = (None, None, None);
    }
    for (item, memory, kept) in [(from, &of_from, &kept_of_from), (to, &of_to, &kept_of_to)] {
        if memory != kept {
            write_memory(conn, item, memory)?;
        }
    }
    let in_directory: [Value; 1] = [from.as_i64().into()];
    for (item, mut memory) in list_memories(conn, "session_placeholder_parent_id = ?1", &in_directory)? {
        if let Some(session) = memory.upload_session.take() {
            abandon(conn, &session)?;
        }
        write_memory(conn, item, &memory)?;
    }
    for (item, mut memory) in list_memories(conn, "taken_parent_id = ?1", &in_directory)? {
        memory.taken_name = None;
        write_memory(conn, item, &memory)?;
    }
    move_openings(conn, from, to)?;
    move_placeholders(conn, from, to)
}

#[cfg(test)]
mod tests;
