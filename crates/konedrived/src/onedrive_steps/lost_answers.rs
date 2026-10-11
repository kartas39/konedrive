//! What became of a request whose answer was lost, or refused in a way its own earlier
//! try explains: design §7's second exception. The step adopts what it finds.
//!
//! **State owned:** none. The record of a request that changes OneDrive is the store's
//! (`konedrive_tree::RequestRecord`, the table `request_records`): written by the step
//! BEFORE its request leaves, and gone when the answer was committed, when OneDrive
//! refused the request for certain, or when what became of it was read here. While a
//! record stands, its item is busy to the plan and the directory it was sent into is a
//! destination no removal is sent for: the plan's input takes both from the running steps
//! AND from the records (`sync_plan_task::plan_input::describe_folder`,
//! `ItemReader::list_request_records`), and so does the question a directory's removal
//! asks before it leaves.
//!
//! **Who settles a record.** The step itself, in the run that wrote it: after an answer
//! that says nothing certain it reads OneDrive once
//! ([`LostAnswers::read_request_outcome`]) and goes on as the fate says; when that read
//! says nothing certain either, the step ends `StepRefusal::Transient` and the record
//! stands. A record that stands — left so, or left by a stop or a kill of the daemon
//! between a request and its commit — is settled by
//! [`LostAnswers::settle_lost_answer`], asked for each record by the plan's task with what
//! else is left to clean in OneDrive (`SessionCancellations::settle_request_records`):
//! after a start as soon as sending is not held, which is after the first listing ended,
//! and before any step of that item or into that directory is planned, since the record
//! keeps both busy until then.
//!
//! **What is on record.** Every request of one answer: a directory made, a move, a
//! removal, and an empty file sent (`upload_empty`). An upload through a session writes
//! none: its record is the opening and the session the step memory keeps, and an answer
//! lost to the opening or to a fragment, the last too, is `Transient` at once
//! (`content_upload`); the file and the directory it is sent into are then not busy, and
//! nothing is lost by that: a file removed here meanwhile may come back once from
//! OneDrive, and a removal of the directory takes only the upload's own file.
//!
//! **Answers for:** a daemon that died, or was stopped, between a request and its commit,
//! and an answer that never came while it runs (the map's F9, E2). Today:
//! `upload/steps/meta.rs:33-36`, `:100-104`,
//! `upload/content.rs:211-215`, `:374`, `:400-403`, `upload/content/session.rs:397`.
//!
//! **Tested without the rest**, a test per case of [`SentRequest`] and of
//! [`LostAnswerFate`], then against the fixture's fake OneDrive:
// test: a_directory_of_the_name_that_no_row_is_bound_to_is_the_one_this_step_made
// test: a_directory_of_the_name_that_a_row_is_bound_to_is_another_item_with_its_name
// test: a_directory_of_the_name_that_is_not_empty_is_another_item
// test: a_file_of_the_name_with_the_hash_sent_that_no_row_is_bound_to_is_adopted
// test: an_item_already_at_the_place_sent_is_the_move_that_went_through
// test: an_item_that_holds_the_hash_sent_is_the_upload_that_went_through
// test: an_item_that_is_gone_is_the_removal_that_went_through
// test: an_item_with_the_etag_planned_from_is_a_request_that_did_not_go_through
// test: an_item_guarded_by_its_ctag_is_as_planned_while_it_has_that_ctag
// test: an_item_that_was_moved_or_changed_by_another_is_not_adopted
// test: a_record_whose_request_went_through_is_committed_as_its_step_would_and_goes
// test: a_record_whose_request_did_not_go_through_goes_and_writes_no_row
// test: a_record_whose_item_cannot_be_read_now_stands
// test: a_record_whose_row_went_or_lost_its_binding_goes

use std::ffi::{OsStr, OsString};
use std::sync::Arc;

use konedrive_graph::drive::{DriveError, DriveItem};
use konedrive_graph::pool::Class;
use konedrive_graph::quickxor::QuickXor;
use konedrive_tree::{ConfirmedItem, DriveId, ItemKind, ItemVersions, OneDriveItemId, OneDriveItemReference, RecordedRequest, RequestRecord, StepCommitOutcome, TreeError, TreeStore};

