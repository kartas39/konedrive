//! The runner of the steps towards OneDrive over the fixture and the fake OneDrive, and
//! what the tests of its parts are staged with ([`StepsFolder`]).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use konedrive_graph::drive::FRAGMENT_UNIT;
use konedrive_tree::{ItemMemory, LocalItemId, RequestRecord, SyncStep};
use serde_json::json;
use wiremock::ResponseTemplate;

use super::content_upload::Limits;
use super::session_cancellations::SessionCancellations;
use super::*;
use crate::fake_onedrive::{qx, FakeGraph};
use crate::mount::item_fixture::{ItemFixture, DRIVE_ID};
use crate::sync_plan_task::step_choice::describe_running_step;

/// A folder with no mount, a fake OneDrive that has what the rows say OneDrive has, and
/// every part of the steps towards OneDrive made over them as the folder's start makes
/// them. The first listing has ended: nothing holds the sending.
pub(in crate::onedrive_steps) struct StepsFolder {
    pub(crate) fixture: ItemFixture,
    pub(crate) graph: FakeGraph,
    pub(crate) steps: OneDriveSteps,
    pub(crate) name_holders: NameHolders,
    pub(crate) lost_answers: LostAnswers,
    pub(crate) made_items: MadeItems,
    pub(crate) cancellations: SessionCancellations,
    /// The pool of the client every part asks Graph through.
    pub(crate) pool: Arc<konedrive_graph::pool::TransferPool>,
}

impl StepsFolder {
    /// Over `fixture` as its rows are now, with fragments of 320 KiB.
    pub async fn start(fixture: ItemFixture) -> Self {
        let graph = fixture.start_fake_onedrive().await;
        // The fake answers items of the store's own drive.
        graph.with(|cloud| cloud.drive_id = Some(DRIVE_ID.to_owned()));
        fixture.sending_holds.set_listing_ended();
        let access = fixture.graph_access(&graph);
        let pool = Arc::clone(access.drive.pool());
        let lost_answers = LostAnswers::new(access.clone(), Arc::clone(&fixture.tree_lock));
        let cancellations = SessionCancellations::new(access.clone(), lost_answers.clone(), Arc::clone(&fixture.running_steps));
        let made_items = MadeItems::new(access.clone());
        let name_holders = NameHolders::new(access.clone(), cancellations.clone());
        let content_upload = ContentUpload::new(access.clone(), lost_answers.clone(), Arc::clone(&fixture.sending_holds), Arc::clone(&fixture.onedrive_space), Arc::clone(&fixture.running_steps), Limits { chunk: FRAGMENT_UNIT });
        let content_files = fixture.make_content_files();
        let steps = OneDriveSteps::new(
            access,
            Arc::clone(&fixture.tree_lock),
            Arc::clone(&content_files),
            content_upload.clone(),
            name_holders.clone(),
            lost_answers.clone(),
            made_items.clone(),
            Arc::clone(&fixture.running_steps),
            Arc::clone(&fixture.onedrive_space),
        );
        StepsFolder { fixture, graph, steps, name_holders, lost_answers, made_items, cancellations, pool }
    }

    /// Runs `step` as the plan's task does: begun in the running steps, run, ended.
    pub(crate) async fn run(&self, step: OneDriveStep) -> StepOutcome {
        let running = describe_running_step(&SyncStep::OneDrive(step.clone()));
        let item = running.item;
        assert!(self.fixture.running_steps.begin(running), "no step runs for item {item}");
        let outcome = self.steps.run(step).await;
        self.fixture.running_steps.end(item);
        outcome
    }

    /// Runs `step` as one whose task a stop took off the running steps while its request
    /// was in flight: its commit may not begin.
    pub(crate) async fn run_stopped_before_its_commit(&self, step: OneDriveStep) -> StepOutcome {
        self.steps.run(step).await
    }

    pub(crate) fn root() -> OneDriveItemReference {
        ItemFixture::onedrive_reference_of("")
    }

    /// The requests on record now.
    pub(crate) fn records(&self) -> Vec<RequestRecord> {
        self.fixture.read_rows(|items| items.list_request_records(usize::MAX))
    }

    /// What the steps remember of `item` that its row still says.
    pub(crate) fn memory(&self, item: LocalItemId) -> ItemMemory {
        self.fixture.read_rows(move |items| items.find_item_memory(item))
    }

    /// Every path of the fake drive, sorted.
    pub(crate) fn cloud_paths(&self) -> Vec<String> {
        self.graph.with(|cloud| cloud.paths())
    }

    /// How many requests of `method` whose path holds `fragment` the fake got.
    pub fn count(&self, method: &str, fragment: &str) -> usize {
        self.graph.with(|cloud| cloud.count(method, fragment))
    }

    /// The next `times` requests of `method` whose path holds `fragment` are answered
    /// `status`, with Graph's error body, and are not carried out.
    pub(crate) fn refuse(&self, method: &str, fragment: &str, status: u16, times: u32) {
        let code = match status {
            409 => "nameAlreadyExists",
            507 => "quotaLimitReached",
            _ => "scripted",
        };
        let answer = ResponseTemplate::new(status).set_body_json(json!({ "error": { "code": code, "message": format!("scripted {status}") } }));
        self.graph.with(|cloud| cloud.script(method, fragment, answer, times));
    }

