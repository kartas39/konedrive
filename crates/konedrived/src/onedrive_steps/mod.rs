//! The runner of the sync plan's steps towards OneDrive: one function per case of
//! `OneDriveStep`, each one request to Graph (an upload is one request in fragments),
//! the answer turned into what its commit takes, and the commit.
//!
//! **State owned:** none. [`OneDriveSteps`] holds the parts a step goes through — the
//! content upload, the holders of names, the lost answers, the made items — each of which
//! has what it needs and no more, and the three things only the runner uses: the tree
//! lock, the local contents and the clock. What a step keeps between tries is the step
//! memory's (`konedrive_tree::StepMemory`); which steps run is `RunningSteps`'.
//!
//! **Every step, in this order** (design §7):
//!
//! 1. takes what it sends from the versions it was planned from, never from the row as it
//!    is when the request leaves: the place sent is the step's `parent` and `name`, the
//!    guard is `item.onedrive_etag`, else the cTag of `item.synced_content`
//!    (`confirmed_item::decide_guard`), the content is `item.local.content`. A name is
//!    given to Graph through `confirmed_item::to_graph_name`. What it reads of the store
//!    before the request is one read (`Store::read_items`): the item's memory
//!    (`ItemReader::find_item_memory`), and what its case says;
//! 2. writes into the step memory what must outlive a crash BEFORE the request that makes
//!    it true: the record of the request itself, for every request of one answer
//!    (`StepMemory::remember_request_record`: a directory made, a move, a removal, an
//!    empty file; it goes with the commit, with a refusal that is certain and with the
//!    fate read), the opening of an upload session, the session before its first byte,
//!    and a temporary name — for `MakeDirectory`, `SendNewFile` and `SendMove` alike, whenever
//!    `name` is not the name of the place of `item.local`:
//!    `remember_temporary_name(item, TemporaryName { name, toward })`, `toward` that
//!    local place (today `temporary`, `upload/steps/shared.rs:333`);
//! 3. sends the request, with no lock held, and with a slot of the account's pool
//!    (`DriveClient::pool`): `Class::Metadata` taken here for a step that sends no
//!    content, held for its requests and let go before any wait; the upload's by
//!    `ContentUpload`. The plan's task takes none;
//! 4. a refusal is read (`graph_refusal::decide_refusal`); a taken name, a changed item
//!    and a missing item are looked at once more only for the two causes no listing will
//!    ever show — the empty file of an upload session, and its own earlier request whose
//!    answer was lost (`name_holders`, `lost_answers`) — and otherwise decide nothing. A
//!    507 of a step that sends no content is `OneDriveSpace::refuse_for_space` with a
//!    size of 0; its settlement counts a failed try (`Reason::Quota`) and keeps no
//!    refusal for lack of space, which is a content's: the step waits and is tried again. An answer that says nothing certain (`RefusedRequest::AnswerLost`, and
//!    an answer of success that cannot be read) is read ONCE for what became of the
//!    request (`LostAnswers::read_request_outcome`), and the step goes on as the fate
//!    says — `WentThrough`: the commit; `DidNotGoThrough`:
//!    `StepRefusal::Transient`; `ItemChanged`: `StepRefusal::ItemChanged`; `NameHeld`:
//!    `StepRefusal::NameTaken`. When that read says nothing certain either, the step
//!    ends `StepRefusal::Transient` with its record standing (point 2), and the record
//!    keeps the item and the directory it was sent into busy to the plan until it is
//!    settled (`lost_answers`);
//! 5. turns Graph's `driveItem` into a `ConfirmedItem` (`confirmed_item`): the id, the
//!    parent's reference, the name Graph stored, the eTag, a file's content;
//! 6. says its commit begins (`RunningSteps::begin_commit`: a stop aborts the step's task
//!    no more; when the answer is that the step runs no more, it was aborted, and the
//!    runner returns `StepOutcome::NotRun(StepObstacle::SendingStopped)` with nothing
//!    written, which nobody settles); takes the folder's tree lock, calls its
//!    commit of `step_commit` in one job of the store (`Store::call`), and lets the lock
//!    go; the record of the request goes in that job. The commit itself forgets what the step memory kept for the confirmed request:
//!    the session that sent the content, the tries, a temporary name that led to the
//!    confirmed place. A commit the store refuses or fails is `StepFailure::Store`;
//! 7. with NO lock: on `Written` marks what it sent on disk — a mark that fails is
//!    logged and the step is written all the same — and answers the event; on
//!    `RowsChanged` asks Graph again where it must (`made_items`: a made item whose row
//!    is gone is removed in OneDrive), and everything else asks for a listing cycle.
//!
//! **The event of a written step** (`StepOutcome::Written { events }`), one each, its
//! path read in the job of the commit (`ItemReader::find_path`, relative to the folder):
//! `MakeDirectory`, `SendNewFile` and `SendContent` answer `ActivityKind::Uploaded` at the
//! item's path after the commit; `SendMove` `ActivityKind::CloudMoved` at the path after
//! the commit, `former_path` the path the two sides agreed on before it
//! (`ItemReader::find_synced_path`, read before the commit); the two removals
//! `ActivityKind::CloudDeleted` at the path agreed on before the commit, one event for a
//! directory with all that is in it.
//!
//! The failed tries, a refusal for good, a taken name and the wait they mean are written
//! by the plan's task from the answer (`sync_plan_task::step_settlement`), not
//! here: one place counts tries.
//!
//! The requests are the upload worker's (design §11): each function names the function
//! of today's worker it takes the request and its handling from, and the call of the
//! Graph client under it (`crates/konedrive-graph/src/drive/`, here `G/`).
//!
//! **Tested without the rest**, over the fixture (`mount::item_fixture`: its store, its
//! backing directory, its fake OneDrive) with `ItemContentFiles` made over it; today's
//! upload tests are the reference for every scenario (`upload/tests/`), and the races'
//! test of design §12 puts a death at every point between a request and its commit:
// test: each_case_sends_the_request_of_its_planned_versions_and_commits_the_answer
// test: a_request_is_guarded_by_the_etag_the_step_was_planned_from_else_the_synced_ctag
// test: a_temporary_name_is_remembered_before_the_request_of_a_new_item_and_of_a_move
// test: a_commit_that_finds_the_rows_changed_after_a_new_item_was_made_removes_it_when_the_row_is_gone
// test: a_commit_that_finds_the_rows_changed_otherwise_asks_for_a_cycle
// test: a_step_replayed_after_a_death_between_its_request_and_its_commit_adopts_what_it_finds
// test: a_request_is_on_record_before_it_leaves_and_the_record_goes_with_its_commit_or_its_certain_refusal
// test: a_step_whose_answer_was_lost_reads_onedrive_once_and_commits_what_went_through
// test: a_step_whose_answer_was_lost_and_whose_read_failed_ends_transient_with_its_record_standing
// test: a_directorys_removal_is_not_sent_while_a_record_names_an_item_below_it_or_a_destination_in_it
// test: a_refusal_is_answered_as_its_case_and_writes_no_row
// test: no_request_is_sent_while_the_tree_lock_is_held
// test: a_step_whose_commit_may_not_begin_writes_nothing
// test: each_written_step_answers_its_event_with_the_path_of_its_commit
// test: a_step_that_sends_no_content_holds_a_metadata_slot_for_its_request
// TODO: a measurement: a 412 is answered `ItemChanged` and waits for a listing to write the item's eTag (`SendContent`, `SendMove`, `SendFileRemoval`). Today's worker reads the item in place, because an eTag moves in OneDrive with no change of content (`upload/content.rs:139`); whether a delta names the item then is not measured. If it does not, the step is refused at every try: measure once on the test account, and if so let the 412 ask for the item to be read again.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_graph::drive::{DriveError, ItemChange, WriteError};
use konedrive_graph::pool::Class;
use konedrive_tree::{ActivityKind, ConfirmedItem, ItemPlace, ItemVersions, LocalItemId, OneDriveItemReference, OneDriveStep, RecordedRequest, RequestRecord, StepCommitOutcome, Store, TemporaryName, TreeError, TreeStore};

