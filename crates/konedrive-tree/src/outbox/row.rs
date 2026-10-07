//! A row of the outbox, a detection, and what an examination and a commit hand the store.

use std::path::PathBuf;

use konedrive_fs::handle::FileHandle;

use super::{LocalSkip, Reason, Snapshot};
use crate::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OutboxKind {
    Create,
    Mkdir,
    Update,
    Move,
    Delete,
    MoveOut,
}

impl OutboxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Mkdir => "mkdir",
            Self::Update => "update",
            Self::Move => "move",
            Self::Delete => "delete",
            Self::MoveOut => "move-out",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Create, Self::Mkdir, Self::Update, Self::Move, Self::Delete, Self::MoveOut].into_iter().find(|k| k.as_str() == value)
    }

    /// Whether the row ends with the item gone from OneDrive.
    pub fn removes(self) -> bool {
        matches!(self, Self::Delete | Self::MoveOut)
    }

    /// Whether the row sends content (`mkdir`, `move` and `delete` are
    /// metadata rows, run one at a time, `docs/design/writes.md` §5.3).
    pub fn sends_content(self) -> bool {
        matches!(self, Self::Create | Self::Update)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutboxState {
    /// Not quiet: open for writing somewhere; examined again later.
    Waiting,
    Ready,
    Running,
    /// Failed; tried again at `next_try`.
    Retry,
    /// Needs the user: a name OneDrive refuses, too large. A full OneDrive
    /// blocks nothing: its rows stay `Ready` with a reason.
    Blocked,
    /// Held by the mass-delete guard until confirmed.
    Held,
}

impl OutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Retry => "retry",
            Self::Blocked => "blocked",
            Self::Held => "held",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Waiting, Self::Ready, Self::Running, Self::Retry, Self::Blocked, Self::Held].into_iter().find(|s| s.as_str() == value)
    }
}

/// The local object a row is about. The handle, where the filesystem gives
/// one, is the identity; the inode number is kept beside it, as the schema
/// has it, and is the identity only where there is no handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inode {
    pub dev: u64,
    pub ino: u64,
    pub handle: Option<FileHandle>,
}

impl Inode {
    pub fn same_object(&self, other: &Inode) -> bool {
        match (&self.handle, &other.handle) {
            (Some(a), Some(b)) => a == b,
            _ => self.dev == other.dev && self.ino == other.ino,
        }
    }
}

/// What a change was made against: the item as the base had it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Base {
    /// `None` when the local content derives from another version than the
    /// base's (a download not yet replaced): the cTag is the guard then.
    pub etag: Option<String>,
    pub ctag: Option<String>,
    pub parent: Option<String>,
    pub name: Option<String>,
}

/// The item a new file's upload left in OneDrive with other content than
/// was sent, as the upload's answer gave it: what says later whether the
/// item is still that upload, or was changed by someone since.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BadItem {
    pub id: String,
    /// The answer's cTag: the item is still the bad upload while its
    /// content tag is this one, whatever became of its eTag.
    pub ctag: Option<String>,
    /// The answer's eTag, kept only when it carried no cTag.
    pub etag: Option<String>,
}

impl BadItem {
    /// What the upload's answer says of the item: its cTag, or its eTag
    /// when it has no cTag.
    pub fn answered(id: &str, ctag: Option<&str>, etag: Option<&str>) -> Self {
        Self { id: id.to_owned(), ctag: ctag.map(str::to_owned), etag: if ctag.is_some() { None } else { etag.map(str::to_owned) } }
    }

    /// Whether an item with these tags now is still the bad upload. With no
    /// tag remembered (a row an older version wrote, an answer that carried
    /// none) nothing says it is not.
    pub fn still(&self, ctag: Option<&str>, etag: Option<&str>) -> bool {
        match (&self.ctag, &self.etag) {
            (Some(kept), _) => ctag == Some(kept.as_str()),
            (None, Some(kept)) => etag == Some(kept.as_str()),
            (None, None) => true,
        }
    }
}

/// An upload session's URL: a bearer credential until it expires, so never
/// logged and never published. Its `Debug` says only that there is one;
/// whoever sends it to OneDrive, or stores it, asks for the text by name
/// ([`as_str`](Self::as_str)).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SessionUrl(String);

impl SessionUrl {
    pub fn new(url: impl Into<String>) -> Self {
        Self(url.into())
    }