    /// Settles the records that stand, one round of the cleaning task.
    pub(crate) async fn settle_records(&self) -> super::session_cancellations::CancellationRound {
        self.cancellations.settle_request_records(10).await
    }
}

/// The case of a step a test stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StagedStep {
    MakeDirectory,
    SendEmptyNewFile,
    SendEmptyContent,
    SendMove,
    SendFileRemoval,
    SendDirectoryRemoval,
}

impl StagedStep {
    /// Every case whose request is one answer, and so is on record.
    const ALL: [StagedStep; 6] = [StagedStep::MakeDirectory, StagedStep::SendEmptyNewFile, StagedStep::SendEmptyContent, StagedStep::SendMove, StagedStep::SendFileRemoval, StagedStep::SendDirectoryRemoval];

    /// The method and a fragment of the path of the case's one request.
    fn request(self) -> (&'static str, &'static str) {
        match self {
            StagedStep::MakeDirectory => ("POST", "children"),
            StagedStep::SendEmptyNewFile => ("PUT", "empty.txt"),
            StagedStep::SendEmptyContent => ("PUT", "items/a.txt/content"),
            StagedStep::SendMove => ("PATCH", "items/a.txt"),
            StagedStep::SendFileRemoval => ("DELETE", "items/a.txt"),
            StagedStep::SendDirectoryRemoval => ("DELETE", "items/docs"),
        }
    }

    /// The fragment of the path of the read that tells what became of the request.
    fn read(self) -> &'static str {
        match self {
            StagedStep::MakeDirectory => "R:/new",
            StagedStep::SendEmptyNewFile => "R:/empty.txt",
            StagedStep::SendEmptyContent | StagedStep::SendMove | StagedStep::SendFileRemoval => "items/a.txt",
            StagedStep::SendDirectoryRemoval => "items/docs",
        }
    }
}

/// A folder in which `staged` is what is unsent, and the step that sends it.
async fn stage(staged: StagedStep) -> (StepsFolder, OneDriveStep) {
    let fixture = ItemFixture::new();
    let root = StepsFolder::root();
    let mut below = Vec::new();
    let item = match staged {
        StagedStep::MakeDirectory => fixture.make_own_directory("new").local_item_id,
        StagedStep::SendEmptyNewFile => fixture.make_own_file("empty.txt", b"").local_item_id,
        StagedStep::SendEmptyContent => {
            let shown = fixture.show_onedrive_file("a.txt", b"old");
            fixture.land_copy(shown.local_item_id, b"old");
            fixture.change_file_here(shown.local_item_id, b"").local_item_id
        }
        StagedStep::SendMove => {
            let shown = fixture.show_onedrive_file("a.txt", b"old");
            fixture.move_here(shown.local_item_id, "b.txt").local_item_id
        }
        StagedStep::SendFileRemoval => {
            let shown = fixture.show_onedrive_file("a.txt", b"old");
            fixture.remove_here(shown.local_item_id).expect("the row with a removal to send").local_item_id
        }
        StagedStep::SendDirectoryRemoval => {
            let directory = fixture.show_onedrive_directory("docs");
            let file = fixture.show_onedrive_file("docs/c.txt", b"c");
            fixture.remove_here(file.local_item_id);
            below.push(fixture.versions_of(file.local_item_id));
            fixture.remove_here(directory.local_item_id).expect("the row with a removal to send").local_item_id
        }
    };
    let versions = fixture.versions_of(item);
    let step = match staged {
        StagedStep::MakeDirectory => OneDriveStep::MakeDirectory { item: versions, parent: root, name: "new".into() },
        StagedStep::SendEmptyNewFile => OneDriveStep::SendNewFile { item: versions, parent: root, name: "empty.txt".into() },
        StagedStep::SendEmptyContent => OneDriveStep::SendContent { item: versions },
        StagedStep::SendMove => OneDriveStep::SendMove { item: versions, parent: root, name: "b.txt".into() },
        StagedStep::SendFileRemoval => OneDriveStep::SendFileRemoval { item: versions },
        StagedStep::SendDirectoryRemoval => OneDriveStep::SendDirectoryRemoval { item: versions, below },
    };
    (StepsFolder::start(fixture).await, step)
}

/// What the fake drive holds once `staged` went through.
fn sent_paths(staged: StagedStep) -> Vec<&'static str> {
    match staged {
        StagedStep::MakeDirectory => vec!["new"],
        StagedStep::SendEmptyNewFile => vec!["empty.txt"],
        StagedStep::SendEmptyContent => vec!["a.txt"],
        StagedStep::SendMove => vec!["b.txt"],
        StagedStep::SendFileRemoval | StagedStep::SendDirectoryRemoval => vec![],
    }
}

