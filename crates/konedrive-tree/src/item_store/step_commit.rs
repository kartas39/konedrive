//! What a step of the sync plan writes into the rows once it is done: its commit. A step
//! runs outside the lock, for seconds or minutes; its commit, in one transaction, asks
//! whether the versions the step was planned from ([`ItemVersions`]) are still the row's,
//! and writes what the step's confirmation writes, or nothing.
//!
//! One function for each case of a [`SyncStep`](crate::SyncStep), named after it. Each is
//! given the versions the step carries, what else the case holds, and what only OneDrive or
//! the disk can tell: the item as Graph answered the request ([`ConfirmedItem`]), the hash
//! that was computed, what the upload rules say of a name the item gets here. It reads
//! everything else off the rows.
//!
//! **What a commit answers** ([`StepCommitOutcome`]). *Written*: the rows say what the step
//! did. *The rows changed*: nothing was written, and the plan made next decides. That is
//! no error. It is answered when the versions of the step's item are not the row's any
//! more, and when another row the step was planned against is not as it was: the directory
//! the item was sent into has no binding any more or OneDrive does not have it, the place the item was to take in the
//! folder is another item's, the folder shows something in a directory that was to leave
//! it. An error ([`TreeError::Refused`]) is left for what no plan asks for: a step of a
//! kind of item it is not for, an answer of OneDrive that is of another item.
//!
//! **Which versions are asked.** A step towards the folder and a step of the rows alone
//! ask all of them, and whether the content is on disk as it was
//! ([`ItemVersions::are_those_of`]). A step that made, moved or filled an item in OneDrive
//! asks the OneDrive version and what was agreed on
//! ([`ItemVersions::are_onedrive_and_synced_those_of`]): while its request ran a program
//! may have renamed the item or changed its content again, and what OneDrive confirmed is
//! written all the same, in the part that was confirmed — OneDrive's version as Graph answered it,
//! the place that was sent as the place agreed on, the content that was sent as the content
//! agreed on — and as the local content only when that is still the one that was sent. What
//! the program did since then differs from what was agreed on, and the next plan sends it.
//! The removal of an item asks all of them: the folder does not show what was removed.
//!
//! **Which row is written.** What OneDrive confirmed of an item it had is written to the
//! row that has the item's binding at the commit. That is the row the step was planned
//! from, unless a file was saved by a rename over it meanwhile and took the binding
//! ([`take_over_binding`](super::bindings)): the client's own upload is then confirmed of
//! the replacing file, and is never read as a change OneDrive made.
//!
//! **A directory's removal** is sent on a positive condition, which the plan decides, the
//! step asks again before its request leaves
//! ([`ItemStore::can_directory_removal_be_sent`]) and the commit asks once more, in its
//! transaction, each time by the one function
//! (`sync_plan::removed_directory::decide_directory_removal`): all the rows say OneDrive
//! has below the directory was shown here and removed here, and OneDrive changed the
//! content of none of it since. Every row it takes the OneDrive version from is stamped
//! with it.
//!
//! **What a commit of a step towards OneDrive writes of OneDrive** goes through the one
//! confirmation ([`confirm_onedrive_version`]): the version, the eTag and the stamp of the
//! guard against an outdated listing, in one statement.
//!
//! **What the steps remember of the item** ([`step_memory`](super::step_memory)). What a
//! step learnt of a local change that was not sent is answered only while the row has the
//! stamp of that change, so a commit that makes the two sides agree forgets it by what it
//! writes, and so does every commit that gives or moves a binding, by the operation on the
//! binding: none of them says a word here. A commit says what the rows do not:
//!
//! - the commit of an upload is told the upload session that ended with it, which is
//!   forgotten and not cancelled ([`forget_ended_upload_session`]);
//! - the commits of an upload into an item OneDrive has and of a move may leave the item
//!   unsent, changed here again while the request ran, under the stamp it had: the failed
//!   tries of the request that went through are forgotten, the taken name of the place a
//!   move was confirmed to, and the temporary name once OneDrive confirmed the item at
//!   another place ([`ConfirmedRequest`]);
//! - a commit that writes that OneDrive does not have an item any more — a removal sent,
//!   a removal of OneDrive's taken by the folder — forgets the temporary name of what
//!   OneDrive does not have;
//! - the commit of a step towards the folder, or of the rows alone, that is written
//!   forgets the failed tries of a step for what OneDrive has of the item
//!   ([`forget_tries_when_written`]): they are told by the eTag, which such a step leaves;
//! - the commit of the step that hashed the local content carries that content, the same
//!   bytes, into the facts that hold it.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};

use rusqlite::{Connection, Transaction};

use super::bindings::{bind_item, give_up_binding, merge_items};
use super::local_content::with_modified_time;
use super::item_row::{confirm_removal_below, find_bound_item, find_item, list_below_in_onedrive, write_item};
use super::onedrive_versions::{confirm_onedrive_version, ListingUnderWay};
use super::place_change_records::{forget_place_change_record_when_written, PlaceChangeRecordId};
use super::sent_removal_window::add_sent_removals;
use super::step_memory::{
    carry_content, forget_confirmed_request, forget_ended_upload_session, forget_temporary_names_of_items_onedrive_lacks, forget_tries_of_onedrive_version, ConfirmedRequest, UploadSessionAddress,
};
use super::{check_can_hold, check_directory, check_empty, find_item_at, refuse, ItemStore};
use crate::model::{ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, ItemVersion, LocalItemId, OneDriveItemReference};
use crate::sync_plan::{compare_local_content, decide_directory_removal, ContentChange, DirectoryRemovalOutcome, ItemBelowRemovedDirectory, ItemVersions};
use crate::{ConfirmedOneDriveVersion, FoldedName, TreeError};

/// What became of a step's commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepCommitOutcome {
    /// The rows say what the step did.
    Written,
    /// The rows are not those the step was planned from: nothing was written.
    RowsChanged,
}