use crate::mount::{ItemContentFiles, LocalContentFile};
use crate::sync_parts::onedrive_space::OneDriveSpace;
use crate::sync_parts::running_steps::RunningSteps;
use crate::sync_parts::step_outcome::{RowsChangedFollowUp, StepEvent, StepFailure, StepObstacle, StepOutcome, StepRefusal, StepTrouble};

/// Graph's answer to a step as its commit takes it, and the guard of a request.
pub mod confirmed_item;
/// A file's content sent: empty at once, otherwise through an upload session.
pub mod content_upload;
/// A refusal of Graph, by what a step does about it.
pub mod graph_refusal;
/// A step's own earlier request whose answer was lost.
pub mod lost_answers;
/// What a step made in OneDrive that its commit did not write.
pub mod made_items;
/// Who holds a name OneDrive answered is taken.
pub mod name_holders;
/// What is left to clean in OneDrive: sessions to cancel, items uploads left, requests
/// whose answer was never committed.
pub mod session_cancellations;

use confirmed_item::{decide_guard, to_confirmed_item, to_graph_name, RequestGuard};
use content_upload::{hash_content, ContentTarget, ContentUpload, NewFilePlace, SendContentError, SentContent};
use graph_refusal::{decide_plain_refusal, decide_read_refusal, decide_refusal, RefusedRequest};
use lost_answers::{FailedRead, LostAnswerFate, LostAnswers, SentRequest};
use made_items::MadeItems;
use name_holders::{NameHolder, NameHolders};

/// What every part that asks Graph is made with.
#[derive(Clone)]
pub struct GraphAccess {
    pub drive: konedrive_graph::drive::DriveClient,
    /// The folder's drive: what an answer's item and parent are referenced in. The Graph
    /// client asks by item id in the account's own drive, and nothing of another drive is
    /// ever shown (a shared folder; the map's RISK-1.r), so every reference a step meets
    /// is of this drive; a step whose reference names another is answered
    /// `StepFailure::AnswerUnreadable`, and nothing is sent.
    pub drive_id: konedrive_tree::DriveId,
    pub store: Store,
    /// Unix seconds: the account's clock, which the tests hold.
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

/// What the job of a commit answers: the commit's outcome, and the item's paths for the
/// event, relative to the folder.
struct CommittedStep {
    outcome: StepCommitOutcome,
    /// The path the two sides agreed on before the commit, else the one the folder showed.
    path_before: Option<PathBuf>,
    /// The path the folder shows after the commit.
    path_after: Option<PathBuf>,
}

/// What a request that was refused, or whose answer was lost, leaves the step: the item
/// to commit (none for a removal that went through), or the step's outcome.
type RequestAnswer = Result<Option<ConfirmedItem>, StepOutcome>;

fn fail_store(error: TreeError) -> StepOutcome {
    StepOutcome::Failed(StepFailure::Store { detail: error.to_string() })
}

fn to_outcome(trouble: StepTrouble) -> StepOutcome {
    match trouble {
        StepTrouble::Obstacle(obstacle) => StepOutcome::NotRun(obstacle),
        StepTrouble::Failure(failure) => StepOutcome::Failed(failure),
    }
}

/// The one event of a written step.
fn describe_written(kind: ActivityKind, path: Option<PathBuf>, former_path: Option<PathBuf>) -> StepOutcome {
    StepOutcome::Written { events: vec![StepEvent { kind, path: path.unwrap_or_default(), former_path }] }
}

/// The place the step memory names for what `item` sends under `name`: the directory of
/// the place of `item.local`, under `name`.
fn find_sent_place(item: &ItemVersions, name: &OsStr) -> Option<ItemPlace> {
    let local_place = item.local.as_ref()?.place.as_ref()?;
    Some(ItemPlace { parent_local_item_id: local_place.parent_local_item_id, name: name.to_owned() })
}

/// The refusal for a name another item holds at the place `item` was sent to.
fn describe_name_taken(item: &ItemVersions, name: &OsStr, holder_name: std::ffi::OsString) -> StepOutcome {
    match find_sent_place(item, name) {
        Some(place) => StepOutcome::Refused(StepRefusal::NameTaken { place, holder_name }),
        None => StepOutcome::Refused(StepRefusal::Transient { detail: format!("the name of item {} is taken in OneDrive, and the item has no local place", item.local_item_id) }),
    }
}

/// The runner of the steps towards OneDrive, for one folder.
pub struct OneDriveSteps {
    graph: GraphAccess,
    /// Held across each commit and nothing else (design §7: "its commit, under the
    /// lock"): never across a request, a hash or a sync of a file.
    tree_lock: Arc<tokio::sync::Mutex<()>>,
    content_files: Arc<ItemContentFiles>,
    content_upload: ContentUpload,
    name_holders: NameHolders,
    lost_answers: LostAnswers,
    made_items: MadeItems,
    /// Told when a step's commit begins: from then on a stop does not abort the step.
    /// Read for the items and the destinations of the running steps before a directory's
    /// removal leaves.
    running_steps: Arc<RunningSteps>,
    /// Asked about a 507 of a step that sends no content, and told the bytes of each
    /// upload that was committed (`OneDriveSpace::count_uploaded`).
    onedrive_space: Arc<OneDriveSpace>,
}

impl OneDriveSteps {
    /// Made by the folder's start (`sync_tasks::start_order::start_sync_tasks`),
    /// with the parts it made before.
    #[allow(clippy::too_many_arguments)]
    pub fn new(graph: GraphAccess, tree_lock: Arc<tokio::sync::Mutex<()>>, content_files: Arc<ItemContentFiles>, content_upload: ContentUpload, name_holders: NameHolders, lost_answers: LostAnswers, made_items: MadeItems, running_steps: Arc<RunningSteps>, onedrive_space: Arc<OneDriveSpace>) -> Self {
        Self { graph, tree_lock, content_files, content_upload, name_holders, lost_answers, made_items, running_steps, onedrive_space }
    }