/// What the fake drive holds while `staged` is unsent.
fn unsent_paths(staged: StagedStep) -> Vec<&'static str> {
    match staged {
        StagedStep::MakeDirectory | StagedStep::SendEmptyNewFile => vec![],
        StagedStep::SendEmptyContent | StagedStep::SendMove | StagedStep::SendFileRemoval => vec!["a.txt"],
        StagedStep::SendDirectoryRemoval => vec!["docs", "docs/c.txt"],
    }
}

/// Whether the rows say what `staged` did: nothing of the step's item is unsent.
fn is_committed(folder: &StepsFolder, staged: StagedStep, step: &OneDriveStep) -> bool {
    let row = folder.fixture.find_item(step.item().local_item_id);
    match staged {
        StagedStep::MakeDirectory | StagedStep::SendEmptyNewFile => row.is_some_and(|row| row.onedrive_item.is_some() && row.onedrive.is_some() && row.synced_place == row.local.as_ref().and_then(|local| local.place.clone())),
        StagedStep::SendEmptyContent => row.is_some_and(|row| row.synced_content.as_ref().is_some_and(|content| content.size() == 0) && row.unsent_since.is_none()),
        StagedStep::SendMove => row.is_some_and(|row| row.unsent_since.is_none() && row.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).is_some_and(|place| place.name == "b.txt")),
        StagedStep::SendFileRemoval | StagedStep::SendDirectoryRemoval => row.is_none_or(|row| row.onedrive.is_none() && row.synced_place.is_none()),
    }
}

#[tokio::test]
async fn each_case_sends_the_request_of_its_planned_versions_and_commits_the_answer() {
    for staged in StagedStep::ALL {
        let (folder, step) = stage(staged).await;
        let outcome = folder.run(step.clone()).await;
        let StepOutcome::Written { events } = &outcome else { panic!("{staged:?}: {outcome:?}") };
        assert_eq!(events.len(), 1, "{staged:?}");
        assert_eq!(folder.cloud_paths(), sent_paths(staged), "{staged:?}");
        assert!(is_committed(&folder, staged, &step), "{staged:?}: {:?}", folder.fixture.find_item(step.item().local_item_id));
        assert_eq!(folder.records(), [], "{staged:?}: the record goes with the commit");
        assert_eq!(folder.count(staged.request().0, staged.request().1), 1, "{staged:?}");
    }
}

#[tokio::test]
async fn each_written_step_answers_its_event_with_the_path_of_its_commit() {
    use konedrive_tree::ActivityKind::{CloudDeleted, CloudMoved, Uploaded};
    let cases = [
        (StagedStep::MakeDirectory, Uploaded, "new", None),
        (StagedStep::SendEmptyNewFile, Uploaded, "empty.txt", None),
        (StagedStep::SendEmptyContent, Uploaded, "a.txt", None),
        (StagedStep::SendMove, CloudMoved, "b.txt", Some("a.txt")),
        (StagedStep::SendFileRemoval, CloudDeleted, "a.txt", None),
        (StagedStep::SendDirectoryRemoval, CloudDeleted, "docs", None),
    ];
    for (staged, kind, path, former_path) in cases {
        let (folder, step) = stage(staged).await;
        let expected = StepEvent { kind, path: path.into(), former_path: former_path.map(Into::into) };
        assert_eq!(folder.run(step).await, StepOutcome::Written { events: vec![expected] }, "{staged:?}");
    }
}

#[tokio::test]
async fn a_request_is_guarded_by_the_etag_planned_from_else_the_synced_ctag_and_a_content_by_its_ctag() {
    for staged in [StagedStep::SendMove, StagedStep::SendFileRemoval] {
        let (folder, step) = stage(staged).await;
        let etag = step.item().onedrive_etag.clone().expect("a listed item has an eTag");
        folder.run(step).await;
        let guards = folder.graph.with(|cloud| cloud.guards.clone());
        assert_eq!(guards.first().map(|(_, tag)| tag.clone()), Some(etag), "{staged:?}: {guards:?}");
    }
    // A content sent into the item: the cTag of the content OneDrive has, which a rename does not raise.
    let (folder, step) = stage(StagedStep::SendEmptyContent).await;
    let ctag = match step.item().onedrive.as_ref().and_then(|onedrive| onedrive.content.as_ref()) {
        Some(konedrive_tree::ContentVersion::Hashed { ctag: Some(ctag), .. }) => ctag.clone(),
        other => panic!("OneDrive's content with a cTag: {other:?}"),
    };
    folder.run(step).await;
    let guards = folder.graph.with(|cloud| cloud.guards.clone());
    assert_eq!(guards.first(), Some(&("me/drive/items/a.txt/content".to_owned(), ctag)), "{guards:?}");
    // No eTag is known: the cTag of the content the two sides agreed on guards.
    let (folder, step) = stage(StagedStep::SendFileRemoval).await;
    let OneDriveStep::SendFileRemoval { mut item } = step else { unreachable!() };
    item.onedrive_etag = None;
    let ctag = match &item.synced_content {
        Some(konedrive_tree::ContentVersion::Hashed { ctag: Some(ctag), .. }) => ctag.clone(),
        other => panic!("a synced content with a cTag: {other:?}"),
    };
    folder.steps.run(OneDriveStep::SendFileRemoval { item }).await;
    assert_eq!(folder.graph.with(|cloud| cloud.guards.clone()), [("me/drive/items/a.txt".to_owned(), ctag)]);
    // Neither: nothing is sent.
    let (folder, step) = stage(StagedStep::SendFileRemoval).await;
    let OneDriveStep::SendFileRemoval { mut item } = step else { unreachable!() };
    (item.onedrive_etag, item.synced_content) = (None, None);
    let outcome = folder.run(OneDriveStep::SendFileRemoval { item }).await;
    assert!(matches!(outcome, StepOutcome::Failed(StepFailure::AnswerUnreadable { .. })), "{outcome:?}");
    assert_eq!((folder.count("DELETE", "items"), folder.records()), (0, vec![]));
}