/// An item as OneDrive has it after a step's request, as Graph answered that request with
/// it: what a step that made, moved or filled an item gives its commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedItem {
    pub item: OneDriveItemReference,
    /// The directory OneDrive has the item in.
    pub parent: OneDriveItemReference,
    /// The name OneDrive stored, which may be another than was sent.
    pub name: OsString,
    /// A file's content, told as OneDrive tells one ([`ContentVersion::Hashed`]); `None` for
    /// a directory.
    pub content: Option<ContentVersion>,
    pub etag: String,
}

impl ItemStore<'_> {
    /// The commit of [`OneDriveStep::MakeDirectory`](crate::OneDriveStep::MakeDirectory):
    /// OneDrive made the directory, sent into `parent` under `name`, as `made`. The item has
    /// that binding, OneDrive's version is what was answered, and the place that was sent is
    /// the place agreed on. A row a listing made for the new item meanwhile is merged into
    /// this one; where that listing said another eTag than the answer's, another client
    /// changed the item since it was made, and OneDrive's version stays what the listing
    /// said: the plan made next takes that change over. When the rows changed, nothing is
    /// written, and the directory OneDrive made is the step's to remove where the item's
    /// row is gone.
    pub fn commit_make_directory(&mut self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr, made: &ConfirmedItem) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_new_item(tx, item, ItemKind::Directory, parent, name, made, None))
    }

    /// The commit of [`OneDriveStep::SendNewFile`](crate::OneDriveStep::SendNewFile): as for
    /// a directory ([`commit_make_directory`](Self::commit_make_directory)), and the
    /// content OneDrive answered is the content agreed on, and the local content too when
    /// that is still the one that was sent. `name` is the name the request was made under:
    /// for an upload that went on with an upload session, the name of the session's empty
    /// file, whatever the file is called here by now. `ended_session` is the upload
    /// session that ended with the upload, when one sent it: it is forgotten, and not
    /// cancelled.
    pub fn commit_send_new_file(&mut self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr, made: &ConfirmedItem, ended_session: Option<&UploadSessionAddress>) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_new_item(tx, item, ItemKind::File, parent, name, made, ended_session))
    }

    /// The commit of [`OneDriveStep::SendContent`](crate::OneDriveStep::SendContent):
    /// OneDrive has the content that was sent, and answered the item as `sent`. OneDrive's
    /// version is what was answered, its content is the content agreed on, and the local
    /// content too when that is still the one that was sent. `ended_session` is the upload
    /// session that ended with the upload, when one sent it: it is forgotten, and not
    /// cancelled.
    pub fn commit_send_content(&mut self, item: &ItemVersions, sent: &ConfirmedItem, ended_session: Option<&UploadSessionAddress>) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_send_content(tx, item, sent, ended_session))
    }

    /// The commit of [`OneDriveStep::SendMove`](crate::OneDriveStep::SendMove): OneDrive
    /// moved the item, sent into `parent` under `name`, and answered it as `moved`.
    /// OneDrive's version is what was answered, and the place that was sent is the place
    /// agreed on: a name OneDrive stored otherwise than it was sent then reads as OneDrive's
    /// change of the place, and an item sent to a temporary name as moved here still.
    pub fn commit_send_move(&mut self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr, moved: &ConfirmedItem) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_send_move(tx, item, parent, name, moved))
    }

    /// The commit of [`OneDriveStep::SendFileRemoval`](crate::OneDriveStep::SendFileRemoval):
    /// OneDrive removed the file. The row that has its binding is left with the binding
    /// alone, stamped, for the guard against an outdated listing. That is the row the step
    /// was planned from, unless a file was made at its place meanwhile and took the
    /// binding: that file then gives the binding up to such a row, and is an item OneDrive
    /// does not have, which the next plan sends as a new one. `now` is the time, in Unix
    /// seconds: the removal is counted among those sent a short time ago
    /// ([`find_sent_removal_window`](Self::find_sent_removal_window)).
    pub fn commit_send_file_removal(&mut self, item: &ItemVersions, now: i64) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_send_file_removal(tx, item, now))
    }

    /// The commit of
    /// [`OneDriveStep::SendDirectoryRemoval`](crate::OneDriveStep::SendDirectoryRemoval):
    /// OneDrive removed the directory with all it had in it. The rows of the directory and
    /// of each of `below`, what was removed here with it, are left with their binding
    /// alone, stamped. Nothing is written unless the rows still say what the removal was
    /// planned from and sent on: the versions of the directory and of every one of `below`
    /// are their rows', and `below` is all the rows say OneDrive has below the directory —
    /// what [`can_directory_removal_be_sent`](Self::can_directory_removal_be_sent) asks,
    /// of the rows alone. When the rows changed, OneDrive has removed what they still say
    /// it has: the step asks for a listing. `now` is the time, in Unix seconds: the
    /// directory and each of `below` are counted among the removals sent a short time ago
    /// ([`find_sent_removal_window`](Self::find_sent_removal_window)).
    pub fn commit_send_directory_removal(&mut self, item: &ItemVersions, below: &[ItemVersions], now: i64) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_send_directory_removal(tx, item, below, now))
    }

    /// Whether the removal of the directory of `item`, planned with `below` as what was
    /// removed here with it, can be sent as the rows are now: what the step asks under the
    /// lock before its request leaves. A directory's removal is unguarded in OneDrive and
    /// takes all that is in it, so it is sent on a positive condition, the one the plan
    /// decided it on (`sync_plan::removed_directory::decide_directory_removal`), asked
    /// again of the rows as they are: no listing is under way; the versions of the
    /// directory and of each of `below` are still their rows'; and all the rows say
    /// OneDrive has in the directory and below it was shown here and removed here — it is
    /// `below`, no row more and none less, none shown by the folder, none the folder never
    /// showed, and none a file whose content OneDrive changed since it was removed here.
    ///
    /// What the rows do not say, the caller gives, as it gives it to the plan
    /// ([`SyncPlanInput`](crate::SyncPlanInput)) and with nothing taken out:
    /// `items_with_running_step`, the items a request of this client that is not settled
    /// is about, and `destinations_of_running_steps`, the directories such a request
    /// sends an item into. The removal that asks is such a request itself once it is on
    /// record, about the directory and each of `below`: those are left out of
    /// `items_with_running_step` here, so that the removal is not held back by itself.
    /// It sends nothing into a directory, so nothing is left out of
    /// `destinations_of_running_steps`: the removal is not sent while another request
    /// sends an item into the directory or into a directory below it — an entry the plan
    /// was not given is then no removal.
    ///
    /// What that costs: another request about the directory or one of `below` that is not
    /// settled reads as the removal's own. The plan gives no removal while one stands
    /// (such an item is held back, and the directory waits for it), and one step runs for
    /// an item at a time, so none is made between the plan and this question. And an item
    /// below the directory that is not of `below` holds the removal whatever is said of
    /// it: the rows are then not as planned.
    pub fn can_directory_removal_be_sent(&self, item: &ItemVersions, below: &[ItemVersions], items_with_running_step: &BTreeSet<LocalItemId>, destinations_of_running_steps: &BTreeSet<LocalItemId>) -> Result<bool, TreeError> {
        let conn = &self.store.conn;
        let is_of_this_removal = |asked: LocalItemId| asked == item.local_item_id || below.iter().any(|removed| removed.local_item_id == asked);
        let is_held_back = |asked: LocalItemId| (items_with_running_step.contains(&asked) && !is_of_this_removal(asked)) || destinations_of_running_steps.contains(&asked);
        Ok(!is_held_back(item.local_item_id) && is_directory_removal_as_planned(conn, item, below, ListingUnderWay::find(conn)?.is_some(), &is_held_back)?)
    }

    /// The commit of [`FolderStep::ShowItem`](crate::FolderStep::ShowItem): the folder
    /// shows the item at OneDrive's place, a file with OneDrive's content, not on disk, and
    /// the two sides agree on both. `is_name_ignored` is what the upload rules say of its
    /// name there.
    pub fn commit_show_item(&mut self, item: &ItemVersions, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_tries_when_written(tx, item, commit_show_item(tx, item, is_name_ignored)?))
    }

    /// The commit of [`FolderStep::MoveItem`](crate::FolderStep::MoveItem): the folder
    /// shows the item at OneDrive's place, which is the place agreed on. `is_name_ignored`
    /// is what the upload rules say of its name there.
    /// `record` is the record of the step's change of the backing directory's names, when
    /// it made one: it goes in this transaction when the commit is written, and stands
    /// when the rows changed, for the caller to take after its undo.
    pub fn commit_move_item(&mut self, item: &ItemVersions, is_name_ignored: bool, record: Option<PlaceChangeRecordId>) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_place_change_record_when_written(tx, record, forget_tries_when_written(tx, item, commit_move_to_onedrive_place(tx, item, is_name_ignored)?)?))
    }

    /// The commit of
    /// [`FolderStep::TakeOneDriveContent`](crate::FolderStep::TakeOneDriveContent): the
    /// file shows OneDrive's content, which is the content agreed on, and its content is
    /// not on disk. The row is written before the copy on disk goes.
    /// `record` is the record of the step's change of the backing directory's names, when
    /// it made one: it goes in this transaction when the commit is written, and stands
    /// when the rows changed, for the caller to take after its undo.
    pub fn commit_take_onedrive_content(&mut self, item: &ItemVersions, record: Option<PlaceChangeRecordId>) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_place_change_record_when_written(tx, record, forget_tries_when_written(tx, item, commit_take_onedrive_content(tx, item)?)?))
    }

    /// The commit of [`FolderStep::RemoveItem`](crate::FolderStep::RemoveItem): the folder
    /// does not show the item, and nothing is agreed on of it any more. The row of an item
    /// OneDrive removed is then a binding alone; that of an item OneDrive has where the
    /// folder cannot show it is that of an item of OneDrive the folder never showed, so that
    /// nothing reads as removed here and no removal is sent. The rows changed while the
    /// folder shows something in the item, a directory.
    /// `record` is the record of the step's change of the backing directory's names, when
    /// it made one: it goes in this transaction when the commit is written, and stands
    /// when the rows changed, for the caller to take after its undo.
    pub fn commit_remove_item(&mut self, item: &ItemVersions, record: Option<PlaceChangeRecordId>) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_place_change_record_when_written(tx, record, forget_tries_when_written(tx, item, commit_remove_item(tx, item)?)?))
    }

    /// The commit of [`FolderStep::DownloadContent`](crate::FolderStep::DownloadContent):
    /// the whole content of the file is on disk.
    /// `record` is the record of the step's change of the backing directory's names, when
    /// it made one: it goes in this transaction when the commit is written, and stands
    /// when the rows changed, for the caller to take after its undo.
    pub fn commit_download_content(&mut self, item: &ItemVersions, record: Option<PlaceChangeRecordId>) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_place_change_record_when_written(tx, record, forget_tries_when_written(tx, item, commit_download_content(tx, item)?)?))
    }

    /// The commit of
    /// [`FolderStep::RenameToConflictCopy`](crate::FolderStep::RenameToConflictCopy): the
    /// folder shows the item under `name` in the directory it is in. `is_name_ignored` is
    /// what the upload rules say of that name. Nothing agreed on changes.
    pub fn commit_rename_to_conflict_copy(&mut self, item: &ItemVersions, name: &OsStr, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_tries_when_written(tx, item, commit_rename_to_conflict_copy(tx, item, name, is_name_ignored)?))
    }

    /// The commit of [`FolderStep::TakeMoveBack`](crate::FolderStep::TakeMoveBack): the
    /// folder shows the item at OneDrive's place again, which is the place agreed on.
    /// `is_name_ignored` is what the upload rules say of its name there.
    /// `record` is the record of the step's change of the backing directory's names, when
    /// it made one: it goes in this transaction when the commit is written, and stands
    /// when the rows changed, for the caller to take after its undo.
    pub fn commit_take_move_back(&mut self, item: &ItemVersions, is_name_ignored: bool, record: Option<PlaceChangeRecordId>) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_place_change_record_when_written(tx, record, forget_tries_when_written(tx, item, commit_move_to_onedrive_place(tx, item, is_name_ignored)?)?))
    }

    /// The commit of
    /// [`FolderStep::MoveToTemporaryName`](crate::FolderStep::MoveToTemporaryName): the
    /// folder shows the item at `place`, which is the place agreed on from now.
    /// `is_name_ignored` is what the upload rules say of its name there.
    /// `record` is the record of the step's change of the backing directory's names, when
    /// it made one: it goes in this transaction when the commit is written, and stands
    /// when the rows changed, for the caller to take after its undo.
    pub fn commit_move_to_temporary_name(&mut self, item: &ItemVersions, place: &ItemPlace, is_name_ignored: bool, record: Option<PlaceChangeRecordId>) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_place_change_record_when_written(tx, record, forget_tries_when_written(tx, item, commit_move_to_temporary_name(tx, item, place, is_name_ignored)?)?))
    }

    /// The commit of [`RowStep::GiveUpBinding`](crate::RowStep::GiveUpBinding)
    /// ([`give_up_binding`](Self::give_up_binding)). `is_name_ignored` is what the upload
    /// rules say of the item's name, asked of it from now on.
    pub fn commit_give_up_binding(&mut self, item: &ItemVersions, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_give_up_binding(tx, item, is_name_ignored))
    }

    /// The commit of [`RowStep::MergeItems`](crate::RowStep::MergeItems)
    /// ([`merge_items`](Self::merge_items)). Nothing is written unless the versions of both
    /// items are still their rows'. A merge is something the two sides agree on, and the
    /// commit writes it: the place the folder has the item at is the place agreed on,
    /// where OneDrive has it in the same directory under a name that is the same as
    /// OneDrive compares names, and the local content is the content agreed on, where it
    /// is the same bytes as OneDrive's. Another spelling of the name in OneDrive, another
    /// cTag or time of the content then read as what OneDrive changed, which the plan
    /// made next follows or agrees on; and the row is no more one of which nothing is
    /// agreed on, whose binding a later plan would take for a guess.
    pub fn commit_merge_items(&mut self, local: &ItemVersions, onedrive: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_merge_items(tx, local, onedrive))
    }

    /// The commit of [`RowStep::AgreeOnRemoval`](crate::RowStep::AgreeOnRemoval): both
    /// sides removed the item; what they agreed on before is forgotten, and the row is a
    /// binding alone.
    pub fn commit_agree_on_removal(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_tries_when_written(tx, item, commit_forget_synced_version(tx, item, false)?))
    }

    /// The commit of
    /// [`RowStep::ForgetSyncedVersion`](crate::RowStep::ForgetSyncedVersion): what the two
    /// sides agreed on of an item the folder does not show and OneDrive has is forgotten.
    pub fn commit_forget_synced_version(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_tries_when_written(tx, item, commit_forget_synced_version(tx, item, true)?))
    }

    /// The commit of [`RowStep::HashLocalContent`](crate::RowStep::HashLocalContent): the
    /// local content, of the size and the time it had, is told by `hash`, which was
    /// computed from the bytes on disk.
    pub fn commit_hash_local_content(&mut self, item: &ItemVersions, hash: &str) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_tries_when_written(tx, item, commit_hash_local_content(tx, item, hash)?))
    }

    /// The commit of [`RowStep::AgreeOnPlace`](crate::RowStep::AgreeOnPlace): the place both
    /// sides have the item at is the place agreed on.
    pub fn commit_agree_on_place(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_tries_when_written(tx, item, commit_agree_on_place(tx, item)?))
    }

    /// The commit of [`RowStep::AgreeOnContent`](crate::RowStep::AgreeOnContent): OneDrive's
    /// content version, with the time the local content has, is the local one and the one
    /// agreed on. The bytes are the same and no file is written, so the time here stays:
    /// it is the time of the copy on disk, which the check at start compares the row
    /// with, and what a program set or saw. OneDrive's content keeps OneDrive's time.
    pub fn commit_agree_on_content(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| forget_tries_when_written(tx, item, commit_agree_on_content(tx, item)?))
    }
}

