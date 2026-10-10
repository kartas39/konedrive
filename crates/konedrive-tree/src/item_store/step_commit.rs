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
//! the item was sent into has no binding any more, the place the item was to take in the
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
//! **A directory's removal** asks again, in its transaction, what the plan asked: the
//! folder shows nothing of what the rows say OneDrive has below the directory
//! ([`ItemStore::can_directory_removal_be_sent`], which a step asks before its request
//! leaves too). Every row it takes the OneDrive version from is stamped with it.
//!
//! **What a commit of a step towards OneDrive writes of OneDrive** goes through the one
//! confirmation ([`confirm_onedrive_version`]): the version, the eTag and the stamp of the
//! guard against an outdated listing, in one statement.
//!
//! **What the steps remember of the item** ([`step_memory`](super::step_memory)) is kept
//! right in the same transaction, where the commit knows more than the rows say: a commit
//! of a step towards OneDrive forgets what was kept for the request OneDrive confirmed
//! ([`ConfirmedRequest`]) — the failed tries, the upload session that sent the content,
//! which ended and is not to be cancelled, the temporary name of a move that reached the
//! place the name led to —, and
//! the commit of the step that hashed the local content carries that content, the same
//! bytes, into the facts that hold it.

use std::ffi::{OsStr, OsString};

use rusqlite::{Connection, Transaction};