/// What a step answers to a refusal OneDrive gave its one request, which was not carried
/// out. `None` where the status is no refusal for the case (a 404 of a removal confirms
/// it).
fn expected_refusal(staged: StagedStep, status: u16) -> Option<StepRefusal> {
    use StagedStep::*;
    let transient = || StepRefusal::Transient { detail: String::new() };
    Some(match (status, staged) {
        (423, _) => StepRefusal::Locked,
        (403, _) => StepRefusal::Forbidden,
        (400, _) => StepRefusal::RefusedForGood { message: "scripted 400".to_owned() },
        (429, _) => StepRefusal::Throttled { retry_after: Some(std::time::Duration::from_secs(3)) },
        // The token is rejected and the fake's token source has no other to give: the account is signed out.
        (401, _) => StepRefusal::SignedOut,
        (507, _) => StepRefusal::NoSpace { free_space: None },
        (404, MakeDirectory | SendEmptyNewFile) => StepRefusal::DirectoryGone,
        (404, SendMove | SendEmptyContent) => StepRefusal::ItemGone,
        (404, SendFileRemoval | SendDirectoryRemoval) => return None,
        // The item is read once: it is as it was planned from, and no earlier request of this step explains the refusal.
        (412, SendMove | SendEmptyContent | SendFileRemoval | SendDirectoryRemoval) => StepRefusal::ItemChanged,
        // A new item has no version to have changed: said as it is, and tried again.
        (412, MakeDirectory | SendEmptyNewFile) => transient(),
        // Nothing holds the name by now: tried again.
        (409, MakeDirectory | SendMove | SendFileRemoval | SendDirectoryRemoval) => transient(),
        (409, SendEmptyContent) => StepRefusal::ItemChanged,
        (409, SendEmptyNewFile) => return None,
        // An answer lost to a request that changed nothing: read, and tried again.
        (500, _) => transient(),
        _ => unreachable!("{status} of {staged:?}"),
    })
}

/// One test per case of a step, an answer per status: the refusal is answered as its
/// case, no row is written, nothing is left on record, and OneDrive is as it was.
#[tokio::test]
async fn a_refusal_is_answered_as_its_case_and_writes_no_row() {
    for staged in StagedStep::ALL {
        for status in [423, 403, 400, 429, 401, 507, 404, 412, 409, 500] {
            let Some(expected) = expected_refusal(staged, status) else { continue };
            let (folder, step) = stage(staged).await;
            let (method, fragment) = staged.request();
            match status {
                429 => folder.graph.with(|cloud| cloud.throttle_429(method, fragment, 0, 3, 1)),
                401 => folder.refuse(method, fragment, 401, 1),
                status => folder.refuse(method, fragment, status, 1),
            }
            let row_before = folder.fixture.find_item(step.item().local_item_id);
            let outcome = folder.run(step.clone()).await;
            match (&outcome, &expected) {
                (StepOutcome::Refused(StepRefusal::Transient { .. }), StepRefusal::Transient { .. }) => {}
                (StepOutcome::Refused(refusal), expected) => assert_eq!(refusal, expected, "{staged:?} answered {status}"),
                _ => panic!("{staged:?} answered {status}: {outcome:?}"),
            }
            assert_eq!(folder.fixture.find_item(step.item().local_item_id), row_before, "{staged:?} answered {status}: no row is written");
            assert_eq!(folder.records(), [], "{staged:?} answered {status}: the record goes with a certain refusal");
            assert_eq!(folder.cloud_paths(), unsent_paths(staged), "{staged:?} answered {status}");
        }
    }
}

#[tokio::test]
async fn a_404_of_a_removal_is_its_confirmation() {
    for staged in [StagedStep::SendFileRemoval, StagedStep::SendDirectoryRemoval] {
        let (folder, step) = stage(staged).await;
        folder.refuse(staged.request().0, staged.request().1, 404, 1);
        let outcome = folder.run(step.clone()).await;
        assert!(matches!(outcome, StepOutcome::Written { .. }), "{staged:?}: {outcome:?}");
        assert!(is_committed(&folder, staged, &step) && folder.records().is_empty(), "{staged:?}");
    }
}