/// The commit of a step towards the folder, or of the rows alone, answered `outcome` for
/// `item`: written, the failed tries of a step for what OneDrive has of the item are over.
pub(super) fn forget_tries_when_written(tx: &Transaction<'_>, item: &ItemVersions, outcome: StepCommitOutcome) -> Result<StepCommitOutcome, TreeError> {
    if outcome == StepCommitOutcome::Written {
        forget_tries_of_onedrive_version(tx, item.local_item_id)?;
    }
    Ok(outcome)
}

/// The row of the item of `planned`, when every version it was planned from is still the
/// row's.
fn find_planned(conn: &Connection, planned: &ItemVersions) -> Result<Option<Item>, TreeError> {
    Ok(find_item(conn, planned.local_item_id)?.filter(|row| planned.are_those_of(row)))
}

/// The row of the item of `planned`, an item OneDrive does not have yet, when the folder
/// may show it otherwise than it was planned from and nothing else of it changed.
fn find_planned_new_item(conn: &Connection, planned: &ItemVersions) -> Result<Option<Item>, TreeError> {
    Ok(find_item(conn, planned.local_item_id)?.filter(|row| planned.are_onedrive_and_synced_those_of(row)))
}

/// Whether the removal of the directory of `item` with `below` is still what the rows say
/// ([`ItemStore::can_directory_removal_be_sent`]): the directory's row is as planned, and
/// the plan's own decision ([`decide_directory_removal`]), made from all the rows say
/// OneDrive has below it now, is that the removal is sent, for `below` and nothing else.
/// `is_held_back`: what the caller says of an item the rows do not say.
fn is_directory_removal_as_planned(conn: &Connection, item: &ItemVersions, below: &[ItemVersions], is_listing_under_way: bool, is_held_back: &dyn Fn(LocalItemId) -> bool) -> Result<bool, TreeError> {
    if find_planned(conn, item)?.is_none() {
        return Ok(false);
    }
    let rows_below: Vec<ItemBelowRemovedDirectory> = list_below_in_onedrive(conn, item.local_item_id)?.iter().map(|row| ItemBelowRemovedDirectory::of(row, is_held_back(row.local_item_id))).collect();
    let by_id = |versions: &[ItemVersions]| {
        let mut versions: Vec<ItemVersions> = versions.to_vec();
        versions.sort_by_key(|versions| versions.local_item_id);
        versions
    };
    Ok(match decide_directory_removal(is_listing_under_way, &rows_below) {
        DirectoryRemovalOutcome::IsSent { removed_with_it } => by_id(&removed_with_it) == by_id(below),
        DirectoryRemovalOutcome::IsOfFolderAlone | DirectoryRemovalOutcome::WaitsFor(_) | DirectoryRemovalOutcome::WaitsForListing => false,
    })
}