use super::confirmed_item::{decide_guard, to_confirmed_item, to_graph_name};
use super::graph_refusal::decide_read_refusal;
use super::GraphAccess;
use crate::sync_parts::step_outcome::{StepFailure, StepOutcome, StepRefusal};

/// A request that changes OneDrive, by what it asked: what is compared with OneDrive to
/// tell whether it went through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentRequest<'a> {
    /// A directory made in `parent` under `name`. Read: `child` of that place.
    MakeDirectory { parent: &'a OneDriveItemReference, name: &'a OsStr },
    /// A file with the content of `sent_hash` made in `parent` under `name`. Read:
    /// `child` of that place.
    SendNewFile { parent: &'a OneDriveItemReference, name: &'a OsStr, sent_hash: &'a str },
    /// The content of `sent_hash` sent into the item. Read: `item`.
    SendContent { sent_hash: &'a str },
    /// The item sent into `parent` under `name`. Read: `item`.
    SendMove { parent: &'a OneDriveItemReference, name: &'a OsStr },
    /// The removal of the item, a file or a directory. Read: `item`.
    SendRemoval,
}

impl<'a> SentRequest<'a> {
    /// The request `recorded` says was sent.
    pub fn of_record(recorded: &'a RecordedRequest) -> Self {
        match recorded {
            RecordedRequest::MakeDirectory { parent, name } => SentRequest::MakeDirectory { parent, name },
            RecordedRequest::SendNewFile { parent, name, sent_hash } => SentRequest::SendNewFile { parent, name, sent_hash },
            RecordedRequest::SendContent { sent_hash } => SentRequest::SendContent { sent_hash },
            RecordedRequest::SendMove { parent, name } => SentRequest::SendMove { parent, name },
            RecordedRequest::SendFileRemoval | RecordedRequest::SendDirectoryRemoval => SentRequest::SendRemoval,
        }
    }
}

/// What became of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LostAnswerFate {
    /// OneDrive has what the request asked: the item as it is now, for the step's commit;
    /// none for a removal, whose item is gone.
    WentThrough(Option<ConfirmedItem>),
    /// OneDrive has the item as the step was planned from it — what the request was
    /// guarded with (`confirmed_item::decide_guard`): the eTag planned from, or, for an
    /// item guarded by the cTag of its synced content, that cTag —, or
    /// nothing at the place a new item was sent to: the request changed nothing.
    DidNotGoThrough,
    /// OneDrive has the item neither as planned nor as asked, or does not have an item
    /// that was to be moved or filled: another change, which a listing brings.
    ItemChanged,
    /// Another item holds the place a new item was sent to — one a row is bound to, of
    /// another kind or content, or a directory that is not empty —, spelt so by OneDrive.
    NameHeld { holder_name: OsString },
}

/// Whether `found` is the item as the step was planned from it: it has what the request
/// was guarded with. Never for an item with no guard.
fn is_as_planned(item: &ItemVersions, found: &DriveItem) -> bool {
    match (&item.onedrive_etag, decide_guard(item)) {
        (Some(etag), _) => found.e_tag.as_deref() == Some(etag.as_str()),
        (None, Some(ctag)) => found.c_tag.as_deref() == Some(ctag.as_str()),
        (None, None) => false,
    }
}

/// Whether `found` holds the content of `sent_hash`; an empty file with no hash holds the
/// empty content.
fn has_sent_content(found: &DriveItem, sent_hash: &str) -> bool {
    found.file.is_some() && (found.quick_xor_hash() == Some(sent_hash) || (found.quick_xor_hash().is_none() && found.size == Some(0) && sent_hash == QuickXor::new().finish_base64()))
}