    /// Runs `step` and answers what came of it: the function of its case. Called by the
    /// plan's task (`step_runners::StepRunners::of_runners`), which runs one step per
    /// item, content as the pool has upload slots and the rest one at a time. Its
    /// task is aborted at a stop of the folder's sync (`RunningSteps::abort_requests`)
    /// while its request is in flight — also while it reads what became of a request
    /// whose answer was lost —, and never once its commit has begun
    /// (`RunningSteps::begin_commit`): what the step memory holds — the record of the
    /// request among it — lets the next run go on or adopt.
    pub async fn run(&self, step: OneDriveStep) -> StepOutcome {
        match &step {
            OneDriveStep::MakeDirectory { item, parent, name } => self.run_make_directory(item, parent, name).await,
            OneDriveStep::SendNewFile { item, parent, name } => self.run_send_new_file(item, parent, name).await,
            OneDriveStep::SendContent { item } => self.run_send_content(item).await,
            OneDriveStep::SendMove { item, parent, name } => self.run_send_move(item, parent, name).await,
            OneDriveStep::SendFileRemoval { item } => self.run_send_file_removal(item).await,
            OneDriveStep::SendDirectoryRemoval { item, below } => self.run_send_directory_removal(item, below).await,
        }
    }

    /// Refuses a reference of another drive than the folder's: nothing is sent for it.
    fn check_drive(&self, item: &ItemVersions, reference: &OneDriveItemReference) -> Result<(), StepOutcome> {
        match reference.drive_id == self.graph.drive_id {
            true => Ok(()),
            false => Err(StepOutcome::Failed(StepFailure::AnswerUnreadable { detail: format!("item {} names an item of another drive than the folder's", item.local_item_id) })),
        }
    }

    /// The item of OneDrive `item` is bound to, in the folder's drive, and what a request
    /// about it is guarded with; the failure for an item with neither.
    fn find_guarded_item(&self, item: &ItemVersions) -> Result<(OneDriveItemReference, RequestGuard), StepOutcome> {
        let unreadable = |what: &str| StepOutcome::Failed(StepFailure::AnswerUnreadable { detail: format!("item {} has {what}", item.local_item_id) });
        let onedrive_item = item.onedrive_item.clone().ok_or_else(|| unreadable("no item of OneDrive"))?;
        self.check_drive(item, &onedrive_item)?;
        let guard = decide_guard(item).ok_or_else(|| unreadable("no eTag and no cTag to guard a request with"))?;
        Ok((onedrive_item, guard))
    }

    /// Writes what must outlive a crash before the request leaves, in one job of the
    /// store: the temporary name, when `name` is not the name of the item's local place,
    /// and the record of the request, when it is one of one answer.
    async fn remember_before_request(&self, item: &ItemVersions, name: Option<&OsStr>, request: Option<RecordedRequest>) -> Result<(), StepOutcome> {
        let id = item.local_item_id;
        let local_place = item.local.as_ref().and_then(|local| local.place.clone());
        let temporary_name = match (name, local_place) {
            (Some(name), Some(toward)) if toward.name != name => Some(TemporaryName { name: name.to_owned(), toward }),
            _ => None,
        };
        let record = request.map(|request| RequestRecord { item: id, onedrive_item: item.onedrive_item.clone(), request, sent_at: (self.graph.clock)() });
        if temporary_name.is_none() && record.is_none() {
            return Ok(());
        }
        self.graph
            .store
            .call(move |store| {
                if let Some(temporary_name) = &temporary_name {
                    store.step_memory().remember_temporary_name(id, temporary_name)?;
                }
                if let Some(record) = &record {
                    store.step_memory().remember_request_record(record)?;
                }
                Ok(())
            })
            .await
            .map_err(fail_store)
    }

    /// The record of the request of `item` goes: OneDrive answered for certain, or what
    /// became of the request was read. A failure of the store is the step's outcome.
    async fn forget_request_record(&self, item: &ItemVersions) -> Result<(), StepOutcome> {
        let id = item.local_item_id;
        self.graph.store.call(move |store| store.step_memory().forget_request_record(id)).await.map_err(fail_store)
    }

    /// Ends the step with `outcome` after a refusal that is certain: the record goes.
    async fn refuse_for_certain(&self, item: &ItemVersions, outcome: StepOutcome) -> StepOutcome {
        match self.forget_request_record(item).await {
            Ok(()) => outcome,
            Err(store_failure) => store_failure,
        }
    }

    /// What the step does with the fate of its request, read from OneDrive. `name` is the
    /// name sent, for a taken name. `is_refused_for_certain`: the request was refused, so
    /// an item found as planned is no request that did not go through but a changed item
    /// (a 412 said so) — but for a new item, where "nothing is there by now" after a 409
    /// is a name that is free again, tried again. The record goes with every fate but
    /// `WentThrough`, whose commit takes it.
    async fn follow_fate(&self, item: &ItemVersions, fate: LostAnswerFate, name: Option<&OsStr>, is_refused_for_certain: bool, detail: &str) -> RequestAnswer {
        let outcome = match fate {
            LostAnswerFate::WentThrough(confirmed) => return Ok(confirmed),
            LostAnswerFate::DidNotGoThrough if !is_refused_for_certain || name.is_some_and(|_| item.onedrive_item.is_none()) => StepOutcome::Refused(StepRefusal::Transient { detail: detail.to_owned() }),
            LostAnswerFate::DidNotGoThrough | LostAnswerFate::ItemChanged => StepOutcome::Refused(StepRefusal::ItemChanged),
            LostAnswerFate::NameHeld { holder_name } => match name {
                Some(name) => describe_name_taken(item, name, holder_name),
                None => StepOutcome::Refused(StepRefusal::ItemChanged),
            },
        };
        Err(self.refuse_for_certain(item, outcome).await)
    }