use super::bindings::{bind_item, give_up_binding, merge_items};
use super::item_row::{confirm_removal_below, find_bound_item, find_item, is_anything_shown_below_in_onedrive, write_item};
use super::onedrive_versions::confirm_onedrive_version;
use super::step_memory::{carry_content, forget_confirmed_request, ConfirmedRequest};
use super::{check_can_hold, check_directory, check_empty, find_item_at, refuse, ItemStore};
use crate::model::{ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, ItemVersion, LocalItemId, OneDriveItemReference};
use crate::sync_plan::ItemVersions;
use crate::{ConfirmedOneDriveVersion, TreeError};

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
    /// this one. When the rows changed, nothing is written, and the directory OneDrive made
    /// is the step's to remove where the item's row is gone.
    pub fn commit_make_directory(&mut self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr, made: &ConfirmedItem) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_new_item(tx, item, ItemKind::Directory, parent, name, made))
    }

    /// The commit of [`OneDriveStep::SendNewFile`](crate::OneDriveStep::SendNewFile): as for
    /// a directory ([`commit_make_directory`](Self::commit_make_directory)), and the
    /// content OneDrive answered is the content agreed on, and the local content too when
    /// that is still the one that was sent.
    pub fn commit_send_new_file(&mut self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr, made: &ConfirmedItem) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_new_item(tx, item, ItemKind::File, parent, name, made))
    }

    /// The commit of [`OneDriveStep::SendContent`](crate::OneDriveStep::SendContent):
    /// OneDrive has the content that was sent, and answered the item as `sent`. OneDrive's
    /// version is what was answered, its content is the content agreed on, and the local
    /// content too when that is still the one that was sent.
    pub fn commit_send_content(&mut self, item: &ItemVersions, sent: &ConfirmedItem) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_send_content(tx, item, sent))
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
    /// OneDrive removed the file. Its row is left with its binding alone, stamped, for the
    /// guard against an outdated listing.
    pub fn commit_send_file_removal(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_send_removal(tx, item, ItemKind::File, &[]))
    }

    /// The commit of
    /// [`OneDriveStep::SendDirectoryRemoval`](crate::OneDriveStep::SendDirectoryRemoval):
    /// OneDrive removed the directory with all it had in it. The rows of the directory and
    /// of each of `below`, what was removed here with it, are left with their binding
    /// alone, stamped; every other row the rows say OneDrive has in it loses its OneDrive
    /// version, as a listing would say, and is stamped too. Nothing is written unless the
    /// versions of the directory and of every one of `below` are still their rows', and
    /// the folder shows nothing OneDrive has below the directory
    /// ([`can_directory_removal_be_sent`](Self::can_directory_removal_be_sent)).
    pub fn commit_send_directory_removal(&mut self, item: &ItemVersions, below: &[ItemVersions]) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_send_removal(tx, item, ItemKind::Directory, below))
    }

    /// Whether the removal of the directory of `item`, planned with `below` as what was
    /// removed here with it, can be sent as the rows are now: the versions of the directory
    /// and of each of `below` are still their rows', and the folder shows nothing of what
    /// the rows say OneDrive has in the directory and below it. What the step asks under
    /// the lock before its request leaves, and what its commit asks again: a directory's
    /// removal is unguarded in OneDrive and takes all that is in it.
    pub fn can_directory_removal_be_sent(&self, item: &ItemVersions, below: &[ItemVersions]) -> Result<bool, TreeError> {
        is_directory_removal_as_planned(&self.store.conn, item, below)
    }

    /// The commit of [`FolderStep::ShowItem`](crate::FolderStep::ShowItem): the folder
    /// shows the item at OneDrive's place, a file with OneDrive's content, not on disk, and
    /// the two sides agree on both. `is_name_ignored` is what the upload rules say of its
    /// name there.
    pub fn commit_show_item(&mut self, item: &ItemVersions, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_show_item(tx, item, is_name_ignored))
    }

    /// The commit of [`FolderStep::MoveItem`](crate::FolderStep::MoveItem): the folder
    /// shows the item at OneDrive's place, which is the place agreed on. `is_name_ignored`
    /// is what the upload rules say of its name there.
    pub fn commit_move_item(&mut self, item: &ItemVersions, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_move_to_onedrive_place(tx, item, is_name_ignored))
    }

    /// The commit of
    /// [`FolderStep::TakeOneDriveContent`](crate::FolderStep::TakeOneDriveContent): the
    /// file shows OneDrive's content, which is the content agreed on, and its content is
    /// not on disk. The row is written before the copy on disk goes.
    pub fn commit_take_onedrive_content(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_take_onedrive_content(tx, item))
    }

    /// The commit of [`FolderStep::RemoveItem`](crate::FolderStep::RemoveItem): the folder
    /// does not show the item, and nothing is agreed on of it any more. The row of an item
    /// OneDrive removed is then a binding alone; that of an item OneDrive has where the
    /// folder cannot show it is that of an item of OneDrive the folder never showed, so that
    /// nothing reads as removed here and no removal is sent. The rows changed while the
    /// folder shows something in the item, a directory.
    pub fn commit_remove_item(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_remove_item(tx, item))
    }

    /// The commit of [`FolderStep::DownloadContent`](crate::FolderStep::DownloadContent):
    /// the whole content of the file is on disk.
    pub fn commit_download_content(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_download_content(tx, item))
    }

    /// The commit of
    /// [`FolderStep::RenameToConflictCopy`](crate::FolderStep::RenameToConflictCopy): the
    /// folder shows the item under `name` in the directory it is in. `is_name_ignored` is
    /// what the upload rules say of that name. Nothing agreed on changes.
    pub fn commit_rename_to_conflict_copy(&mut self, item: &ItemVersions, name: &OsStr, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_rename_to_conflict_copy(tx, item, name, is_name_ignored))
    }

    /// The commit of [`FolderStep::TakeMoveBack`](crate::FolderStep::TakeMoveBack): the
    /// folder shows the item at OneDrive's place again, which is the place agreed on.
    /// `is_name_ignored` is what the upload rules say of its name there.
    pub fn commit_take_move_back(&mut self, item: &ItemVersions, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_move_to_onedrive_place(tx, item, is_name_ignored))
    }

    /// The commit of
    /// [`FolderStep::MoveToTemporaryName`](crate::FolderStep::MoveToTemporaryName): the
    /// folder shows the item at `place`, which is the place agreed on from now.
    /// `is_name_ignored` is what the upload rules say of its name there.
    pub fn commit_move_to_temporary_name(&mut self, item: &ItemVersions, place: &ItemPlace, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_move_to_temporary_name(tx, item, place, is_name_ignored))
    }

    /// The commit of [`RowStep::GiveUpBinding`](crate::RowStep::GiveUpBinding)
    /// ([`give_up_binding`](Self::give_up_binding)). `is_name_ignored` is what the upload
    /// rules say of the item's name, asked of it from now on.
    pub fn commit_give_up_binding(&mut self, item: &ItemVersions, is_name_ignored: bool) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_give_up_binding(tx, item, is_name_ignored))
    }

    /// The commit of [`RowStep::MergeItems`](crate::RowStep::MergeItems)
    /// ([`merge_items`](Self::merge_items)). Nothing is written unless the versions of both
    /// items are still their rows'.
    pub fn commit_merge_items(&mut self, local: &ItemVersions, onedrive: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_merge_items(tx, local, onedrive))
    }

    /// The commit of [`RowStep::AgreeOnRemoval`](crate::RowStep::AgreeOnRemoval): both
    /// sides removed the item; what they agreed on before is forgotten, and the row is a
    /// binding alone.
    pub fn commit_agree_on_removal(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_forget_synced_version(tx, item, false))
    }

    /// The commit of
    /// [`RowStep::ForgetSyncedVersion`](crate::RowStep::ForgetSyncedVersion): what the two
    /// sides agreed on of an item the folder does not show and OneDrive has is forgotten.
    pub fn commit_forget_synced_version(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_forget_synced_version(tx, item, true))
    }

    /// The commit of [`RowStep::HashLocalContent`](crate::RowStep::HashLocalContent): the
    /// local content, of the size and the time it had, is told by `hash`, which was
    /// computed from the bytes on disk.
    pub fn commit_hash_local_content(&mut self, item: &ItemVersions, hash: &str) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_hash_local_content(tx, item, hash))
    }

    /// The commit of [`RowStep::AgreeOnPlace`](crate::RowStep::AgreeOnPlace): the place both
    /// sides have the item at is the place agreed on.
    pub fn commit_agree_on_place(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_agree_on_place(tx, item))
    }

    /// The commit of [`RowStep::AgreeOnContent`](crate::RowStep::AgreeOnContent): OneDrive's
    /// content version is the local one and the one agreed on.
    pub fn commit_agree_on_content(&mut self, item: &ItemVersions) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| commit_agree_on_content(tx, item))
    }
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
/// ([`ItemStore::can_directory_removal_be_sent`]).
fn is_directory_removal_as_planned(conn: &Connection, item: &ItemVersions, below: &[ItemVersions]) -> Result<bool, TreeError> {
    for planned in std::iter::once(item).chain(below) {
        if find_planned(conn, planned)?.is_none() {
            return Ok(false);
        }
    }
    Ok(!is_anything_shown_below_in_onedrive(conn, item.local_item_id)?)
}