/// What became of `request` about `item`, from what OneDrive has now at the place the
/// request names (`found`: the item read, none when OneDrive has none there) and whether
/// a row is bound to what was found. Pure. A new item is the step's own only when no row
/// is bound to it, and a directory only when it is empty besides (`DriveItem::folder`'s
/// `childCount` is 0; a count that is not given counts as not empty): the step's own
/// request made an empty directory, and one with something in it is another client's of
/// the same name that no listing brought yet, whose content no row knows — adopted, an
/// unguarded removal would take it. Such a directory is `NameHeld`, and the plan merges
/// the two rows once a listing brought it with what is in it. A move went through when the item is in `parent` under `name` as
/// Graph spells both; a content when the item's hash is `sent_hash`. An answer that
/// cannot be read as an item (`confirmed_item::to_confirmed_item`) is the failure.
pub fn decide_lost_answer_fate(request: &SentRequest<'_>, item: &ItemVersions, found: Option<&DriveItem>, is_found_bound: bool, drive_id: &DriveId) -> Result<LostAnswerFate, StepFailure> {
    let went_through = |found: &DriveItem| Ok(LostAnswerFate::WentThrough(Some(to_confirmed_item(found, drive_id)?)));
    let name_held = |found: &DriveItem, sent_name: &OsStr| LostAnswerFate::NameHeld { holder_name: found.name.clone().map_or_else(|| sent_name.to_owned(), OsString::from) };
    match (request, found) {
        (SentRequest::MakeDirectory { .. } | SentRequest::SendNewFile { .. }, None) => Ok(LostAnswerFate::DidNotGoThrough),
        (SentRequest::MakeDirectory { name, .. }, Some(found)) => {
            let is_empty_directory = found.folder.as_ref().and_then(|folder| folder.get("childCount")).and_then(serde_json::Value::as_u64) == Some(0);
            match is_empty_directory && !is_found_bound {
                true => went_through(found),
                false => Ok(name_held(found, name)),
            }
        }
        (SentRequest::SendNewFile { name, sent_hash, .. }, Some(found)) => match has_sent_content(found, sent_hash) && !is_found_bound {
            true => went_through(found),
            false => Ok(name_held(found, name)),
        },
        (SentRequest::SendRemoval, None) => Ok(LostAnswerFate::WentThrough(None)),
        (SentRequest::SendContent { .. } | SentRequest::SendMove { .. }, None) => Ok(LostAnswerFate::ItemChanged),
        (SentRequest::SendContent { sent_hash }, Some(found)) if has_sent_content(found, sent_hash) => went_through(found),
        (SentRequest::SendMove { parent, name }, Some(found))
            if found.parent_reference.as_ref().and_then(|at| at.id.as_deref()).is_some_and(|id| id.eq_ignore_ascii_case(parent.item_id.as_str())) && found.name.as_deref().map(OsStr::new) == Some(*name) =>
        {
            went_through(found)
        }
        (SentRequest::SendContent { .. } | SentRequest::SendMove { .. } | SentRequest::SendRemoval, Some(found)) => match is_as_planned(item, found) {
            true => Ok(LostAnswerFate::DidNotGoThrough),
            false => Ok(LostAnswerFate::ItemChanged),
        },
    }
}

/// Commits the answer of the request `record` says was sent, which went through, as its
/// step would have: `versions` are those of the row as it is now, `confirmed` the item as
/// OneDrive has it (none for a removal). In the caller's job of the store, under the
/// tree lock. A file is confirmed with no session: a request on record sent an empty
/// file by one request.
fn commit_settled_request(store: &mut TreeStore, record: &RequestRecord, versions: &ItemVersions, confirmed: Option<&ConfirmedItem>, now: i64) -> Result<StepCommitOutcome, TreeError> {
    match (&record.request, confirmed) {
        (RecordedRequest::MakeDirectory { parent, name }, Some(made)) => store.item_store().commit_make_directory(versions, parent, name, made),
        (RecordedRequest::SendNewFile { parent, name, .. }, Some(made)) => store.item_store().commit_send_new_file(versions, parent, name, made, None),
        (RecordedRequest::SendContent { .. }, Some(sent)) => store.item_store().commit_send_content(versions, sent, None),
        (RecordedRequest::SendMove { parent, name }, Some(moved)) => store.item_store().commit_send_move(versions, parent, name, moved),
        (RecordedRequest::SendFileRemoval, None) => store.item_store().commit_send_file_removal(versions, now),
        (RecordedRequest::SendDirectoryRemoval, None) => {
            // All the rows say OneDrive has below the directory, as they are now: the commit asks that it is what was removed here.
            let below: Vec<ItemVersions> = store.item_store().list_below_in_onedrive(versions.local_item_id)?.iter().map(ItemVersions::of).collect();
            store.item_store().commit_send_directory_removal(versions, &below, now)
        }
        // A fate that does not fit its request writes nothing.
        (RecordedRequest::MakeDirectory { .. } | RecordedRequest::SendNewFile { .. } | RecordedRequest::SendContent { .. } | RecordedRequest::SendMove { .. }, None) | (RecordedRequest::SendFileRemoval | RecordedRequest::SendDirectoryRemoval, Some(_)) => {
            Ok(StepCommitOutcome::RowsChanged)
        }
    }
}