    /// After a refusal that is certain and that an earlier try of the same step may
    /// explain (a 409 of a new directory, a 412 of a move): one read of what OneDrive
    /// has, and the fate followed. A read that fails is the step's outcome, and the
    /// record goes: the request was refused.
    async fn follow_explained_refusal(&self, item: &ItemVersions, request: SentRequest<'_>, name: Option<&OsStr>, detail: &str) -> RequestAnswer {
        match self.lost_answers.read_request_outcome(item, request).await {
            Ok(fate) => self.follow_fate(item, fate, name, true, detail).await,
            Err(failed_read) => Err(self.refuse_for_certain(item, failed_read.into_outcome()).await),
        }
    }

    /// After an answer that says nothing certain: one read of what became of the
    /// request, and the fate followed. When the read says nothing certain either, the
    /// step ends `Transient` and the record STANDS: it keeps the item and its
    /// destination busy until it is settled. A read OneDrive refuses for certain will
    /// never say what became of the request: the record goes, and the step answers
    /// `StepRefusal::ItemChanged`, which counts a try and asks for the listing cycle that
    /// brings what OneDrive has.
    async fn follow_lost_answer(&self, item: &ItemVersions, request: SentRequest<'_>, name: Option<&OsStr>, detail: String) -> RequestAnswer {
        match self.lost_answers.read_request_outcome(item, request).await {
            Ok(fate) => self.follow_fate(item, fate, name, false, &detail).await,
            Err(FailedRead::NothingCertain(_)) => Err(StepOutcome::Refused(StepRefusal::Transient { detail })),
            Err(FailedRead::RefusedForCertain(outcome)) => {
                tracing::warn!("what became of the request of item {} cannot be read from OneDrive ({outcome:?}): a listing is what is left", item.local_item_id);
                Err(self.refuse_for_certain(item, StepOutcome::Refused(StepRefusal::ItemChanged)).await)
            }
        }
    }

    /// What a refusal that needs no second look leaves a step that sends no content to
    /// answer; the record goes. A 507 reads the quota, with a size of 0.
    async fn refuse_plainly(&self, item: &ItemVersions, error: &WriteError, not_found: StepRefusal) -> StepOutcome {
        let refusal = match decide_refusal(error) {
            RefusedRequest::NotFound => not_found,
            RefusedRequest::NoSpace => self.onedrive_space.refuse_for_space(&self.graph.drive, 0, (self.graph.clock)()).await,
            RefusedRequest::Settled(refusal) => refusal,
            RefusedRequest::NameExists | RefusedRequest::Changed | RefusedRequest::SessionGone | RefusedRequest::AnswerLost { .. } => decide_plain_refusal(error),
        };
        self.refuse_for_certain(item, StepOutcome::Refused(refusal)).await
    }

    /// The commit of a step: says that it begins, takes the tree lock, and runs `commit`
    /// in ONE job of the store with the end of the request's record and the reading of
    /// the item's paths for the event. `Err` with the outcome when the step runs no more
    /// (a stop aborted it: nothing is written) and when the store refuses or fails.
    async fn commit_step(&self, item: &ItemVersions, commit: impl FnOnce(&mut TreeStore) -> Result<StepCommitOutcome, TreeError> + Send + 'static) -> Result<CommittedStep, StepOutcome> {
        let id = item.local_item_id;
        if !self.running_steps.begin_commit(id) {
            return Err(StepOutcome::NotRun(StepObstacle::SendingStopped));
        }
        let _tree = self.tree_lock.lock().await;
        self.graph
            .store
            .call(move |store| {
                let path_before = store.read_items(|items| Ok(items.find_synced_path(id)?.or(items.find_path(id)?)))?;
                let outcome = commit(store)?;
                store.step_memory().forget_request_record(id)?;
                let path_after = store.read_items(|items| items.find_path(id))?;
                Ok(CommittedStep { outcome, path_before, path_after })
            })
            .await
            .map_err(fail_store)
    }

