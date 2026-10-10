//! What came of one step of the sync plan: the one answer every runner of steps gives —
//! towards OneDrive, towards the folder, of the rows alone — and the plan's task settles.
//!
//! **State owned:** none: values. It is here, below the runners, because the runner
//! towards the folder (`fuse`), the one towards OneDrive (`upload`) and the task that
//! reads their answers (`sync`) must name one type, and none of them may name the others
//! above it.
//!
//! **Answers for:** design §7: a step that is done says so with its commit; a step whose
//! commit found other rows than it was planned from changed nothing; a step OneDrive
//! refuses decides nothing, and says which refusal it was.
//!
//! **Tested without the rest:** nothing to test: no function decides here.
#![allow(dead_code, unused_variables)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use konedrive_tree::{ActivityKind, ItemPlace, OneDriveItemReference};

/// What came of a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// The step is done and its commit wrote the rows: what happened, for the activity
    /// log.
    Written { events: Vec<StepEvent> },
    /// The commit found the rows changed and wrote nothing; what the runner did then.
    /// Never a fault, and never tried again with the same versions: the next plan decides.
    RowsChanged(RowsChangedFollowUp),
    /// OneDrive refused the request.
    Refused(StepRefusal),
    /// The step did not run to its request or its change of the folder, for a cause on
    /// this computer that is no failure.
    NotRun(StepObstacle),
    /// The disk or the store failed.
    Failed(StepFailure),
}

/// What a runner did after its commit answered that the rows changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowsChangedFollowUp {
    /// Nothing was to undo, or the change of the backing directory was undone.
    Nothing,
    /// OneDrive did change and the rows do not say so: a listing cycle is asked for, and
    /// brings it (a sent move, content, removal; a new item whose row is still there).
    CycleWanted,
    /// The item's row is gone, and what the step had made in OneDrive was removed there
    /// again: which item of OneDrive.
    MadeItemRemoved(OneDriveItemReference),
    /// The row is gone and what was made could not be removed: it stays in OneDrive, and
    /// a listing brings it as an item of OneDrive's.
    MadeItemLeft(OneDriveItemReference),
}

/// A refusal of OneDrive, by what the plan's task does about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepRefusal {
    /// 409: another item has the name at `place`, spelt `holder_name` there. Kept as a
    /// taken name; a listing cycle is asked for.
    NameTaken { place: ItemPlace, holder_name: OsString },
    /// 409: the empty file of an upload session holds the name, and it is not provably
    /// this folder's own: the step waits for it (design §7's first exception).
    NameHeldByUploadSession,
    /// 412: the item changed in OneDrive since the versions the step was planned from. A
    /// listing cycle is asked for.
    ItemChanged,
    /// 404 where the item itself was asked for: OneDrive does not have it. A listing
    /// cycle is asked for.
    ItemGone,
    /// 404 for the directory sent into. A listing cycle is asked for.
    DirectoryGone,
    /// 507: no space for this content. `free_space` is what the quota read after it said,
    /// when it could be read: with space left, the file alone is too big.
    NoSpace { free_space: Option<u64> },
    /// 429 or 503: all sending waits, until the time OneDrive named or one chosen.
    Throttled { retry_after: Option<Duration> },
    /// 423: the item is locked in OneDrive. Tried again later.
    Locked,
    /// 403: not allowed. Refused for good as this version, until a sign-in.
    Forbidden,
    /// 400: refused for good as this version, with OneDrive's message.
    RefusedForGood { message: String },
    /// The account is signed out: all sending waits for the sign-in.
    SignedOut,
    /// The upload ended with other content in OneDrive than was sent. For a new file the
    /// item it made was remembered and its removal asked; for an existing item the fact
    /// was kept (`OwnUploadContent`). A listing cycle is asked for either way.
    ContentMismatch,
    /// The upload session is gone in OneDrive and the content is not there: one new
    /// session, later.
    UploadSessionGone,
    /// The network, or an answer that says nothing certain: tried again later.
    Transient { detail: String },
}

/// Why a step did not run, on this computer. The first two end by themselves and write
/// nothing; the other three do not change by themselves, and count a failed try, so that
/// the same step is not planned again at once for good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepObstacle {
    /// A program opened the file for writing since the plan was made: its close wakes the
    /// plan.
    OpenForWriting,
    /// The sending as a whole was stopped between two fragments, or the step's job was
    /// dropped at a stop: the session is kept, and the end of the hold wakes the plan.
    SendingStopped,
    /// The file on disk has not the size and the time of the content the step was planned
    /// from, and no writer holds it: seen before the request, or between two fragments.
    ContentChanged,
    /// The backing file the row says is on disk is not there, or is not readable.
    BackingFileMissing,
    /// The place the folder is to take is held on disk by what no row says.
    PlaceHeldOnDisk { path: PathBuf },
}

/// A failure that is neither OneDrive's refusal nor a changed row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepFailure {
    /// The backing directory: the errno and where.
    Disk { errno: i32, path: PathBuf },
    /// The store.
    Store { detail: String },
    /// A change of the backing directory could not be undone after its commit was
    /// refused: the mount is taken down, and the folder starts over through the check at
    /// start (design §8).
    UndoFailed { path: PathBuf },
    /// Graph answered what cannot be read as the item asked for.
    AnswerUnreadable { detail: String },
}

/// One thing a step did, as the activity log says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepEvent {
    pub kind: ActivityKind,
    /// Relative to the folder's root, as the rows said it when the step was committed.
    pub path: PathBuf,
    /// For a move: where it was.
    pub former_path: Option<PathBuf>,
}

/// Why a part of a step did not get to its end, when it is one of the two that are no
/// refusal of OneDrive: what opening or reading a local content answers, and what a step
/// turns into its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepTrouble {
    /// Not a failure of the daemon: an obstacle the step answers with.
    Obstacle(StepObstacle),
    Failure(StepFailure),
}