/// What reads OneDrive for what became of a step's request.
#[derive(Clone)]
pub struct LostAnswers {
    graph: GraphAccess,
    /// Held across the commit of a record whose request went through, and nothing else.
    tree_lock: Arc<tokio::sync::Mutex<()>>,
}

impl LostAnswers {
    /// Made by the folder's start, with the folder's tree lock.
    pub fn new(graph: GraphAccess, tree_lock: Arc<tokio::sync::Mutex<()>>) -> Self {
        Self { graph, tree_lock }
    }

    /// What OneDrive has now where `request` names: one read, with a `Class::Metadata`
    /// slot of the pool for itself. `Ok(None)` when nothing is there.
    async fn read_requested_item(&self, item: &ItemVersions, request: &SentRequest<'_>) -> Result<Option<DriveItem>, StepOutcome> {
        let drive = &self.graph.drive;
        let _slot = drive.pool().acquire(Class::Metadata).await;
        let read = match request {
            SentRequest::MakeDirectory { parent, name } | SentRequest::SendNewFile { parent, name, .. } => {
                let name = to_graph_name(name).map_err(StepOutcome::Refused)?;
                drive.child(parent.item_id.as_str(), name).await
            }
            SentRequest::SendContent { .. } | SentRequest::SendMove { .. } | SentRequest::SendRemoval => {
                let Some(onedrive_item) = &item.onedrive_item else {
                    return Err(StepOutcome::Failed(StepFailure::AnswerUnreadable { detail: format!("item {} has no item of OneDrive to read", item.local_item_id) }));
                };
                drive.item(onedrive_item.item_id.as_str()).await
            }
        };
        match read {
            Ok(found) => Ok(Some(found)),
            Err(DriveError::NotFound) => Ok(None),
            Err(error) => Err(StepOutcome::Refused(decide_read_refusal(error))),
        }
    }

    /// What became of `request`, by ONE read of OneDrive (`item`, `G/mod.rs:217`, or
    /// `child`, `G/mod.rs:228`, as the case of [`SentRequest`] says), whether a row is
    /// bound to what was found (`ItemReader::find_bound_item`), and
    /// [`decide_lost_answer_fate`]. `DriveError::NotFound` is no failed read: it is
    /// `found: None`. Any other `DriveError` is the step's outcome, read through
    /// `WriteError::from` (`graph_refusal::decide_read_refusal`: a lost answer of a read
    /// is `StepRefusal::Transient`). Each read takes a `Class::Metadata` slot of the pool
    /// for itself and lets it go with its answer.
    ///
    /// Called for a request that was certainly refused and whose refusal an earlier try
    /// of the same step may explain — a 409 of `MakeDirectory`
    /// (`upload/steps/meta.rs:33-36`), a 412 of `SendMove` (`meta.rs:100-104`) and of
    /// `SendContent` (`upload/content.rs:400-403`), an upload session found gone —, and
    /// for a request whose answer said nothing certain.
    pub async fn read_request_outcome(&self, item: &ItemVersions, request: SentRequest<'_>) -> Result<LostAnswerFate, StepOutcome> {
        let found = self.read_requested_item(item, &request).await?;
        let is_found_bound = match &found {
            Some(found) => {
                let reference = OneDriveItemReference { drive_id: self.graph.drive_id.clone(), item_id: OneDriveItemId::new(found.id.clone()) };
                self.graph.store.read_items(move |items| Ok(items.find_bound_item(&reference)?.is_some())).await.map_err(|error| StepOutcome::Failed(StepFailure::Store { detail: error.to_string() }))?
            }
            None => false,
        };
        decide_lost_answer_fate(&request, item, found.as_ref(), is_found_bound, &self.graph.drive_id).map_err(StepOutcome::Failed)
    }