    /// `MakeDirectory`: a directory made here is made in OneDrive in `parent` under
    /// `name`.
    ///
    /// Request: today's `mkdir` (`upload/steps/meta.rs:19`), which calls `create_folder`
    /// (`G/write.rs:120`). Answer: the made `driveItem` as a `ConfirmedItem`. 409: one
    /// read of the place (`LostAnswers::read_request_outcome` with
    /// `SentRequest::MakeDirectory`; `meta.rs:33-36`): an EMPTY directory there that no
    /// row is bound to is this step's own earlier request, and is adopted (`WentThrough`);
    /// another item — a directory with something in it too: another client's, whose
    /// content no row knows — is `StepRefusal::NameTaken` with the name it has there
    /// (`NameHeld`) and the place the step memory names (the directory of the place of
    /// `item.local`, under `name`);
    /// nothing there by now is `StepRefusal::Transient`. 404: `DirectoryGone`.
    /// Commit: `commit_make_directory(item, parent, name, made)`; then, with no lock,
    /// `ItemContentFiles::mark_made_directory`. Step memory before the request: the
    /// record of the request, and a temporary name, when `name` is one; the commit
    /// forgets the tries and a temporary name the directory was not made under.
    /// `RowsChanged`: `MadeItems::follow_up_new_item`.
    async fn run_make_directory(&self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr) -> StepOutcome {
        if let Err(outcome) = self.check_drive(item, parent) {
            return outcome;
        }
        let graph_name = match to_graph_name(name) {
            Ok(graph_name) => graph_name,
            Err(refusal) => return StepOutcome::Refused(refusal),
        };
        let recorded = RecordedRequest::MakeDirectory { parent: parent.clone(), name: name.to_owned() };
        if let Err(outcome) = self.remember_before_request(item, Some(name), Some(recorded)).await {
            return outcome;
        }
        let request = SentRequest::MakeDirectory { parent, name };
        let answered = {
            let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
            self.graph.drive.create_folder(parent.item_id.as_str(), graph_name).await
        };
        let made = match answered {
            Ok(answer) => match to_confirmed_item(&answer, &self.graph.drive_id) {
                Ok(made) => Ok(Some(made)),
                Err(failure) => self.follow_lost_answer(item, request, Some(name), format!("{failure:?}")).await,
            },
            Err(error) => match decide_refusal(&error) {
                RefusedRequest::NameExists => self.follow_explained_refusal(item, request, Some(name), "nothing holds the name of the new directory in OneDrive any more").await,
                RefusedRequest::AnswerLost { detail } => self.follow_lost_answer(item, request, Some(name), detail).await,
                _ => Err(self.refuse_plainly(item, &error, StepRefusal::DirectoryGone).await),
            },
        };
        let made = match made {
            Ok(Some(made)) => made,
            Ok(None) => return self.refuse_for_certain(item, StepOutcome::Failed(StepFailure::AnswerUnreadable { detail: "OneDrive answered no directory for the one that was made".to_owned() })).await,
            Err(outcome) => return outcome,
        };
        let (planned, sent_parent, sent_name, committed_item) = (item.clone(), parent.clone(), name.to_owned(), made.clone());
        let committed = match self.commit_step(item, move |store| store.item_store().commit_make_directory(&planned, &sent_parent, &sent_name, &committed_item)).await {
            Ok(committed) => committed,
            Err(outcome) => return outcome,
        };
        match committed.outcome {
            StepCommitOutcome::Written => {
                let (content_files, planned, marked) = (Arc::clone(&self.content_files), item.clone(), made);
                match tokio::task::spawn_blocking(move || content_files.mark_made_directory(&planned, &marked)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!("the directory of item {} is not marked with its item id: {error}", item.local_item_id),
                    Err(error) => tracing::warn!("the marking of the directory of item {} ended: {error}", item.local_item_id),
                }
                describe_written(ActivityKind::Uploaded, committed.path_after, None)
            }
            StepCommitOutcome::RowsChanged => StepOutcome::RowsChanged(self.made_items.follow_up_new_item(item, &made).await),
        }
    }

    /// Opens the content `item` sends, off the runtime.
    async fn open_content(&self, item: &ItemVersions) -> Result<Arc<LocalContentFile>, StepOutcome> {
        let (content_files, planned) = (Arc::clone(&self.content_files), item.clone());
        match tokio::task::spawn_blocking(move || content_files.open_to_send(&planned)).await {
            Ok(Ok(file)) => Ok(Arc::new(file)),
            Ok(Err(trouble)) => Err(to_outcome(trouble)),
            Err(error) => Err(StepOutcome::Failed(StepFailure::Store { detail: format!("the opening of the content of item {} ended: {error}", item.local_item_id) })),
        }
    }

    /// After the commit of an upload was written, with no lock: the file that was sent is
    /// marked, and the bytes counted off the space OneDrive has.
    async fn mark_sent_content(&self, item: &ItemVersions, file: &Arc<LocalContentFile>, sent: &SentContent) {
        let (content_files, marked_file, confirmed) = (Arc::clone(&self.content_files), Arc::clone(file), sent.item.clone());
        match tokio::task::spawn_blocking(move || content_files.mark_sent_file(&marked_file, &confirmed)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!("the file of item {} is not marked as sent: {error}", item.local_item_id),
            Err(error) => tracing::warn!("the marking of the file of item {} ended: {error}", item.local_item_id),
        }
        self.onedrive_space.count_uploaded(file.size());
    }

    /// `SendNewFile`: a file made here is sent, with its content, into `parent` under
    /// `name`.
    ///
    /// First the content is opened (`ItemContentFiles::open_to_send`, off the runtime; a
    /// trouble of it is the outcome). Then an item an earlier upload of this item left
    /// with other content is removed, or adopted and committed as the upload
    /// (`ContentUpload::remove_mismatched_item`; today `Job::clear_bad_item`,
    /// `upload/content.rs:165`). Request: today's `Job::create`
    /// (`upload/content.rs:200`) over `Job::send` (`upload/content/session.rs:127`):
    /// `ContentUpload::send_content` with `ContentTarget::NewFile`. Answer: a
    /// `SentContent`, its hash asked already. 409 (`SendContentError::NameExists`): the
    /// hash of the file is read (`LocalContentFile::hash`) and
    /// `NameHolders::find_name_holder` asked — the
    /// empty file of this item's own session: the upload goes on; this folder's own empty
    /// file, an abandoned session or another item's session past its end: removed,
    /// cancelled or forgotten, and the request sent once more in
    /// this step; an unknown empty file or another item's live session:
    /// `NameHeldByUploadSession`; the same content there
    /// that no row is bound to: adopted; another item: `NameTaken`, with the place the
    /// step memory names (the directory of the place of `item.local`, under `name`).
    /// 404: `DirectoryGone`. 507: `NoSpace`, read by the upload.
    /// Commit: `commit_send_new_file(item, parent, name, made, ended_session)` — `parent`
    /// and `name` those of `SentContent::new_file_place`, the place of the session's
    /// empty file when a session was gone on with; `ended_session` the address of the
    /// upload session that sent the content, none for an empty file —, which forgets
    /// that session and the tries; then, with no lock, `ItemContentFiles::mark_sent_file`
    /// and `OneDriveSpace::count_uploaded`. Step memory
    /// before the request: a temporary name, when `name` is one; the opening and the
    /// session, or the record of an empty file's one request (see `content_upload`).
    /// `RowsChanged`: `MadeItems::follow_up_new_item`; the session stays, and the next try
    /// finds it gone and forgets it (`forget_ended_upload_session`).
    async fn run_send_new_file(&self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr) -> StepOutcome {
        let id = item.local_item_id;
        if let Err(outcome) = self.check_drive(item, parent) {
            return outcome;
        }
        if let Err(refusal) = to_graph_name(name) {
            return StepOutcome::Refused(refusal);
        }
        let file = match self.open_content(item).await {
            Ok(file) => file,
            Err(outcome) => return outcome,
        };
        let memory = match self.graph.store.read_items(move |items| items.find_item_memory(id)).await {
            Ok(memory) => memory,
            Err(error) => return fail_store(error),
        };
        if let Err(outcome) = self.remember_before_request(item, Some(name), None).await {
            return outcome;
        }
        let place = NewFilePlace { parent: parent.clone(), name: name.to_owned() };
        let target = ContentTarget::NewFile(place.clone());
        let sent = match self.content_upload.remove_mismatched_item(item, &memory, &file).await {
            Err(outcome) => return outcome,
            Ok(Some(adopted)) => adopted,
            Ok(None) => {
                let mut is_sent_once_more = false;
                loop {
                    match self.content_upload.send_content(item, &target, &file).await {
                        Ok(sent) => break sent,
                        Err(SendContentError::Outcome(outcome)) => return outcome,
                        // A second 409 in the same step: no step asks a third time.
                        Err(SendContentError::NameExists) if is_sent_once_more => return StepOutcome::Refused(StepRefusal::NameHeldByUploadSession),
                        Err(SendContentError::NameExists) => {}
                    }
                    let sent_hash = match hash_content(&file).await {
                        Ok(sent_hash) => sent_hash,
                        Err(outcome) => return outcome,
                    };
                    match self.name_holders.find_name_holder(item, parent, name, Some(&sent_hash)).await {
                        Err(outcome) => return outcome,
                        Ok(NameHolder::Nobody | NameHolder::OwnSession | NameHolder::AnotherItemsEndedSession { .. } | NameHolder::AbandonedSession(_) | NameHolder::OwnEmptyFile(_)) => is_sent_once_more = true,
                        Ok(NameHolder::AnotherItemsSession | NameHolder::UnknownEmptyFile) => return StepOutcome::Refused(StepRefusal::NameHeldByUploadSession),
                        Ok(NameHolder::OwnEarlierUpload(found)) => break SentContent { item: found, sent_hash, ended_session: None, new_file_place: Some(place.clone()) },
                        Ok(NameHolder::AnotherItem { holder_name }) => return describe_name_taken(item, name, holder_name),
                    }
                }
            }
        };
        let sent_place = sent.new_file_place.clone().unwrap_or(place);
        let (planned, made, ended_session) = (item.clone(), sent.item.clone(), sent.ended_session.clone());
        let committed = match self.commit_step(item, move |store| store.item_store().commit_send_new_file(&planned, &sent_place.parent, &sent_place.name, &made, ended_session.as_ref())).await {
            Ok(committed) => committed,
            Err(outcome) => return outcome,
        };
        match committed.outcome {
            StepCommitOutcome::Written => {
                self.mark_sent_content(item, &file, &sent).await;
                describe_written(ActivityKind::Uploaded, committed.path_after, None)
            }
            StepCommitOutcome::RowsChanged => StepOutcome::RowsChanged(self.made_items.follow_up_new_item(item, &sent.item).await),
        }
    }