/// The request went through and its answer was lost: the step reads OneDrive once, finds
/// what it asked, and commits it.
#[tokio::test]
async fn a_step_whose_answer_was_lost_reads_onedrive_once_and_commits_what_went_through() {
    for staged in StagedStep::ALL {
        let (folder, step) = stage(staged).await;
        folder.graph.with(|cloud| cloud.lose_answers(staged.request().0, staged.request().1, 1));
        let reads_before = folder.count("GET", staged.read());
        let outcome = folder.run(step.clone()).await;
        assert!(matches!(outcome, StepOutcome::Written { .. }), "{staged:?}: {outcome:?}");
        assert_eq!(folder.count("GET", staged.read()), reads_before + 1, "{staged:?}: one read");
        assert_eq!(folder.cloud_paths(), sent_paths(staged), "{staged:?}");
        assert!(is_committed(&folder, staged, &step) && folder.records().is_empty(), "{staged:?}");
        assert_eq!(folder.count(staged.request().0, staged.request().1), 1, "{staged:?}: nothing is sent twice");
    }
}

/// The request went through, its answer was lost, and the read that follows says nothing
/// certain either: the step ends `Transient` with its record standing; the record is
/// settled later by reading OneDrive, and the answer committed then.
#[tokio::test]
async fn a_step_whose_answer_was_lost_and_whose_read_failed_ends_transient_with_its_record_standing() {
    for staged in StagedStep::ALL {
        let (folder, step) = stage(staged).await;
        folder.graph.with(|cloud| cloud.lose_answers(staged.request().0, staged.request().1, 1));
        folder.refuse("GET", staged.read(), 500, 1);
        let outcome = folder.run(step.clone()).await;
        assert!(matches!(outcome, StepOutcome::Refused(StepRefusal::Transient { .. })), "{staged:?}: {outcome:?}");
        let records = folder.records();
        assert_eq!(records.iter().map(|record| record.item).collect::<Vec<_>>(), [step.item().local_item_id], "{staged:?}: the record stands");
        assert!(!is_committed(&folder, staged, &step), "{staged:?}: nothing is written yet");

        let round = folder.settle_records().await;
        assert_eq!((round.done_count, round.failed_count, round.left_to_listing_count), (1, 0, 0), "{staged:?}");
        assert!(is_committed(&folder, staged, &step) && folder.records().is_empty(), "{staged:?}: {:?}", folder.fixture.find_item(step.item().local_item_id));
        assert_eq!(folder.count(staged.request().0, staged.request().1), 1, "{staged:?}: nothing is sent twice");
    }
}

/// A stop took the step off the running steps while its request was in flight, and the
/// answer came all the same: the commit may not begin, nothing is written, and the record
/// stands for the next start to settle.
#[tokio::test]
async fn a_step_whose_commit_may_not_begin_writes_nothing() {
    for staged in StagedStep::ALL {
        let (folder, step) = stage(staged).await;
        let row_before = folder.fixture.find_item(step.item().local_item_id);
        let outcome = folder.run_stopped_before_its_commit(step.clone()).await;
        assert_eq!(outcome, StepOutcome::NotRun(StepObstacle::SendingStopped), "{staged:?}");
        assert_eq!(folder.fixture.find_item(step.item().local_item_id), row_before, "{staged:?}");
        assert_eq!(folder.cloud_paths(), sent_paths(staged), "{staged:?}: the request went through");
        assert_eq!(folder.records().len(), 1, "{staged:?}");

        // The next start: the record is settled by reading OneDrive.
        assert_eq!(folder.settle_records().await.done_count, 1, "{staged:?}");
        assert!(is_committed(&folder, staged, &step) && folder.records().is_empty(), "{staged:?}");
    }
}

/// The daemon died between the record and the request: the record stands over a request
/// that never left. Read, it did not go through: the record goes, nothing is written, and
/// the step runs again and sends.
#[tokio::test]
async fn a_record_whose_request_never_left_goes_and_the_step_is_sent_again() {
    for staged in StagedStep::ALL {
        let (folder, step) = stage(staged).await;
        let request = match &step {
            OneDriveStep::MakeDirectory { parent, name, .. } => RecordedRequest::MakeDirectory { parent: parent.clone(), name: name.clone() },
            OneDriveStep::SendNewFile { parent, name, .. } => RecordedRequest::SendNewFile { parent: parent.clone(), name: name.clone(), sent_hash: qx(b"") },
            OneDriveStep::SendContent { .. } => RecordedRequest::SendContent { sent_hash: qx(b"") },
            OneDriveStep::SendMove { parent, name, .. } => RecordedRequest::SendMove { parent: parent.clone(), name: name.clone() },
            OneDriveStep::SendFileRemoval { .. } => RecordedRequest::SendFileRemoval,
            OneDriveStep::SendDirectoryRemoval { .. } => RecordedRequest::SendDirectoryRemoval,
        };
        let record = RequestRecord { item: step.item().local_item_id, onedrive_item: step.item().onedrive_item.clone(), request, sent_at: folder.fixture.now.load(Ordering::SeqCst) };
        folder.fixture.write_rows(move |store| store.step_memory().remember_request_record(&record));
        let row_before = folder.fixture.find_item(step.item().local_item_id);

        assert_eq!(folder.settle_records().await.done_count, 1, "{staged:?}");
        assert_eq!((folder.records(), folder.fixture.find_item(step.item().local_item_id)), (vec![], row_before), "{staged:?}");
        assert_eq!(folder.cloud_paths(), unsent_paths(staged), "{staged:?}");

        let outcome = folder.run(step.clone()).await;
        assert!(matches!(outcome, StepOutcome::Written { .. }), "{staged:?}: {outcome:?}");
        assert_eq!(folder.cloud_paths(), sent_paths(staged), "{staged:?}");
    }
}