/// The directory that has the binding `directory`, when a row has it and says OneDrive has
/// the directory: nothing is confirmed to be in a directory OneDrive removed meanwhile.
fn find_bound_directory(conn: &Connection, directory: &OneDriveItemReference) -> Result<Option<LocalItemId>, TreeError> {
    Ok(find_bound_item(conn, directory)?.filter(|row| row.kind == ItemKind::Directory && row.onedrive.is_some()).map(|row| row.local_item_id))
}

/// Whether a check of the rows' rules held: a refusal says it did not, and anything else
/// that failed is the caller's error.
fn holds(check: Result<(), TreeError>) -> Result<bool, TreeError> {
    match check {
        Ok(()) => Ok(true),
        Err(TreeError::Refused(_)) => Ok(false),
        Err(other) => Err(other),
    }
}

/// Whether the folder can show `item` at `place` as the rows are now: it shows the
/// directory, nothing else has the name there, and the item is not placed below itself or
/// too deep.
fn can_be_shown_at(conn: &Connection, item: LocalItemId, place: &ItemPlace) -> Result<bool, TreeError> {
    let is_free = find_item_at(conn, place.parent_local_item_id, &place.name)?.is_none_or(|holder| holder.local_item_id == item);
    Ok(is_free && holds(check_directory(conn, place.parent_local_item_id))? && holds(check_can_hold(conn, place.parent_local_item_id, item))?)
}