    /// `SendContent`: the content of a file OneDrive has, changed here, is sent into that
    /// item where OneDrive has it, guarded.
    ///
    /// The content is opened as for a new file. An item with no guard
    /// (`confirmed_item::decide_guard`) is `StepFailure::AnswerUnreadable`. Request: the
    /// content half of today's `Job::update` (`upload/content.rs:326`, from
    /// `:354`) over `Job::send`: `ContentUpload::send_content` with
    /// `ContentTarget::ExistingItem { guard }` (`G/upload.rs:56`). The commit writes to
    /// the row that has the binding at the commit, which after a save-over is another
    /// local item id than the step's. Answer: a `SentContent`. What the upload decides
    /// itself and answers as the outcome: 412 — the hash sent, there already, is this
    /// step's own earlier upload and is answered as sent; anything else is
    /// `ItemChanged`, and the next plan has the case as rows (today's `changed`,
    /// `content.rs:388`, decided it in place) —, 404 `ItemGone`, 507 `NoSpace`.
    /// Commit: `commit_send_content(item, sent, ended_session)`, which forgets the session
    /// it is told ended and the tries; then `mark_sent_file` and
    /// `OneDriveSpace::count_uploaded`. `RowsChanged` — the versions changed, no row has the
    /// binding any more, or a second `SendContent` of the same item committed first: a
    /// listing cycle is asked for.
    async fn run_send_content(&self, item: &ItemVersions) -> StepOutcome {
        let guard = match self.find_guarded_item(item) {
            Ok((_, guard)) => guard,
            Err(outcome) => return outcome,
        };
        let file = match self.open_content(item).await {
            Ok(file) => file,
            Err(outcome) => return outcome,
        };
        let sent = match self.content_upload.send_content(item, &ContentTarget::ExistingItem { guard }, &file).await {
            Ok(sent) => sent,
            Err(SendContentError::Outcome(outcome)) => return outcome,
            // No place is named for an existing item: the upload answers this itself.
            Err(SendContentError::NameExists) => return StepOutcome::Refused(StepRefusal::ItemChanged),
        };
        let (planned, confirmed, ended_session) = (item.clone(), sent.item.clone(), sent.ended_session.clone());
        let committed = match self.commit_step(item, move |store| store.item_store().commit_send_content(&planned, &confirmed, ended_session.as_ref())).await {
            Ok(committed) => committed,
            Err(outcome) => return outcome,
        };
        match committed.outcome {
            StepCommitOutcome::Written => {
                self.mark_sent_content(item, &file, &sent).await;
                describe_written(ActivityKind::Uploaded, committed.path_after, None)
            }
            StepCommitOutcome::RowsChanged => StepOutcome::RowsChanged(RowsChangedFollowUp::CycleWanted),
        }
    }