#[tokio::test]
async fn a_request_is_on_record_before_it_leaves_and_the_record_goes_with_its_commit_or_its_certain_refusal() {
    // The request is held in flight: the record is there while it is.
    let (folder, step) = stage(StagedStep::SendMove).await;
    let folder = Arc::new(folder);
    folder.graph.with(|cloud| cloud.delay("PATCH", "items/a.txt", std::time::Duration::from_secs(3600), 1));
    let item = step.item().local_item_id;
    let running = tokio::spawn({
        let (folder, step) = (Arc::clone(&folder), step.clone());
        async move { folder.run(step).await }
    });
    // The fake logs a request when it reaches it: the record was written before.
    while folder.count("PATCH", "items/a.txt") == 0 {
        tokio::task::yield_now().await;
    }
    let records = folder.records();
    assert_eq!(records.len(), 1);
    assert_eq!((records[0].item, records[0].onedrive_item.clone()), (item, Some(ItemFixture::onedrive_reference_of("a.txt"))));
    assert_eq!(records[0].request, RecordedRequest::SendMove { parent: StepsFolder::root(), name: "b.txt".into() });
    running.abort();
}

#[tokio::test]
async fn a_temporary_name_is_remembered_before_the_request_of_a_new_item_and_of_a_move() {
    // A directory made under a temporary name: the request is refused, so that the memory is read as it was before the request's answer.
    let (folder, step) = stage(StagedStep::MakeDirectory).await;
    let OneDriveStep::MakeDirectory { item, parent, .. } = step else { unreachable!() };
    folder.refuse("POST", "children", 423, 1);
    folder.run(OneDriveStep::MakeDirectory { item: item.clone(), parent, name: ".swap-1".into() }).await;
    let toward = item.local.as_ref().and_then(|local| local.place.clone()).expect("a local place");
    assert_eq!(folder.memory(item.local_item_id).temporary_name, Some(konedrive_tree::TemporaryName { name: ".swap-1".into(), toward }));

    let (folder, step) = stage(StagedStep::SendMove).await;
    let OneDriveStep::SendMove { item, parent, .. } = step else { unreachable!() };
    folder.refuse("PATCH", "items/a.txt", 423, 1);
    folder.run(OneDriveStep::SendMove { item: item.clone(), parent, name: ".swap-2".into() }).await;
    assert_eq!(folder.memory(item.local_item_id).temporary_name.map(|kept| kept.name), Some(".swap-2".into()));

    // The name of the local place is no temporary name.
    let (folder, step) = stage(StagedStep::SendMove).await;
    let item = step.item().local_item_id;
    folder.refuse("PATCH", "items/a.txt", 423, 1);
    folder.run(step).await;
    assert_eq!(folder.memory(item).temporary_name, None);
}

#[tokio::test]
async fn a_directory_of_the_name_that_no_row_is_bound_to_is_adopted_only_when_it_is_empty() {
    // The step's own earlier request made it: empty, and no row is bound to it.
    let (folder, step) = stage(StagedStep::MakeDirectory).await;
    folder.graph.with(|cloud| cloud.add(crate::fake_onedrive::FakeItem { id: "EARLIER".into(), parent: Some("R".into()), name: "new".into(), folder: true, content: Vec::new(), hash: None, size: 0, etag: "e-x".into(), ctag: "c-x".into(), mtime: 0 }));
    let outcome = folder.run(step.clone()).await;
    assert!(matches!(outcome, StepOutcome::Written { .. }), "{outcome:?}");
    assert_eq!(folder.fixture.find_item(step.item().local_item_id).and_then(|row| row.onedrive_item), Some(ItemFixture::onedrive_reference_of("EARLIER")));

    // Another client's directory of the same name, with a file in it no row knows.
    let (folder, step) = stage(StagedStep::MakeDirectory).await;
    folder.graph.with(|cloud| {
        cloud.add(crate::fake_onedrive::FakeItem { id: "THEIRS".into(), parent: Some("R".into()), name: "New".into(), folder: true, content: Vec::new(), hash: None, size: 0, etag: "e-x".into(), ctag: "c-x".into(), mtime: 0 });
        cloud.add_file("THEIRS!f", "THEIRS", "f.txt", b"theirs");
    });
    let place = step.item().local.as_ref().and_then(|local| local.place.clone()).expect("a local place");
    assert_eq!(folder.run(step.clone()).await, StepOutcome::Refused(StepRefusal::NameTaken { place, holder_name: "New".into() }));
    assert_eq!((folder.records(), folder.cloud_paths()), (vec![], vec!["New".to_owned(), "New/f.txt".to_owned()]));
}