/// A step of another kind of item than it is for, or of a row it cannot be of.
fn refuse_step(item: LocalItemId, what: &str) -> TreeError {
    refuse(format!("no step of this kind is planned for item {item}: {what}"))
}

/// The row of `item`, read again after something was written to it.
fn read_row(conn: &Connection, item: LocalItemId) -> Result<Item, TreeError> {
    find_item(conn, item)?.ok_or_else(|| refuse(format!("item {item} has no row")))
}

/// What OneDrive answered of an item is what OneDrive has of `item`: its version, with the
/// directory resolved, and its eTag, written by the one confirmation. `false`, with
/// nothing written, when no row has the binding of the directory OneDrive has it in.
fn confirm_item(tx: &Transaction<'_>, item: LocalItemId, confirmed: &ConfirmedItem) -> Result<bool, TreeError> {
    let Some(directory) = find_bound_directory(tx, &confirmed.parent)? else { return Ok(false) };
    let version = ItemVersion { place: Some(ItemPlace { parent_local_item_id: directory, name: confirmed.name.clone() }), content: confirmed.content.clone() };
    confirm_onedrive_version(tx, item, &ConfirmedOneDriveVersion::Present { version, etag: confirmed.etag.clone() })?;
    Ok(true)
}

/// The content OneDrive confirmed is the content agreed on of `row`, and its local content
/// too when that is still `sent`, the one the step was planned from. Where a program
/// opened the file for writing since, `sent` is the content kept from before its first
/// writer, and what was confirmed is kept there instead: closed unchanged, the file agrees.
fn agree_on_confirmed_content(row: &mut Item, sent: Option<&ContentVersion>, confirmed: Option<&ContentVersion>) {
    row.synced_content = confirmed.cloned();
    if row.content_before_write.is_some() && row.content_before_write.as_ref() == sent {
        row.content_before_write = confirmed.cloned();
    }
    if let Some(local) = row.local.as_mut().filter(|local| local.content.as_ref() == sent) {
        local.content = confirmed.cloned();
    }
}