    /// `SendMove`: a move or a rename made here is sent, guarded: the item goes into
    /// `parent` under `name`, the local name or a temporary one.
    ///
    /// Step memory before the request: the record of the request, and, when `name` is a
    /// temporary name, `remember_temporary_name` (today `temporary`,
    /// `upload/steps/shared.rs:333`).
    /// Request: today's `moved` (`upload/steps/meta.rs:61`), which calls `update_item`
    /// (`G/write.rs:129`) with only what differs from where the row says OneDrive has the
    /// item (`item.onedrive`'s place): the name when it is not `name`, the directory when
    /// the row bound to `parent` (`ItemReader::find_bound_item`, in the read before the
    /// request) is not that place's directory. When neither differs nothing is sent —
    /// the item is there already, an answer lost and the rows merged since
    /// (`meta.rs:73`) —: the item is read (`item`, `G/mod.rs:217`) and committed as it
    /// is. An item with no guard is `StepFailure::AnswerUnreadable`. Answer: the
    /// `driveItem` as a `ConfirmedItem`: the name Graph stored. 409: `NameTaken` with the
    /// holder's name as OneDrive spells it (`NameHolders::find_holder_name`) and the
    /// place the step memory names (the directory of the place of `item.local`, under
    /// `name`); `Transient` when nothing holds the name by now. 412: the
    /// item is read once (`LostAnswers::read_request_outcome` with
    /// `SentRequest::SendMove`): there already under the place
    /// sent is this step's own earlier request and is adopted (`meta.rs:100-104`);
    /// anything else is `ItemChanged`. 404: `ItemGone`, also when it is the directory
    /// sent into that is gone, which Graph answers alike: both ask for a cycle.
    /// Commit: `commit_send_move(item, parent, name, moved)`, which forgets the tries and
    /// the temporary name when the confirmed place is the one it led to.
    /// `RowsChanged` — also when the directory sent into has no OneDrive version any
    /// more: a listing cycle is asked for.
    async fn run_send_move(&self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr) -> StepOutcome {
        let (onedrive_item, guard) = match self.find_guarded_item(item) {
            Ok(guarded) => guarded,
            Err(outcome) => return outcome,
        };
        if let Err(outcome) = self.check_drive(item, parent) {
            return outcome;
        }
        let graph_name = match to_graph_name(name) {
            Ok(graph_name) => graph_name,
            Err(refusal) => return StepOutcome::Refused(refusal),
        };
        let bound = parent.clone();
        let parent_row = match self.graph.store.read_items(move |items| Ok(items.find_bound_item(&bound)?.map(|row| row.local_item_id))).await {
            Ok(parent_row) => parent_row,
            Err(error) => return fail_store(error),
        };
        let onedrive_place = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref());
        let is_name_other = onedrive_place.is_none_or(|place| place.name != name);
        let is_directory_other = match (onedrive_place, parent_row) {
            (Some(place), Some(parent_row)) => place.parent_local_item_id != parent_row,
            _ => true,
        };
        let item_id = onedrive_item.item_id.as_str();
        let moved = if !is_name_other && !is_directory_other {
            // The item is there already: nothing is sent, and it is committed as it is.
            if let Err(outcome) = self.remember_before_request(item, Some(name), None).await {
                return outcome;
            }
            let read = {
                let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
                self.graph.drive.item(item_id).await
            };
            match read {
                Ok(answer) => to_confirmed_item(&answer, &self.graph.drive_id).map(Some).map_err(StepOutcome::Failed),
                Err(DriveError::NotFound) => Err(StepOutcome::Refused(StepRefusal::ItemGone)),
                Err(error) => Err(StepOutcome::Refused(decide_read_refusal(error))),
            }
        } else {
            let recorded = RecordedRequest::SendMove { parent: parent.clone(), name: name.to_owned() };
            if let Err(outcome) = self.remember_before_request(item, Some(name), Some(recorded)).await {
                return outcome;
            }
            let request = SentRequest::SendMove { parent, name };
            let change = ItemChange { name: is_name_other.then_some(graph_name), parent_id: is_directory_other.then_some(parent.item_id.as_str()), modified: None };
            let answered = {
                let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
                self.graph.drive.update_item(item_id, guard.as_str(), &change).await
            };
            match answered {
                Ok(answer) => match to_confirmed_item(&answer, &self.graph.drive_id) {
                    Ok(moved) => Ok(Some(moved)),
                    Err(failure) => self.follow_lost_answer(item, request, None, format!("{failure:?}")).await,
                },
                Err(error) => match decide_refusal(&error) {
                    RefusedRequest::NameExists => {
                        let outcome = match self.name_holders.find_holder_name(parent, name).await {
                            Ok(Some(holder_name)) => describe_name_taken(item, name, holder_name),
                            Ok(None) => StepOutcome::Refused(StepRefusal::Transient { detail: "nothing holds the name the item is moved to in OneDrive any more".to_owned() }),
                            Err(outcome) => outcome,
                        };
                        Err(self.refuse_for_certain(item, outcome).await)
                    }
                    RefusedRequest::Changed => self.follow_explained_refusal(item, request, None, "the item changed in OneDrive").await,
                    RefusedRequest::AnswerLost { detail } => self.follow_lost_answer(item, request, None, detail).await,
                    _ => Err(self.refuse_plainly(item, &error, StepRefusal::ItemGone).await),
                },
            }
        };
        let moved = match moved {
            Ok(Some(moved)) => moved,
            Ok(None) => return self.refuse_for_certain(item, StepOutcome::Refused(StepRefusal::ItemGone)).await,
            Err(outcome) => return outcome,
        };
        let (planned, sent_parent, sent_name) = (item.clone(), parent.clone(), name.to_owned());
        let committed = match self.commit_step(item, move |store| store.item_store().commit_send_move(&planned, &sent_parent, &sent_name, &moved)).await {
            Ok(committed) => committed,
            Err(outcome) => return outcome,
        };
        match committed.outcome {
            StepCommitOutcome::Written => describe_written(ActivityKind::CloudMoved, committed.path_after, committed.path_before),
            StepCommitOutcome::RowsChanged => StepOutcome::RowsChanged(RowsChangedFollowUp::CycleWanted),
        }
    }

    /// What a removal's request leaves the step: `Ok` when the item is gone in OneDrive —
    /// removed, found gone (a 404 is a confirmation), or read as gone after a lost
    /// answer —, else the outcome. A 412 is `ItemChanged`.
    async fn follow_removal_answer(&self, item: &ItemVersions, answered: Result<(), WriteError>) -> Result<(), StepOutcome> {
        let error = match answered {
            Ok(()) | Err(WriteError::NotFound) => return Ok(()),
            Err(error) => error,
        };
        match decide_refusal(&error) {
            RefusedRequest::AnswerLost { detail } => match self.follow_lost_answer(item, SentRequest::SendRemoval, None, detail).await? {
                None => Ok(()),
                // A removal confirms no item: read as one that did not go through.
                Some(_) => Err(self.refuse_for_certain(item, StepOutcome::Refused(StepRefusal::ItemChanged)).await),
            },
            RefusedRequest::Changed => Err(self.refuse_for_certain(item, StepOutcome::Refused(StepRefusal::ItemChanged)).await),
            _ => Err(self.refuse_plainly(item, &error, StepRefusal::ItemGone).await),
        }
    }