/// The directory that has the binding `directory`, when a row has it.
fn find_bound_directory(conn: &Connection, directory: &OneDriveItemReference) -> Result<Option<LocalItemId>, TreeError> {
    Ok(find_bound_item(conn, directory)?.filter(|row| row.kind == ItemKind::Directory).map(|row| row.local_item_id))
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
pub(crate) fn commit_new_item(tx: &Transaction<'_>, item: &ItemVersions, kind: ItemKind, parent: &OneDriveItemReference, name: &OsStr, made: &ConfirmedItem) -> Result<StepCommitOutcome, TreeError> {
    let id = item.local_item_id;
    let Some(row) = find_planned_new_item(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    if row.kind != kind || row.onedrive_item.is_some() || row.local.is_none() {
        return Err(refuse_step(id, "it is of another kind, has a binding, or the folder does not show it"));
    }
    let (Some(sent_directory), Some(_)) = (find_bound_directory(tx, parent)?, find_bound_directory(tx, &made.parent)?) else { return Ok(StepCommitOutcome::RowsChanged) };
    // A listing brought the new item, and the folder shows it as an item of its own.
    if find_bound_item(tx, &made.item)?.is_some_and(|holder| holder.local.is_some()) {
        return Ok(StepCommitOutcome::RowsChanged);
    }
    // Before the binding is given: when a row a listing made is merged into this one, a
    // binding moves, and a session still kept then is taken for one to cancel.
    let sent = item.local.as_ref().and_then(|local| local.content.as_ref());
    forget_confirmed_request(tx, id, ConfirmedRequest::Upload { sent })?;
    bind_item(tx, id, &made.item)?;
    if !confirm_item(tx, id, made)? {
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
pub(crate) fn commit_send_content(tx: &Transaction<'_>, item: &ItemVersions, sent: &ConfirmedItem) -> Result<StepCommitOutcome, TreeError> {
    let Some(id) = find_planned_item_of_onedrive(tx, item, sent)?.map(|row| row.local_item_id) else { return Ok(StepCommitOutcome::RowsChanged) };
    if !confirm_item(tx, id, sent)? {
        return Ok(StepCommitOutcome::RowsChanged);
    }
    let mut row = read_row(tx, id)?;
    let sent_content = item.local.as_ref().and_then(|local| local.content.as_ref());
    agree_on_confirmed_content(&mut row, sent_content, sent.content.as_ref());
    write_item(tx, &row)?;
    forget_confirmed_request(tx, id, ConfirmedRequest::Upload { sent: sent_content })?;
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

/// [`ItemStore::commit_send_file_removal`] and [`ItemStore::commit_send_directory_removal`],
/// inside the caller's transaction, which is dropped on any error. The removal is confirmed
/// of the item; then all the rows say OneDrive has below a directory, those of `below`
/// among them, lose their OneDrive version and what was agreed on, under the same stamp.
pub(crate) fn commit_send_removal(tx: &Transaction<'_>, item: &ItemVersions, kind: ItemKind, below: &[ItemVersions]) -> Result<StepCommitOutcome, TreeError> {
    let id = item.local_item_id;
    let Some(row) = find_planned(tx, item)? else { return Ok(StepCommitOutcome::RowsChanged) };
    if row.kind != kind || row.local.is_some() || row.onedrive_item.is_none() {
        return Err(refuse_step(id, "it is of another kind, the folder shows it, or it has no binding"));
    }
    for removed_with_it in below {
        let Some(row) = find_planned(tx, removed_with_it)? else { return Ok(StepCommitOutcome::RowsChanged) };
        if row.local.is_some() || row.onedrive_item.is_none() {
            return Err(refuse_step(row.local_item_id, "the folder shows it, or it has no binding: it is not removed with a directory"));
        }
    }
    if kind == ItemKind::Directory && is_anything_shown_below_in_onedrive(tx, id)? {
        return Ok(StepCommitOutcome::RowsChanged);
    }
    let count = confirm_onedrive_version(tx, id, &ConfirmedOneDriveVersion::Removed)?;
    if kind == ItemKind::Directory {
        confirm_removal_below(tx, id, count)?;
    }
    for removed in std::iter::once(item).chain(below) {
        forget_confirmed_request(tx, removed.local_item_id, ConfirmedRequest::Removal)?;
    }
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

/// OneDrive's content, `content`, is the local content of `row` and the content agreed on.
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