    /// Settles `record`, a request that left and whose answer was never committed: reads
    /// what became of it as [`read_request_outcome`](Self::read_request_outcome) does,
    /// with the versions of the row as it is now — the row of the record's item, or, for
    /// a request about an item of OneDrive, the row that has that binding now.
    /// `WentThrough`: the answer is committed as the step would have, under the tree
    /// lock, in one job of the store with the record's end; a commit that finds the rows
    /// changed, or that the store refuses, writes nothing, and the record goes all the
    /// same: a listing brings what OneDrive has. Any other fate: the record goes, and the
    /// next plan sends what is unsent. A record whose row went, or whose item of
    /// OneDrive no row is bound to any more, goes with nothing read.
    ///
    /// `Err` when nothing certain could be read — the network, a throttle, a sign-out, a
    /// read OneDrive refuses: the record stands, and with it its item and its destination
    /// stay busy; the refusal says why, and when OneDrive named a time. Nothing is
    /// marked on disk and no event is answered for a record committed here: the marks
    /// serve the check at start, which reads the rows.
    // TODO: a made directory and an empty file whose record is committed here get no mark of their item id on disk (`ItemContentFiles::mark_made_directory`, `mark_sent_file` are the runner's); check whether the check at start needs them and fix if needed.
    pub async fn settle_lost_answer(&self, record: &RequestRecord) -> Result<(), StepRefusal> {
        let store_failure = |error: TreeError| StepRefusal::Transient { detail: format!("the store: {error}") };
        let (item, onedrive_item) = (record.item, record.onedrive_item.clone());
        let row = self
            .graph
            .store
            .read_items(move |items| match &onedrive_item {
                Some(onedrive_item) => items.find_bound_item(onedrive_item),
                None => items.find_item(item),
            })
            .await
            .map_err(store_failure)?;
        let forget = move |store: &mut TreeStore| store.step_memory().forget_request_record(item);
        let Some(row) = row else {
            return self.graph.store.call(forget).await.map_err(store_failure);
        };
        let versions = ItemVersions::of(&row);
        let fate = match self.read_request_outcome(&versions, SentRequest::of_record(&record.request)).await {
            Ok(fate) => fate,
            Err(StepOutcome::Refused(refusal)) => return Err(refusal),
            Err(outcome) => return Err(StepRefusal::Transient { detail: format!("{outcome:?}") }),
        };
        let LostAnswerFate::WentThrough(confirmed) = fate else {
            return self.graph.store.call(forget).await.map_err(store_failure);
        };
        // A file and a directory are told apart by the request; a kind that is not the row's writes nothing.
        let is_of_kind = match &record.request {
            RecordedRequest::MakeDirectory { .. } | RecordedRequest::SendDirectoryRemoval => row.kind == ItemKind::Directory,
            RecordedRequest::SendNewFile { .. } | RecordedRequest::SendContent { .. } | RecordedRequest::SendFileRemoval => row.kind == ItemKind::File,
            RecordedRequest::SendMove { .. } => true,
        };
        let (record, now) = (record.clone(), (self.graph.clock)());
        let _tree = self.tree_lock.lock().await;
        self.graph
            .store
            .call(move |store| {
                if is_of_kind {
                    match commit_settled_request(store, &record, &versions, confirmed.as_ref(), now) {
                        Ok(StepCommitOutcome::Written) => tracing::info!("the request of item {} whose answer was lost went through: its answer is written", record.item),
                        Ok(StepCommitOutcome::RowsChanged) => {}
                        Err(error) => tracing::warn!("the answer of the request of item {} that went through is not written: {error}", record.item),
                    }
                }
                store.step_memory().forget_request_record(record.item)
            })
            .await
            .map_err(store_failure)
    }
}