    /// `SendFileRemoval`: the removal of a file that was removed here is sent, guarded.
    ///
    /// Request: today's `delete` (`upload/steps/meta.rs:196`), which calls `delete_item`
    /// (`G/write.rs:140`), on record before it leaves. An item with no guard is
    /// `StepFailure::AnswerUnreadable`.
    /// Answer: nothing. 404 is a confirmation: the item is gone
    /// (`meta.rs:217`). 412: `ItemChanged`; the next plan has "OneDrive changed its
    /// content: the item comes back" or "only moved it: the removal is sent" as rows,
    /// with the eTag a listing wrote (today `file_changed`, `meta.rs:251`, read the item
    /// and tried three times in place).
    /// Commit: `commit_send_file_removal(item, now)`, `now` in Unix seconds, which counts
    /// the removal among those sent a short time ago and forgets what was kept for the
    /// request. `RowsChanged`: a listing cycle is asked for.
    async fn run_send_file_removal(&self, item: &ItemVersions) -> StepOutcome {
        let (onedrive_item, guard) = match self.find_guarded_item(item) {
            Ok(guarded) => guarded,
            Err(outcome) => return outcome,
        };
        if let Err(outcome) = self.remember_before_request(item, None, Some(RecordedRequest::SendFileRemoval)).await {
            return outcome;
        }
        let answered = {
            let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
            self.graph.drive.delete_item(onedrive_item.item_id.as_str(), guard.as_str()).await
        };
        if let Err(outcome) = self.follow_removal_answer(item, answered).await {
            return outcome;
        }
        let (planned, now) = (item.clone(), (self.graph.clock)());
        let committed = match self.commit_step(item, move |store| store.item_store().commit_send_file_removal(&planned, now)).await {
            Ok(committed) => committed,
            Err(outcome) => return outcome,
        };
        match committed.outcome {
            StepCommitOutcome::Written => describe_written(ActivityKind::CloudDeleted, committed.path_before, None),
            StepCommitOutcome::RowsChanged => StepOutcome::RowsChanged(RowsChangedFollowUp::CycleWanted),
        }
    }

    /// `SendDirectoryRemoval`: the removal of a directory is sent, with no guard, as one
    /// removal of all OneDrive has in it.
    ///
    /// Request: today's `delete_folder` (`upload/steps/meta.rs:275`), which calls
    /// `delete_folder` (`G/write.rs:153`). 404 is a confirmation. BEFORE the request
    /// leaves, under the tree lock, in one job of the store:
    /// `ItemStore::can_directory_removal_be_sent(item,
    /// below, items_with_running_step, destinations_of_running_steps)`, the two sets as
    /// the plan is given them and with nothing taken out — `RunningSteps::list_items`
    /// and the items of the request records that stand
    /// (`ItemReader::list_request_records`), and the rows bound to
    /// `RunningSteps::list_destinations` and to the destinations of those records
    /// (`ItemReader::find_bound_item` of each, as `plan_input.rs` resolves them), both
    /// read under the lock: every item a request not
    /// settled yet is about, and every directory such a request sends an item into, a
    /// request whose answer was lost among them, by its record; the
    /// store leaves this step's own request out itself; when it
    /// answers no, nothing is sent and the step ends `RowsChanged(Nothing)` — an
    /// unguarded removal takes all OneDrive has in the directory, and it is sent only
    /// while the rows still say that all of it was shown here and removed here, no
    /// listing is under way and no request not settled is about any of it. When it
    /// answers yes, the record of this removal is written in the same job. The lock is
    /// let go before the request.
    /// Commit: `commit_send_directory_removal(item, below, now)`, which asks the same of
    /// the rows alone and counts the directory and each of `below` among the removals
    /// sent a short time ago. `RowsChanged`: the directory is gone in OneDrive with what the rows
    /// still say it has: a listing cycle is asked for.
    async fn run_send_directory_removal(&self, item: &ItemVersions, below: &[ItemVersions]) -> StepOutcome {
        let id = item.local_item_id;
        let Some(onedrive_item) = item.onedrive_item.clone() else {
            return StepOutcome::Failed(StepFailure::AnswerUnreadable { detail: format!("item {id} has no item of OneDrive to remove") });
        };
        if let Err(outcome) = self.check_drive(item, &onedrive_item) {
            return outcome;
        }
        let can_be_sent = {
            let _tree = self.tree_lock.lock().await;
            let (mut busy_items, mut destinations) = (self.running_steps.list_items(), self.running_steps.list_destinations());
            let (planned, planned_below) = (item.clone(), below.to_vec());
            let record = RequestRecord { item: id, onedrive_item: Some(onedrive_item.clone()), request: RecordedRequest::SendDirectoryRemoval, sent_at: (self.graph.clock)() };
            self.graph
                .store
                .call(move |store| {
                    let destination_rows = store.read_items(|items| {
                        // The requests on record are not settled either, whether their step runs or not; this removal's own has none yet.
                        for standing in items.list_request_records(usize::MAX)? {
                            destinations.extend(standing.request.destination().cloned());
                            busy_items.insert(standing.item);
                        }
                        let mut destination_rows: BTreeSet<LocalItemId> = BTreeSet::new();
                        for destination in &destinations {
                            destination_rows.extend(items.find_bound_item(destination)?.map(|row| row.local_item_id));
                        }
                        Ok(destination_rows)
                    })?;
                    let can_be_sent = store.item_store().can_directory_removal_be_sent(&planned, &planned_below, &busy_items, &destination_rows)?;
                    if can_be_sent {
                        store.step_memory().remember_request_record(&record)?;
                    }
                    Ok(can_be_sent)
                })
                .await
        };
        match can_be_sent {
            Ok(true) => {}
            Ok(false) => return StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing),
            Err(error) => return fail_store(error),
        }
        let answered = {
            let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
            self.graph.drive.delete_folder(onedrive_item.item_id.as_str()).await
        };
        if let Err(outcome) = self.follow_removal_answer(item, answered).await {
            return outcome;
        }
        let (planned, planned_below, now) = (item.clone(), below.to_vec(), (self.graph.clock)());
        let committed = match self.commit_step(item, move |store| store.item_store().commit_send_directory_removal(&planned, &planned_below, now)).await {
            Ok(committed) => committed,
            Err(outcome) => return outcome,
        };
        match committed.outcome {
            StepCommitOutcome::Written => describe_written(ActivityKind::CloudDeleted, committed.path_before, None),
            StepCommitOutcome::RowsChanged => StepOutcome::RowsChanged(RowsChangedFollowUp::CycleWanted),
        }
    }
}

#[cfg(test)]
pub(in crate::onedrive_steps) mod tests;