#[tokio::test]
async fn a_move_whose_name_is_taken_answers_the_holder_as_onedrive_spells_it() {
    let (folder, step) = stage(StagedStep::SendMove).await;
    folder.graph.with(|cloud| cloud.add_file("HOLDER", "R", "B.TXT", b"another"));
    let place = step.item().local.as_ref().and_then(|local| local.place.clone()).expect("a local place");
    assert_eq!(folder.run(step).await, StepOutcome::Refused(StepRefusal::NameTaken { place, holder_name: "B.TXT".into() }));
    assert_eq!(folder.records(), []);
}

/// A 412 of a move whose earlier request went through: the item is there already under
/// the place sent, and is adopted.
#[tokio::test]
async fn a_step_replayed_after_a_death_between_its_request_and_its_commit_adopts_what_it_finds() {
    let (folder, step) = stage(StagedStep::SendMove).await;
    // The earlier request: the item is at the place sent, under another eTag than the step was planned from.
    folder.graph.with(|cloud| cloud.rename("a.txt", "R", "b.txt"));
    let outcome = folder.run(step.clone()).await;
    assert!(matches!(outcome, StepOutcome::Written { .. }), "{outcome:?}");
    assert!(is_committed(&folder, StagedStep::SendMove, &step) && folder.records().is_empty());

    // Moved by another, elsewhere: not adopted.
    let (folder, step) = stage(StagedStep::SendMove).await;
    folder.graph.with(|cloud| cloud.rename("a.txt", "R", "elsewhere.txt"));
    assert_eq!(folder.run(step).await, StepOutcome::Refused(StepRefusal::ItemChanged));
    assert_eq!(folder.records(), []);
}

/// A move with nothing to send — the item is there already, as the rows say — is read and
/// committed as it is.
#[tokio::test]
async fn a_move_to_where_the_rows_say_onedrive_has_the_item_sends_nothing() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("a.txt", b"old");
    let versions = fixture.versions_of(shown.local_item_id);
    let folder = StepsFolder::start(fixture).await;
    let outcome = folder.run(OneDriveStep::SendMove { item: versions, parent: StepsFolder::root(), name: "a.txt".into() }).await;
    assert!(matches!(outcome, StepOutcome::Written { .. }), "{outcome:?}");
    assert_eq!((folder.count("PATCH", "items"), folder.count("GET", "items/a.txt"), folder.records()), (0, 1, vec![]));
}

#[tokio::test]
async fn a_commit_that_finds_the_rows_changed_after_a_new_item_was_made_removes_it_when_the_row_is_gone() {
    let (folder, step) = stage(StagedStep::MakeDirectory).await;
    let folder = Arc::new(folder);
    let item = step.item().local_item_id;
    // The directory is removed here while its request is in flight.
    folder.graph.with(|cloud| cloud.delay("POST", "children", std::time::Duration::from_millis(300), 1));
    let running = tokio::spawn({
        let (folder, step) = (Arc::clone(&folder), step.clone());
        async move { folder.run(step).await }
    });
    while folder.count("POST", "children") == 0 {
        tokio::task::yield_now().await;
    }
    std::fs::remove_dir(folder.fixture.disk_path("new")).expect("the directory is removed from disk");
    folder.fixture.write_rows(move |store| store.item_store().apply(&konedrive_tree::LocalVersionChange::Removed { item }));

    let outcome = running.await.expect("the step's task");
    assert!(matches!(outcome, StepOutcome::RowsChanged(RowsChangedFollowUp::MadeItemRemoved(_))), "{outcome:?}");
    assert_eq!((folder.cloud_paths(), folder.records()), (Vec::<String>::new(), vec![]));
}

#[tokio::test]
async fn a_commit_that_finds_the_rows_changed_otherwise_asks_for_a_cycle() {
    let (folder, step) = stage(StagedStep::SendMove).await;
    // A listing wrote another version of the item while the step was planned: the commit finds the rows changed.
    folder.fixture.list_onedrive_file("a.txt", b"changed in OneDrive");
    // The fake still has the eTag the step was planned from: the request goes through.
    assert_eq!(folder.run(step).await, StepOutcome::RowsChanged(RowsChangedFollowUp::CycleWanted));
    assert_eq!((folder.cloud_paths(), folder.records()), (vec!["b.txt".to_owned()], vec![]));
}