    /// The URL itself: for the request, and for the store.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SessionUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionUrl(..)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub seq: i64,
    pub kind: OutboxKind,
    /// `None` until a `create` or `mkdir` lands.
    pub item_id: Option<String>,
    pub inode: Option<Inode>,
    /// Where it was last seen, relative to the root.
    pub rel: PathBuf,
    pub base: Option<Base>,
    /// The parent's item id: `None` while the parent is a `mkdir` still to
    /// land (the row waits for it, and finds the id by `rel` then).
    pub target_parent: Option<String>,
    /// The name the item takes; a temporary one on its way there
    /// ([`OutboxRow::swap_name`]); for a `move-out` row, where its object
    /// was last proved to be ([`OutboxRow::last_place`]).
    pub target_name: Option<String>,
    pub state: OutboxState,
    pub reason: Option<Reason>,
    pub attempts: u32,
    pub next_try: Option<i64>,
    /// What the row holds of the content it sends: its size and time, or
    /// a `move-out` row's marker.
    pub snapshot: Option<Snapshot>,
    pub session_url: Option<SessionUrl>,
    pub session_expires: Option<i64>,
    pub session_next: Option<u64>,
    /// A removal the user confirmed through the mass-delete guard: never
    /// counted or held again.
    pub confirmed: bool,
    /// The file's size when the change was detected: what the counts and
    /// sums say until the row's snapshot does. `None` for a row written
    /// before it was recorded, and for what sends no content.
    pub size: Option<u64>,
}

impl OutboxRow {
    /// The (parent, name) the row takes the item to, as the base's pair is compared.
    pub(super) fn target(&self) -> (Option<&str>, Option<&str>) {
        (self.target_parent.as_deref(), self.target_name.as_deref())
    }

    /// The row's reason as it is stored.
    pub fn reason_text(&self) -> Option<String> {
        self.reason.as_ref().map(Reason::to_string)
    }

    /// The row starts over as another request than the one it was sending
    /// (a conflict copy's, a file uploaded again as new): no reason, no
    /// attempt counted, due at once, and nothing kept of the content that
    /// was going up — its snapshot and its upload session.
    pub fn reset_for_resend(&mut self) {
        self.reason = None;
        self.attempts = 0;
        self.next_try = None;
        self.snapshot = None;
        self.session_url = None;
        self.session_expires = None;
        self.session_next = None;
    }
}

/// What an examination found about one item, or one local object not
/// uploaded yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub kind: OutboxKind,
    pub item_id: Option<String>,
    pub inode: Option<Inode>,
    pub rel: PathBuf,
    /// `None` for a `create` or a `mkdir`.
    pub base: Option<Base>,
    pub target_parent: Option<String>,
    pub target_name: Option<String>,
    /// For a `move`: the content was checked and is the base's. A `move` is
    /// also how an examination says "the item is here": at its base place
    /// with the same content it removes a pending update, a pending delete
    /// of it, or a move.
    pub same_content: bool,
    pub state: OutboxState,
    pub reason: Option<Reason>,
    pub next_try: Option<i64>,
    /// The file's size as the examination saw it.
    pub size: Option<u64>,
}

impl Detection {
    pub(super) fn target(&self) -> (Option<&str>, Option<&str>) {
        (self.target_parent.as_deref(), self.target_name.as_deref())
    }

    pub(super) fn at_base(&self, base: Option<&Base>) -> bool {
        base.is_some_and(|b| self.target_parent.is_some() && (b.parent.as_deref(), b.name.as_deref()) == self.target())
    }
}

/// What recording a detection did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    Inserted(i64),
    Merged(i64),
    /// The detection cancelled the row: a create deleted before it was
    /// sent, a move back to where the base has it.
    Removed(i64),
    Nothing,
}

/// One step of an examination's result, applied with the others in one
/// transaction ([`TreeStore::outbox_apply`](crate::TreeStore::outbox_apply)).
// Built once per examination and consumed at once: not worth a box.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxOp {
    Record(Detection),
    /// Rows under `from` are now under `to`: a directory they are in moved.
    Rebase { from: PathBuf, to: PathBuf },
    Remove(i64),
    /// The inode the item is now (a scan's refresh).
    SetHandle { item_id: String, handle: Option<FileHandle> },
    /// Something never uploaded, listed under "Not uploaded" (§4.2 rule 2),
    /// with its size when it is a file.
    Skip { rel: PathBuf, reason: LocalSkip, size: u64 },
    Unskip(PathBuf),
    /// Lines of the skipped list, each at its first place, are at the second
    /// now: a directory the examination found renamed took them along.
    MoveSkipped(Vec<(PathBuf, PathBuf)>),
    /// The mass-delete guard holds a removal already waiting (unless it
    /// runs already).
    Hold { seq: i64, reason: Reason },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxApplied {
    /// Rows inserted or merged into, by `seq`.
    pub queued: Vec<i64>,
    /// Rows removed.
    pub removed: Vec<i64>,
}

/// A committed row's answer from OneDrive.
#[derive(Debug, Clone, Copy)]
pub enum Committed<'a> {
    /// The item as Graph answered, and the local object it now is.
    Item { row: &'a Row, handle: Option<&'a FileHandle> },
    /// Deleted in OneDrive (to its recycle bin).
    Gone { item_id: &'a str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSkipped {
    pub rel: PathBuf,
    pub reason: LocalSkip,
    /// When it was first listed, unix seconds.
    pub at: i64,
}
