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
//!    it true: the opening of an upload session, the session before its first byte, and a
//!    temporary name — for `MakeDirectory`, `SendNewFile` and `SendMove` alike, whenever
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
//!    an answer of success that cannot be read) does NOT end the step: it reads what
//!    became of the request (`LostAnswers::settle_lost_answer`) and goes on as the fate
//!    says — `WentThrough`: the commit; `DidNotGoThrough`:
//!    `StepRefusal::Transient`; `ItemChanged`: `StepRefusal::ItemChanged`; `NameHeld`:
//!    `StepRefusal::NameTaken`. So an item whose request may have gone through, and the
//!    directory it was sent into, have a running step until that is known;
//! 5. turns Graph's `driveItem` into a `ConfirmedItem` (`confirmed_item`): the id, the
//!    parent's reference, the name Graph stored, the eTag, a file's content;
//! 6. says its commit begins (`RunningSteps::begin_commit`: a stop aborts the step's task
//!    no more; when the answer is that the step runs no more, it was aborted, and the
//!    runner returns `StepOutcome::NotRun(StepObstacle::SendingStopped)` with nothing
//!    written, which nobody settles); takes the folder's tree lock, calls its
//!    commit of `step_commit` in one job of the store (`Store::call`), and lets the lock
//!    go. The commit itself forgets what the step memory kept for the confirmed request:
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
//! by the plan's task from the answer (`sync::sync_plan_task::step_settlement`), not
//! here: one place counts tries.
//!
//! The requests are the upload worker's (design §11): each function names the function
//! of today's worker it takes the request and its handling from, and the call of the
//! Graph client under it (`crates/konedrive-graph/src/drive/`, here `G/`).
//!
//! **Tested without the rest**, over the fixture (`fuse::item_fixture`: its store, its
//! backing directory, its fake OneDrive) with `ItemContentFiles` made over it; today's
//! upload tests are the reference for every scenario (`upload/tests/`), and the races'
//! test of design §12 puts a death at every point between a request and its commit:
// test: each_case_sends_the_request_of_its_planned_versions_and_commits_the_answer
// test: a_request_is_guarded_by_the_etag_the_step_was_planned_from_else_the_synced_ctag
// test: a_temporary_name_is_remembered_before_the_request_of_a_new_item_and_of_a_move
// test: a_commit_that_finds_the_rows_changed_after_a_new_item_was_made_removes_it_when_the_row_is_gone
// test: a_commit_that_finds_the_rows_changed_otherwise_asks_for_a_cycle
// test: a_step_replayed_after_a_death_between_its_request_and_its_commit_adopts_what_it_finds
// test: a_step_whose_answer_was_lost_runs_until_onedrive_was_read_and_commits_what_went_through
// test: a_refusal_is_answered_as_its_case_and_writes_no_row
// test: no_request_is_sent_while_the_tree_lock_is_held
// test: a_step_whose_commit_may_not_begin_writes_nothing
// test: each_written_step_answers_its_event_with_the_path_of_its_commit
// test: a_step_that_sends_no_content_holds_a_metadata_slot_for_its_request
// TODO: a measurement: a 412 is answered `ItemChanged` and waits for a listing to write the item's eTag (`SendContent`, `SendMove`, `SendFileRemoval`). Today's worker reads the item in place, because an eTag moves in OneDrive with no change of content (`upload/content.rs:139`); whether a delta names the item then is not measured. If it does not, the step is refused at every try: measure once on the test account, and if so let the 412 ask for the item to be read again.
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;
use std::sync::Arc;

use konedrive_tree::{ItemVersions, OneDriveItemReference, OneDriveStep, Store};

use crate::folder::running_steps::RunningSteps;
use crate::folder::step_outcome::StepOutcome;
use crate::fuse::ItemContentFiles;
use crate::upload::onedrive_space::OneDriveSpace;

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
/// What is left to clean in OneDrive: sessions to cancel, items uploads left.
pub mod session_cancellations;