#[tokio::test]
async fn a_step_that_names_an_item_of_another_drive_sends_nothing() {
    let (folder, step) = stage(StagedStep::MakeDirectory).await;
    let OneDriveStep::MakeDirectory { item, name, .. } = step else { unreachable!() };
    let elsewhere = OneDriveItemReference { drive_id: konedrive_tree::DriveId::new("another").expect("a drive id"), item_id: konedrive_tree::OneDriveItemId::new("R") };
    let outcome = folder.run(OneDriveStep::MakeDirectory { item, parent: elsewhere, name }).await;
    assert!(matches!(outcome, StepOutcome::Failed(StepFailure::AnswerUnreadable { .. })), "{outcome:?}");
    assert_eq!((folder.count("POST", "children"), folder.records()), (0, vec![]));
}

/// The directory removal's check before its request leaves: a request on record that
/// sends an item into the directory, and a step that runs for an item into it, hold the
/// removal back; nothing is sent, and no record is written.
#[tokio::test]
async fn a_directorys_removal_is_not_sent_while_a_record_names_an_item_below_it_or_a_destination_in_it() {
    // A record of another item whose request sends into the directory: its answer was lost.
    let (folder, step) = stage(StagedStep::SendDirectoryRemoval).await;
    let other = folder.fixture.make_own_file("other.txt", b"x").local_item_id;
    let record = RequestRecord { item: other, onedrive_item: None, request: RecordedRequest::SendMove { parent: ItemFixture::onedrive_reference_of("docs"), name: "x".into() }, sent_at: 1 };
    folder.fixture.write_rows(move |store| store.step_memory().remember_request_record(&record));
    assert_eq!(folder.run(step.clone()).await, StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing));
    assert_eq!((folder.count("DELETE", "items"), folder.records().len(), folder.cloud_paths()), (0, 1, vec!["docs".to_owned(), "docs/c.txt".to_owned()]));

    // The record settled, the removal is sent.
    folder.fixture.write_rows(move |store| store.step_memory().forget_request_record(other));
    assert!(matches!(folder.run(step).await, StepOutcome::Written { .. }));

    // A running step that sends an item into the directory.
    let (folder, step) = stage(StagedStep::SendDirectoryRemoval).await;
    let other = folder.fixture.make_own_file("other.txt", b"x").local_item_id;
    let into_the_directory = crate::sync_parts::running_steps::RunningStep { class: crate::sync_parts::running_steps::StepClass::Metadata, item: other, other_items: Vec::new(), destination: Some(ItemFixture::onedrive_reference_of("docs")), sent: 0, total: 0 };
    assert!(folder.fixture.running_steps.begin(into_the_directory));
    assert_eq!(folder.run(step.clone()).await, StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing));
    assert_eq!((folder.count("DELETE", "items"), folder.records()), (0, vec![]));
    folder.fixture.running_steps.end(other);
    assert!(matches!(folder.run(step).await, StepOutcome::Written { .. }));
}

/// The rows changed since the removal was planned — a listing put a file into the
/// directory —: nothing is sent.
#[tokio::test]
async fn a_directorys_removal_is_not_sent_once_the_rows_say_onedrive_has_more_in_it() {
    let (folder, step) = stage(StagedStep::SendDirectoryRemoval).await;
    folder.fixture.list_onedrive_file("docs/new-in-onedrive.txt", b"n");
    assert_eq!(folder.run(step).await, StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing));
    assert_eq!((folder.count("DELETE", "items"), folder.records()), (0, vec![]));
}

#[tokio::test]
async fn no_request_is_sent_while_the_tree_lock_is_held() {
    // The lock is held by another: the request leaves all the same, and the step waits for the lock only at its commit.
    let (folder, step) = stage(StagedStep::SendMove).await;
    let folder = Arc::new(folder);
    let held = folder.fixture.tree_lock.clone().lock_owned().await;
    let running = tokio::spawn({
        let (folder, step) = (Arc::clone(&folder), step.clone());
        async move { folder.run(step).await }
    });
    while folder.cloud_paths() != ["b.txt"] {
        tokio::task::yield_now().await;
    }
    assert!(!is_committed(&folder, StagedStep::SendMove, &step), "the commit waits for the lock");
    drop(held);
    assert!(matches!(running.await.expect("the step's task"), StepOutcome::Written { .. }));
    assert!(is_committed(&folder, StagedStep::SendMove, &step));
}

#[tokio::test]
async fn a_step_that_sends_no_content_holds_a_metadata_slot_for_its_request() {
    use konedrive_graph::pool::Class;
    let (folder, step) = stage(StagedStep::SendMove).await;
    let folder = Arc::new(folder);
    let pool = Arc::clone(&folder.pool);
    folder.graph.with(|cloud| cloud.delay("PATCH", "items/a.txt", std::time::Duration::from_millis(300), 1));
    let running = tokio::spawn({
        let (folder, step) = (Arc::clone(&folder), step.clone());
        async move { folder.run(step).await }
    });
    while folder.count("PATCH", "items/a.txt") == 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(pool.held(Class::Metadata), 1, "held while the request is in flight");
    assert!(matches!(running.await.expect("the step's task"), StepOutcome::Written { .. }));
    assert_eq!(pool.held(Class::Metadata), 0, "let go with the answer");
}