/// [`ItemStore::commit_make_directory`] and [`ItemStore::commit_send_new_file`], inside the
/// caller's transaction, which is dropped on any error.
pub(crate) fn commit_new_item(
    tx: &Transaction<'_>,
    item: &ItemVersions,
    kind: ItemKind,
    parent: &OneDriveItemReference,
    name: &OsStr,
    made: &ConfirmedItem,
    ended_session: Option<&UploadSessionAddress>,
) -> Result<StepCommitOutcome, TreeError> {
    let id = item.local_item_id;
    let Some(row) = find_planned_new_item(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    if row.kind != kind || row.onedrive_item.is_some() || row.local.is_none() {
        return Err(refuse_step(id, "it is of another kind, has a binding, or the folder does not show it"));
    }
    let (Some(sent_directory), Some(_)) = (find_bound_directory(tx, parent)?, find_bound_directory(tx, &made.parent)?) else { return Ok(StepCommitOutcome::RowsChanged) };
    // A listing brought the new item, and the folder shows it as an item of its own.
    let listed = find_bound_item(tx, &made.item)?;
    if listed.as_ref().is_some_and(|holder| holder.local.is_some()) {
        return Ok(StepCommitOutcome::RowsChanged);
    }
    // A listing brought it under another eTag than the answer's: another client changed
    // it since OneDrive made it, and what the listing said is the later version.
    let is_listed_changed = listed.is_some_and(|holder| holder.onedrive.is_some() && holder.onedrive_etag.as_deref() != Some(made.etag.as_str()));
    // Before the binding is given: the item gets a binding, and a session still kept then
    // is taken for one to cancel. All else the steps learnt of the item as one OneDrive did
    // not have goes with that. The upload ended whichever version of OneDrive's the row
    // keeps: its session is forgotten also where the listing's stays.
    if let Some(address) = ended_session {
        forget_ended_upload_session(tx, id, address)?;
    }
    let sent = item.local.as_ref().and_then(|local| local.content.as_ref());
    bind_item(tx, id, &made.item)?;
    if !is_listed_changed && !confirm_item(tx, id, made)? {
        return Err(refuse(format!("the directory OneDrive has item {id} in lost its row within one transaction")));
    }
    let mut row = read_row(tx, id)?;
    row.synced_place = Some(ItemPlace { parent_local_item_id: sent_directory, name: name.to_owned() });
    agree_on_confirmed_content(&mut row, sent, made.content.as_ref());
    write_item(tx, &row)?;
    Ok(StepCommitOutcome::Written)
}

/// The row that has now the binding of the item of `planned`, which OneDrive has and
/// answered a request about as `confirmed`: the row the step was planned from, or the row
/// of a file saved over it since. `None` when the rows changed: no row has the binding, or
/// what the row says of OneDrive's side and of what was agreed on is not what the step was
/// planned from. Refused when the answer is of another item than the step's.
fn find_planned_item_of_onedrive(conn: &Connection, planned: &ItemVersions, confirmed: &ConfirmedItem) -> Result<Option<Item>, TreeError> {
    if planned.onedrive_item.as_ref() != Some(&confirmed.item) {
        return Err(refuse(format!("OneDrive's answer is of the item {}, which is not item {}", confirmed.item.item_id.as_str(), planned.local_item_id)));
    }
    Ok(find_bound_item(conn, &confirmed.item)?.filter(|row| planned.are_onedrive_and_synced_those_of(row)))
}

/// [`ItemStore::commit_send_content`], inside the caller's transaction, which is dropped on
/// any error.
pub(crate) fn commit_send_content(tx: &Transaction<'_>, item: &ItemVersions, sent: &ConfirmedItem, ended_session: Option<&UploadSessionAddress>) -> Result<StepCommitOutcome, TreeError> {
    let Some(id) = find_planned_item_of_onedrive(tx, item, sent)?.map(|row| row.local_item_id) else { return Ok(StepCommitOutcome::RowsChanged) };
    if !confirm_item(tx, id, sent)? {
        return Ok(StepCommitOutcome::RowsChanged);
    }
    let mut row = read_row(tx, id)?;
    let sent_content = item.local.as_ref().and_then(|local| local.content.as_ref());
    agree_on_confirmed_content(&mut row, sent_content, sent.content.as_ref());
    write_item(tx, &row)?;
    forget_confirmed_request(tx, id, ConfirmedRequest::Upload { ended_session })?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_send_move`], inside the caller's transaction, which is dropped on any
/// error.
pub(crate) fn commit_send_move(tx: &Transaction<'_>, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr, moved: &ConfirmedItem) -> Result<StepCommitOutcome, TreeError> {
    let Some(sent_directory) = find_bound_directory(tx, parent)? else { return Ok(StepCommitOutcome::RowsChanged) };
    let Some(id) = find_planned_item_of_onedrive(tx, item, moved)?.map(|row| row.local_item_id) else { return Ok(StepCommitOutcome::RowsChanged) };
    if !confirm_item(tx, id, moved)? {
        return Ok(StepCommitOutcome::RowsChanged);
    }
    let mut row = read_row(tx, id)?;
    let sent_place = ItemPlace { parent_local_item_id: sent_directory, name: name.to_owned() };
    row.synced_place = Some(sent_place.clone());
    write_item(tx, &row)?;
    forget_confirmed_request(tx, id, ConfirmedRequest::Move { to: &sent_place })?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_send_file_removal`], inside the caller's transaction, which is
/// dropped on any error.
pub(crate) fn commit_send_file_removal(tx: &Transaction<'_>, item: &ItemVersions, now: i64) -> Result<StepCommitOutcome, TreeError> {
    let id = item.local_item_id;
    let (Some(binding), None) = (&item.onedrive_item, &item.local) else { return Err(refuse_step(id, "it has no binding, or the folder showed it when the step was planned")) };
    let Some(row) = find_bound_item(tx, binding)?.filter(|row| item.are_onedrive_and_synced_those_of(row)) else { return Ok(StepCommitOutcome::RowsChanged) };
    if row.kind != ItemKind::File {
        return Err(refuse_step(id, "it is no file"));
    }
    // A file made at the removed one's place has the binding: it is the user's own from
    // now on, and the row that takes the binding from it is the removed item's.
    let removed = match row.local {
        Some(_) => give_up_binding(tx, row.local_item_id, row.is_name_ignored)?,
        None => row.local_item_id,
    };
    confirm_onedrive_version(tx, removed, &ConfirmedOneDriveVersion::Removed)?;
    add_sent_removals(tx, 1, now)?;
    forget_temporary_names_of_items_onedrive_lacks(tx)?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_send_directory_removal`], inside the caller's transaction, which is
/// dropped on any error. The removal is confirmed of the directory; then all the rows say
/// OneDrive has below it, which are those of `below`, lose their OneDrive version and what
/// was agreed on, under the same stamp. The rows alone are asked: a listing under way and
/// a request not settled hold a removal back before it is sent, not once OneDrive
/// confirmed it.
pub(crate) fn commit_send_directory_removal(tx: &Transaction<'_>, item: &ItemVersions, below: &[ItemVersions], now: i64) -> Result<StepCommitOutcome, TreeError> {
    let id = item.local_item_id;
    let Some(row) = find_planned(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    if row.kind != ItemKind::Directory || row.local.is_some() || row.onedrive_item.is_none() {
        return Err(refuse_step(id, "it is of another kind, the folder shows it, or it has no binding"));
    }
    if !is_directory_removal_as_planned(tx, item, below, false, &|_| false)? {
        return Ok(StepCommitOutcome::RowsChanged);
    }
    let count = confirm_onedrive_version(tx, id, &ConfirmedOneDriveVersion::Removed)?;
    confirm_removal_below(tx, id, count)?;
    add_sent_removals(tx, 1 + below.len() as u64, now)?;
    forget_temporary_names_of_items_onedrive_lacks(tx)?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_show_item`], inside the caller's transaction, which is dropped on any
/// error.
pub(crate) fn commit_show_item(tx: &Transaction<'_>, item: &ItemVersions, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
    let id = item.local_item_id;
    let Some(mut row) = find_planned(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    let (None, Some(onedrive)) = (&row.local, row.onedrive.clone()) else { return Err(refuse_step(id, "the folder shows it, or OneDrive does not have it")) };
    let place = onedrive.place.clone().ok_or_else(|| refuse_step(id, "it is the root"))?;
    if !can_be_shown_at(tx, id, &place)? {
        return Ok(StepCommitOutcome::RowsChanged);
    }
    row.synced_place = Some(place);
    row.synced_content = onedrive.content.clone();
    row.local = Some(onedrive);
    row.content_presence = ContentPresence::NotOnDisk;
    row.content_before_write = None;
    row.is_name_ignored = is_name_ignored;
    write_item(tx, &row)?;
    Ok(StepCommitOutcome::Written)
}

/// The folder shows the item of `row` at `place` from now on, which is the place agreed on
/// when `is_agreed_on` says so. `false`, with nothing written, when the folder cannot show
/// it there as the rows are now.
fn write_local_place(tx: &Transaction<'_>, mut row: Item, place: ItemPlace, is_agreed_on: bool, is_name_ignored: bool) -> Result<bool, TreeError> {
    let id = row.local_item_id;
    let Some(local) = row.local.as_mut().filter(|local| local.place.is_some()) else { return Err(refuse_step(id, "the folder does not show it, or it is the root")) };
    if !can_be_shown_at(tx, id, &place)? {
        return Ok(false);
    }
    local.place = Some(place.clone());
    if is_agreed_on {
        row.synced_place = Some(place);
    }
    row.is_name_ignored = is_name_ignored;
    write_item(tx, &row)?;
    Ok(true)
}

fn outcome_of(is_written: bool) -> StepCommitOutcome {
    if is_written {
        StepCommitOutcome::Written
    } else {
        StepCommitOutcome::RowsChanged
    }
}

/// [`ItemStore::commit_move_item`] and [`ItemStore::commit_take_move_back`], inside the
/// caller's transaction, which is dropped on any error.
pub(crate) fn commit_move_to_onedrive_place(tx: &Transaction<'_>, item: &ItemVersions, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
    let Some(row) = find_planned(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    let place = row.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()).ok_or_else(|| refuse_step(item.local_item_id, "OneDrive has no place for it"))?;
    write_local_place(tx, row, place, true, is_name_ignored).map(outcome_of)
}

/// [`ItemStore::commit_move_to_temporary_name`], inside the caller's transaction, which is
/// dropped on any error.
pub(crate) fn commit_move_to_temporary_name(tx: &Transaction<'_>, item: &ItemVersions, place: &ItemPlace, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
    let Some(row) = find_planned(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    if row.onedrive_item.is_none() {
        return Err(refuse_step(item.local_item_id, "it has no binding, and nothing is agreed on of it"));
    }
    write_local_place(tx, row, place.clone(), true, is_name_ignored).map(outcome_of)
}

/// [`ItemStore::commit_rename_to_conflict_copy`], inside the caller's transaction, which is
/// dropped on any error.
pub(crate) fn commit_rename_to_conflict_copy(tx: &Transaction<'_>, item: &ItemVersions, name: &OsStr, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
    let Some(row) = find_planned(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    let directory = row.local.as_ref().and_then(|local| local.place.as_ref()).map(|place| place.parent_local_item_id).ok_or_else(|| refuse_step(item.local_item_id, "the folder does not show it"))?;
    write_local_place(tx, row, ItemPlace { parent_local_item_id: directory, name: name.to_owned() }, false, is_name_ignored).map(outcome_of)
}

/// The row of the item of `planned` as the folder shows it, a file both sides have, with
/// OneDrive's content; `None` when the rows changed.
fn find_planned_file_on_both_sides(conn: &Connection, planned: &ItemVersions) -> Result<Option<(Item, ContentVersion)>, TreeError> {
    let Some(row) = find_planned(conn, planned)? else { return Ok(None) };
    match (row.kind, &row.local, row.onedrive.as_ref().and_then(|onedrive| onedrive.content.clone())) {
        (ItemKind::File, Some(_), Some(content)) => Ok(Some((row, content))),
        _ => Err(refuse_step(planned.local_item_id, "it is no file both sides have")),
    }
}

/// `content`, OneDrive's or OneDrive's with the time the file has here, is the local
/// content of `row` and the content agreed on.
fn write_onedrive_content_as_local(tx: &Transaction<'_>, mut row: Item, content: ContentVersion) -> Result<(), TreeError> {
    if let Some(local) = row.local.as_mut() {
        local.content = Some(content.clone());
    }
    row.synced_content = Some(content);
    row.content_before_write = None;
    write_item(tx, &row)
}

/// [`ItemStore::commit_take_onedrive_content`], inside the caller's transaction, which is
/// dropped on any error.
pub(crate) fn commit_take_onedrive_content(tx: &Transaction<'_>, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
    let Some((mut row, content)) = find_planned_file_on_both_sides(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    row.content_presence = ContentPresence::NotOnDisk;
    write_onedrive_content_as_local(tx, row, content)?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_agree_on_content`], inside the caller's transaction, which is dropped
/// on any error.
pub(crate) fn commit_agree_on_content(tx: &Transaction<'_>, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
    let Some((row, content)) = find_planned_file_on_both_sides(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    let content = match row.local.as_ref().and_then(|local| local.content.as_ref()).map(ContentVersion::mtime) {
        Some((mtime, mtime_nsec)) => with_modified_time(&content, mtime, mtime_nsec),
        None => content,
    };
    write_onedrive_content_as_local(tx, row, content)?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_remove_item`], inside the caller's transaction, which is dropped on
/// any error.
pub(crate) fn commit_remove_item(tx: &Transaction<'_>, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
    let id = item.local_item_id;
    let Some(mut row) = find_planned(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    if row.local.as_ref().is_none_or(|local| local.place.is_none()) || row.onedrive_item.is_none() {
        return Err(refuse_step(id, "the folder does not show it, it is the root, or it has no binding"));
    }
    if !holds(check_empty(tx, id))? {
        return Ok(StepCommitOutcome::RowsChanged);
    }
    row.local = None;
    row.synced_place = None;
    row.synced_content = None;
    row.content_presence = ContentPresence::NotOnDisk;
    row.content_before_write = None;
    write_item(tx, &row)?;
    forget_temporary_names_of_items_onedrive_lacks(tx)?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_download_content`], inside the caller's transaction, which is dropped
/// on any error.
pub(crate) fn commit_download_content(tx: &Transaction<'_>, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
    let Some(mut row) = find_planned(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    if row.kind != ItemKind::File || row.local.is_none() {
        return Err(refuse_step(item.local_item_id, "it is no file the folder shows"));
    }
    row.content_presence = ContentPresence::OnDisk;
    write_item(tx, &row)?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_give_up_binding`], inside the caller's transaction, which is dropped
/// on any error.
pub(crate) fn commit_give_up_binding(tx: &Transaction<'_>, item: &ItemVersions, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
    if find_planned(tx, item)?.is_none() {
        return Ok(StepCommitOutcome::RowsChanged);
    }
    give_up_binding(tx, item.local_item_id, is_name_ignored)?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_merge_items`], inside the caller's transaction, which is dropped on
/// any error.
pub(crate) fn commit_merge_items(tx: &Transaction<'_>, local: &ItemVersions, onedrive: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
    if find_planned(tx, local)?.is_none() || find_planned(tx, onedrive)?.is_none() {
        return Ok(StepCommitOutcome::RowsChanged);
    }
    merge_items(tx, local.local_item_id, onedrive.local_item_id)?;
    let mut row = read_row(tx, local.local_item_id)?;
    let (here, in_onedrive) = (row.local.clone(), row.onedrive.clone());
    let place_of = |version: &Option<ItemVersion>| version.as_ref().and_then(|version| version.place.clone());
    let content_of = |version: &Option<ItemVersion>| version.as_ref().and_then(|version| version.content.clone());
    if let (Some(here), Some(in_onedrive)) = (place_of(&here), place_of(&in_onedrive)) {
        if here.parent_local_item_id == in_onedrive.parent_local_item_id && FoldedName::of(&here.name) == FoldedName::of(&in_onedrive.name) {
            row.synced_place = Some(here);
        }
    }
    if row.kind == ItemKind::File && compare_local_content(content_of(&here).as_ref(), content_of(&in_onedrive).as_ref()) == ContentChange::Same {
        row.synced_content = content_of(&here);
    }
    write_item(tx, &row)?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_agree_on_removal`] and [`ItemStore::commit_forget_synced_version`], inside
/// the caller's transaction, which is dropped on any error. `is_in_onedrive`: whether the
/// step is for an item OneDrive has.
pub(crate) fn commit_forget_synced_version(tx: &Transaction<'_>, item: &ItemVersions, is_in_onedrive: bool) -> Result<StepCommitOutcome, TreeError> {
    let Some(mut row) = find_planned(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    if row.local.is_some() || row.onedrive.is_some() != is_in_onedrive {
        return Err(refuse_step(item.local_item_id, "the folder shows it, or OneDrive has it where it was to have removed it, or the other way"));
    }
    row.synced_place = None;
    row.synced_content = None;
    write_item(tx, &row)?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_hash_local_content`], inside the caller's transaction, which is
/// dropped on any error.
pub(crate) fn commit_hash_local_content(tx: &Transaction<'_>, item: &ItemVersions, hash: &str) -> Result<StepCommitOutcome, TreeError> {
    let Some(mut row) = find_planned(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    let Some((local, content)) = row.local.as_mut().and_then(|local| local.content.clone().map(|content| (local, content))) else {
        return Err(refuse_step(item.local_item_id, "the folder shows no content of it"));
    };
    let (mtime, mtime_nsec) = content.mtime();
    let hashed = ContentVersion::Hashed { size: content.size(), mtime, mtime_nsec, hash: Some(hash.to_owned()), ctag: None };
    local.content = Some(hashed.clone());
    write_item(tx, &row)?;
    carry_content(tx, item.local_item_id, &content, &hashed)?;
    Ok(StepCommitOutcome::Written)
}

/// [`ItemStore::commit_agree_on_place`], inside the caller's transaction, which is dropped
/// on any error.
pub(crate) fn commit_agree_on_place(tx: &Transaction<'_>, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
    let Some(mut row) = find_planned(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    let place = row.local.as_ref().and_then(|local| local.place.clone());
    if place.is_none() || place != row.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()) {
        return Err(refuse_step(item.local_item_id, "the two sides do not have it at one place"));
    }
    row.synced_place = place;
    write_item(tx, &row)?;
    Ok(StepCommitOutcome::Written)
}

#[cfg(test)]
mod tests;