use content_upload::ContentUpload;
use lost_answers::LostAnswers;
use made_items::MadeItems;
use name_holders::NameHolders;

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
    /// Made by the folder's start (`sync::sync_tasks::start_order::start_sync_tasks`),
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
    /// (`RunningSteps::begin_commit`): what the step memory holds lets the next run go on
    /// or adopt.
    pub async fn run(&self, step: OneDriveStep) -> StepOutcome {
        todo!()
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
    /// `ItemContentFiles::mark_made_directory`. Step memory before the request: a
    /// temporary name, when `name` is one; the commit
    /// forgets the tries and a temporary name the directory was not made under.
    /// `RowsChanged`: `MadeItems::follow_up_new_item`.
    async fn run_make_directory(&self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr) -> StepOutcome {
        todo!()
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
    /// session (see `content_upload`).
    /// `RowsChanged`: `MadeItems::follow_up_new_item`; the session stays, and the next try
    /// finds it gone and forgets it (`forget_ended_upload_session`).
    async fn run_send_new_file(&self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr) -> StepOutcome {
        todo!()
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
        todo!()
    }

    /// `SendMove`: a move or a rename made here is sent, guarded: the item goes into
    /// `parent` under `name`, the local name or a temporary one.
    ///
    /// Step memory before the request, when `name` is a temporary name:
    /// `remember_temporary_name` (today `temporary`, `upload/steps/shared.rs:333`).
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
        todo!()
    }

    /// `SendFileRemoval`: the removal of a file that was removed here is sent, guarded.
    ///
    /// Request: today's `delete` (`upload/steps/meta.rs:196`), which calls `delete_item`
    /// (`G/write.rs:140`). An item with no guard is `StepFailure::AnswerUnreadable`.
    /// Answer: nothing. 404 is a confirmation: the item is gone
    /// (`meta.rs:217`). 412: `ItemChanged`; the next plan has "OneDrive changed its
    /// content: the item comes back" or "only moved it: the removal is sent" as rows,
    /// with the eTag a listing wrote (today `file_changed`, `meta.rs:251`, read the item
    /// and tried three times in place).
    /// Commit: `commit_send_file_removal(item, now)`, `now` in Unix seconds, which counts
    /// the removal among those sent a short time ago and forgets what was kept for the
    /// request. `RowsChanged`: a listing cycle is asked for.
    async fn run_send_file_removal(&self, item: &ItemVersions) -> StepOutcome {
        todo!()
    }

    /// `SendDirectoryRemoval`: the removal of a directory is sent, with no guard, as one
    /// removal of all OneDrive has in it.
    ///
    /// Request: today's `delete_folder` (`upload/steps/meta.rs:275`), which calls
    /// `delete_folder` (`G/write.rs:153`). 404 is a confirmation. BEFORE the request
    /// leaves, under the tree lock, in one job of the store:
    /// `ItemStore::can_directory_removal_be_sent(item,
    /// below, items_with_running_step, destinations_of_running_steps)`, the two sets as
    /// the plan is given them and with nothing taken out — `RunningSteps::list_items`,
    /// and the rows bound to `RunningSteps::list_destinations`
    /// (`ItemReader::find_bound_item` of each, as `plan_input.rs` resolves them), both
    /// read under the lock: every item a request not
    /// settled yet is about, and every directory such a request sends an item into, a
    /// request whose answer was lost among them, since its step still runs; the
    /// store leaves this step's own request out itself; when it
    /// answers no, nothing is sent and the step ends `RowsChanged(Nothing)` — an
    /// unguarded removal takes all OneDrive has in the directory, and it is sent only
    /// while the rows still say that all of it was shown here and removed here, no
    /// listing is under way and no request not settled is about any of it. The lock is
    /// let go before the request.
    /// Commit: `commit_send_directory_removal(item, below, now)`, which asks the same of
    /// the rows alone and counts the directory and each of `below` among the removals
    /// sent a short time ago. `RowsChanged`: the directory is gone in OneDrive with what the rows
    /// still say it has: a listing cycle is asked for.
    async fn run_send_directory_removal(&self, item: &ItemVersions, below: &[ItemVersions]) -> StepOutcome {
        todo!()
    }
}
