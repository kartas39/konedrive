//! Why a change is kept back: the [`Reason`] an unsent item waits or is
//! refused for (what the sync plan answers, and what the steps remember of a
//! failed try and of a refusal), the [`LocalSkip`] of what is never uploaded,
//! and what an item that cannot stay in the folder still [`WaitsFor`].
//!
//! Each is stored, and sent over D-Bus, as a string: its **key**, and for
//! some a **detail** behind it — `<key>: <detail>` (`refused: <the service's
//! message>`, `download-failed: errno 5`), or `too-big:<needs>:<free>`. The
//! spellings are the contract with the database and with the clients (the
//! window's and Dolphin's tables are keyed by them): [`Reason::key`] and
//! [`LocalSkip::key`] are the one place they are written.
//!
//! A string the tables do not know — an older version's, a damaged one — is
//! kept as it is ([`Reason::Other`], [`LocalSkip::Other`]) and written back
//! unchanged.
//!
//! Every key has its [`Group`]: what the user can do about it
//! (`NotUploadedSummary()`).
//!
//! A crate of its own, with no dependencies: the store (`konedrive-tree`,
//! which re-exports them) and the daemon hold these, and
//! `konedrivectl` words them without linking the store.

use std::fmt;

/// What the user can do about a reason, in the order the window shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// One action fixes every file of the reason: OneDrive full, a sign-in
    /// that does not allow writes.
    OneAction,
    /// Each file needs the user: a name OneDrive refuses, a file too large,
    /// refused by OneDrive with a message.
    PerFile,
    /// Never uploaded, and nothing to do: symbolic links, pipes, another device.
    Never,
    /// Goes up by itself.
    Waiting,
}

impl Group {
    pub const ALL: [Group; 4] = [Self::OneAction, Self::PerFile, Self::Never, Self::Waiting];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneAction => "one-action",
            Self::PerFile => "per-file",
            Self::Never => "never",
            Self::Waiting => "waiting",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|g| g.as_str() == value)
    }
}

/// Why an unsent item is not sent now: what the sync plan answers of it, and
/// what the settlement of a step keeps of a failed try or a refusal.
///
/// A variant with an `Option<String>` is stored `<key>: <detail>` when it
/// has one, and as the bare key when not.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Reason {
    /// A writer has the file open (`docs/design/writes.md` §4.3).
    OpenForWriting,
    /// The name holds one of `" * : < > ? \ |` (§4.4).
    NameCharacters,
    /// The name starts or ends with a space.
    NameSpaces,
    /// A name OneDrive reserves.
    NameReserved,
    /// Linux allows the name; JSON cannot carry it.
    NameNotUtf8,
    /// Larger than OneDrive takes.
    TooLarge,
    /// OneDrive is full (`507`, `quotaLimitReached`): what earlier versions
    /// blocked a row with. Such rows wait for space now.
    Quota,
    /// `403`: OneDrive does not allow this change. Blocked until a worker
    /// begins anew, as it does after a sign-in.
    Forbidden,
    /// `400`: `refused: <the service's message>`. Blocked.
    Refused(Option<String>),
    /// `423`: locked, most likely open for co-authoring.
    Locked,
    /// The local object is not where the row saw it: the examination
    /// catches up.
    NotFound,
    /// Its size or time moved while it was being sent (§6.3).
    Changed,
    /// The folder it goes into is not in OneDrive (yet, or any more).
    Parent,
    /// OneDrive holds other content than was sent: sent again from zero.
    Hash,
    /// The account is paused: an upload in fragments stopped after the
    /// fragment it was sending, its session kept. Waiting, never a failure.
    Paused,
    /// A new file's name is held in OneDrive by the empty placeholder of an
    /// upload session of this folder. Tried again later.
    SessionOpen,
    /// The row's name in OneDrive is held by an empty file the delta feed
    /// never listed: an upload session's placeholder. Tried
    /// again later.
    NameHeld,
    /// The item changed in OneDrive each time its removal was sent: in
    /// backoff.
    ChangedAgain,
    /// The upload session ended under the upload twice in one run: in
    /// backoff.
    SessionEnded,
    /// The write gate closed between two fragments: `not allowed now:
    /// <why>`. Waiting until it opens; the session is kept.
    NotAllowed(Option<String>),
    /// What a blocked row with no reason is listed under.
    Blocked,
    /// The check of the folder on disk at the daemon's start could not read the item, and
    /// left it as it found it: nothing of it is sent until a start's check decides it.
    NotReadOnDisk,
    /// The sync plan cannot judge the item from what the store says of it: the daemon
    /// asks OneDrive for a full listing by itself, and the item waits for it.
    BeingChecked,
    /// The file's content was changed a moment ago: it is sent once its quiet time has
    /// ended, so that a program that saves in several writes is not sent half-way.
    QuietTime,
    /// OneDrive could not be reached. In backoff; the error's
    /// own text is in the journal only.
    Network,
    /// The local file could not be read or written. In backoff.
    LocalIo,
    /// The daemon's own index (the store) failed. In backoff.
    Store,
    /// Any other failure of a step. In backoff.
    Failed,
    /// Refused while OneDrive is full: ready, in its place, not taken until
    /// a quota read lets it go.
    WaitingForSpace,
    /// Refused while space is left: `too-big:<needs>:<free>`, in bytes.
    /// With no sizes it is only the key every such reason is summed under
    /// (`NotUploadedSummary()`), which no row holds.
    TooBig(Option<(u64, u64)>),
    /// A string no variant spells: kept and written back as it is.
    Other(String),
}

/// How every *too big* reason begins, the sizes behind it.
pub const TOO_BIG_PREFIX: &str = "too-big:";
/// The key every *too big* reason is summed under.
const TOO_BIG_KEY: &str = "too-big";

impl Reason {
    /// Every variant but [`Reason::Other`], those with a detail without it.
    pub const ALL: [Reason; 30] = [
        Self::OpenForWriting,
        Self::NameCharacters,
        Self::NameSpaces,
        Self::NameReserved,
        Self::NameNotUtf8,
        Self::TooLarge,
        Self::Quota,
        Self::Forbidden,
        Self::Refused(None),
        Self::Locked,
        Self::NotFound,
        Self::Changed,
        Self::Parent,
        Self::Hash,
        Self::Paused,
        Self::SessionOpen,
        Self::NameHeld,
        Self::ChangedAgain,
        Self::SessionEnded,
        Self::NotAllowed(None),
        Self::Blocked,
        Self::NotReadOnDisk,
        Self::BeingChecked,
        Self::QuietTime,
        Self::Network,
        Self::LocalIo,
        Self::Store,
        Self::Failed,
        Self::WaitingForSpace,
        Self::TooBig(None),
    ];

    /// The stored spelling of the reason without its detail: what it is
    /// summed under, and what the clients' tables are keyed by. For
    /// [`Reason::Other`], what [`key_of`] makes of the string.
    pub fn key(&self) -> &str {
        match self {
            Self::OpenForWriting => "open-for-writing",
            Self::NameCharacters => "name-characters",
            Self::NameSpaces => "name-spaces",
            Self::NameReserved => "name-reserved",
            Self::NameNotUtf8 => "name-not-utf8",
            Self::TooLarge => "too-large",
            Self::Quota => "quota-exceeded",
            Self::Forbidden => "forbidden",
            Self::Refused(_) => "refused",
            Self::Locked => "locked",
            Self::NotFound => "not-found",
            Self::Changed => "changed-while-sending",
            Self::Parent => "parent-not-in-onedrive",
            Self::Hash => "hash-mismatch",
            Self::Paused => "paused",
            Self::SessionOpen => "upload-session-open",
            Self::NameHeld => "name-held-by-an-upload",
            Self::ChangedAgain => "changed in OneDrive again and again",
            Self::SessionEnded => "the upload session ended twice",
            Self::NotAllowed(_) => "not allowed now",
            Self::Blocked => "blocked",
            Self::NotReadOnDisk => "not-read-on-disk",
            Self::BeingChecked => "being-checked",
            Self::QuietTime => "quiet-time",
            Self::Network => "network",
            Self::LocalIo => "local-error",
            Self::Store => "index-error",
            Self::Failed => "upload-error",
            Self::WaitingForSpace => "waiting-for-space",
            Self::TooBig(_) => TOO_BIG_KEY,
            Self::Other(stored) => key_of(stored),
        }
    }

    /// The part behind the key: a message, an error, an errno, the sizes
    /// (`<needs>:<free>`).
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Refused(detail)
            | Self::NotAllowed(detail) => detail.clone(),
            Self::TooBig(sizes) => sizes.map(|(needs, free)| format!("{needs}:{free}")),
            Self::Other(stored) => detail_of(stored),
            _ => None,
        }
    }

    /// The same reason with `detail` behind it, for a variant that carries
    /// one as `<key>: <detail>`.
    fn with(self, detail: &str) -> Option<Self> {
        let detail = Some(detail.to_owned());
        Some(match self {
            Self::Refused(_) => Self::Refused(detail),
            Self::NotAllowed(_) => Self::NotAllowed(detail),
            _ => return None,
        })
    }

    /// The variant spelled exactly `key`, without a detail.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.iter().find(|reason| reason.key() == key).cloned()
    }

    /// The reason a stored string spells. Writing it back
    /// ([`fmt::Display`]) gives the same string, byte for byte: what would
    /// not is [`Reason::Other`].
    pub fn parse(stored: &str) -> Self {
        if let Some(reason) = Self::from_key(stored) {
            return reason;
        }
        if let Some(sizes) = too_big_sizes(stored) {
            let reason = Self::TooBig(Some(sizes));
            if reason.to_string() == stored {
                return reason;
            }
        }
        stored
            .split_once(": ")
            .and_then(|(key, detail)| Self::from_key(key)?.with(detail))
            .unwrap_or_else(|| Self::Other(stored.to_owned()))
    }

    /// The group of a row kept back for this reason, by the reason alone
    /// (a blocked row is never [`Group::Waiting`]: the daemon's
    /// `kept_back::group_of`). `None` for a string in no table.
    pub fn group(&self) -> Option<Group> {
        Some(match self {
            // `quota-exceeded` only until a start converts it to `waiting-for-space`.
            Self::Quota | Self::WaitingForSpace | Self::TooBig(_) | Self::Forbidden => Group::OneAction,
            Self::NameCharacters | Self::NameSpaces | Self::NameReserved | Self::NameNotUtf8 | Self::TooLarge | Self::Refused(_) => {
                Group::PerFile
            }
            // What the worker blocks a row with beside those: the row itself, or the
            // file's state, is not what a step can work with. `Blocked`: no reason at all.
            Self::NotReadOnDisk
            | Self::Blocked => Group::PerFile,
            Self::OpenForWriting
            | Self::BeingChecked
            | Self::QuietTime
            | Self::Locked
            | Self::NotFound
            | Self::Changed
            | Self::Parent
            | Self::Hash
            | Self::Network
            | Self::LocalIo
            | Self::Store
            | Self::Failed
            | Self::Paused
            | Self::SessionOpen
            | Self::NameHeld
            | Self::ChangedAgain
            | Self::SessionEnded
            | Self::NotAllowed(_) => Group::Waiting,
            Self::Other(stored) => return known_group(key_of(stored)),
        })
    }

    /// `(needs, free)` of a *too big* reason.
    pub fn sizes(&self) -> Option<(u64, u64)> {
        match self {
            Self::TooBig(sizes) => *sizes,
            Self::Other(stored) => too_big_sizes(stored),
            _ => None,
        }
    }

    /// Whether a row with this reason waits for space: not taken until a
    /// quota read lets it go.
    pub fn waits_for_space(&self) -> bool {
        match self {
            Self::WaitingForSpace | Self::TooBig(Some(_)) => true,
            Self::Other(stored) => stored.starts_with(TOO_BIG_PREFIX),
            _ => false,
        }
    }

    /// Whether the stored string is empty: a row that says nothing.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Other(stored) if stored.is_empty())
    }
}

impl fmt::Display for Reason {
    /// The stored spelling, whole.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(stored) => f.write_str(stored),
            Self::TooBig(Some((needs, free))) => write!(f, "{TOO_BIG_PREFIX}{needs}:{free}"),
            _ => match self.detail() {
                Some(detail) => write!(f, "{}: {detail}", self.key()),
                None => f.write_str(self.key()),
            },
        }
    }
}

impl From<&str> for Reason {
    fn from(stored: &str) -> Self {
        Self::parse(stored)
    }
}

impl From<String> for Reason {
    fn from(stored: String) -> Self {
        Self::parse(&stored)
    }
}

/// Why an examination never uploads something (`local_skipped`, §4.2 rule 2).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LocalSkip {
    Symlink,
    Fifo,
    Socket,
    Device,
    /// A `.konedrive-` name, which the daemon keeps for itself.
    ReservedName,
    /// Another name of an object OneDrive can hold only once.
    HardLink,
    /// A file from another OneDrive folder, or a copy, that is not
    /// downloaded here: it cannot be read to be uploaded as new.
    NotDownloaded,
    /// On another device than the folder (a nested Btrfs subvolume, a
    /// mount): never uploaded (F72).
    OtherDevice,
    /// The daemon is refused to read it (another user's file, a directory
    /// closed to it), or reading it failed: not examined, so neither it nor
    /// anything inside it is uploaded until it can be read.
    Unreadable,
    /// A file that carries an item id and no state konedrive can read: it
    /// is left alone.
    BadState,
    /// In the table of groups, written by no code.
    Ignored,
    /// A string no variant spells: kept and written back as it is.
    Other(String),
}

impl LocalSkip {
    /// Every variant but [`LocalSkip::Other`].
    pub const ALL: [LocalSkip; 11] = [
        Self::Symlink,
        Self::Fifo,
        Self::Socket,
        Self::Device,
        Self::ReservedName,
        Self::HardLink,
        Self::NotDownloaded,
        Self::OtherDevice,
        Self::Unreadable,
        Self::BadState,
        Self::Ignored,
    ];

    /// The stored spelling: what it is summed under. For
    /// [`LocalSkip::Other`], what [`key_of`] makes of the string.
    pub fn key(&self) -> &str {
        match self {
            Self::Symlink => "symlink",
            Self::Fifo => "fifo",
            Self::Socket => "socket",
            Self::Device => "device",
            Self::ReservedName => "reserved-name",
            Self::HardLink => "hard-link",
            Self::NotDownloaded => "not-downloaded",
            Self::OtherDevice => "other-device",
            Self::Unreadable => "unreadable",
            Self::BadState => "state-unreadable",
            Self::Ignored => "ignored",
            Self::Other(stored) => key_of(stored),
        }
    }

    /// The part behind the key. No skip the daemon writes has one.
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Other(stored) => detail_of(stored),
            _ => None,
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.iter().find(|skip| skip.key() == key).cloned()
    }

    pub fn parse(stored: &str) -> Self {
        Self::from_key(stored).unwrap_or_else(|| Self::Other(stored.to_owned()))
    }

    /// The group it is listed in; `None` for a string in no table.
    /// What cannot be read needs the user, file by file.
    pub fn group(&self) -> Option<Group> {
        Some(match self {
            Self::Symlink | Self::Fifo | Self::Socket | Self::Device | Self::OtherDevice | Self::ReservedName | Self::HardLink | Self::Ignored => {
                Group::Never
            }
            Self::NotDownloaded => Group::Waiting,
            Self::Unreadable | Self::BadState => Group::PerFile,
            Self::Other(stored) => return known_group(key_of(stored)),
        })
    }
}

impl fmt::Display for LocalSkip {
    /// The stored spelling.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(stored) => f.write_str(stored),
            _ => f.write_str(self.key()),
        }
    }
}

impl From<&str> for LocalSkip {
    fn from(stored: &str) -> Self {
        Self::parse(stored)
    }
}

impl From<String> for LocalSkip {
    fn from(stored: String) -> Self {
        Self::parse(&stored)
    }
}

/// What keeps an item on this computer that the folder cannot hold any more
/// (a name too long, the Personal Vault...): it leaves the disk in the first
/// cycle that finds nothing of these in it. The third field of a line of
/// `Skipped()`, stored with the item's deferred change.
///
/// Stored as `<key>:<detail>`: the number of changes, or the path of the
/// first thing found, relative to the folder. A line of an item that is not
/// on this computer has the empty string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WaitsFor {
    /// It is still here, and no cycle has looked at it yet.
    Cycle,
    /// So many changes at or below it wait in the outbox.
    Uploads(u64),
    /// Something at this path differs from what OneDrive has, and is still
    /// to be recorded: made, changed, moved, renamed or deleted here.
    Changes(String),
    /// A file a program has open for writing.
    OpenForWriting(String),
    /// A file of ours whose state cannot be read.
    UnknownState(String),
    /// A file that is not downloaded and is not where the base has its item:
    /// from elsewhere, a copy, or renamed here and not recorded yet.
    NotDownloaded(String),
    /// A file or a directory whose name is on the ignore list: it is only on
    /// this computer, and is never uploaded.
    LocalOnly(String),
    /// Another filesystem mounted inside.
    MountedInside(String),
    /// An item OneDrive moved out of it, which could not be put where it
    /// belongs yet (its name there is held by something made here), and
    /// stays where it was meanwhile.
    MovedAway(String),
    /// A string no variant spells.
    Other(String),
}

impl WaitsFor {
    pub fn key(&self) -> &str {
        match self {
            Self::Cycle => "cycle",
            Self::Uploads(_) => "uploads",
            Self::Changes(_) => "changes",
            Self::OpenForWriting(_) => "open-for-writing",
            Self::UnknownState(_) => "unknown-state",
            Self::NotDownloaded(_) => "not-downloaded",
            Self::LocalOnly(_) => "local-only",
            Self::MountedInside(_) => "mounted-inside",
            Self::MovedAway(_) => "moved-in-onedrive",
            Self::Other(stored) => stored.split_once(':').map_or(stored.as_str(), |(key, _)| key),
        }
    }

    /// The path of what keeps it, relative to the folder; none for the
    /// variants without one.
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Changes(path) | Self::OpenForWriting(path) | Self::UnknownState(path) | Self::NotDownloaded(path) | Self::LocalOnly(path) | Self::MountedInside(path) | Self::MovedAway(path) => {
                Some(path)
            }
            _ => None,
        }
    }

    /// This, with its path below `root`: what `Skipped()` sends.
    pub fn under(self, root: &str) -> Self {
        let full = |path: String| format!("{}/{path}", root.trim_end_matches('/'));
        match self {
            Self::Changes(path) => Self::Changes(full(path)),
            Self::OpenForWriting(path) => Self::OpenForWriting(full(path)),
            Self::UnknownState(path) => Self::UnknownState(full(path)),
            Self::NotDownloaded(path) => Self::NotDownloaded(full(path)),
            Self::LocalOnly(path) => Self::LocalOnly(full(path)),
            Self::MountedInside(path) => Self::MountedInside(full(path)),
            Self::MovedAway(path) => Self::MovedAway(full(path)),
            other => other,
        }
    }

    pub fn parse(stored: &str) -> Self {
        let (key, detail) = stored.split_once(':').unwrap_or((stored, ""));
        let path = || detail.to_owned();
        match key {
            "cycle" if detail.is_empty() => Self::Cycle,
            "uploads" => detail.parse().map_or_else(|_| Self::Other(stored.to_owned()), Self::Uploads),
            "changes" => Self::Changes(path()),
            "open-for-writing" => Self::OpenForWriting(path()),
            "unknown-state" => Self::UnknownState(path()),
            "not-downloaded" => Self::NotDownloaded(path()),
            "local-only" => Self::LocalOnly(path()),
            "mounted-inside" => Self::MountedInside(path()),
            "moved-in-onedrive" => Self::MovedAway(path()),
            _ => Self::Other(stored.to_owned()),
        }
    }
}

impl fmt::Display for WaitsFor {
    /// The stored spelling.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cycle => f.write_str("cycle"),
            Self::Uploads(n) => write!(f, "uploads:{n}"),
            Self::Other(stored) => f.write_str(stored),
            _ => write!(f, "{}:{}", self.key(), self.path().unwrap_or_default()),
        }
    }
}

/// `(needs, free)` of `too-big:<needs>:<free>`.
fn too_big_sizes(stored: &str) -> Option<(u64, u64)> {
    let (needs, free) = stored.strip_prefix(TOO_BIG_PREFIX)?.split_once(':')?;
    Some((needs.parse().ok()?, free.parse().ok()?))
}

/// The key a stored reason, a row's or a skip's, is summed under: its key;
/// the key of a reason that carries a detail behind it, `<key>: <detail>`,
/// for every key of the two tables; and `too-big` for every
/// `too-big:<needs>:<free>`. A string in no table is its own key.
pub fn key_of(stored: &str) -> &str {
    if too_big_sizes(stored).is_some() {
        return TOO_BIG_KEY;
    }
    match stored.split_once(": ") {
        Some((key, _)) if known_group(key).is_some() => key,
        _ => stored,
    }
}

/// What stands behind the key ([`key_of`]) in a string no variant spells.
fn detail_of(stored: &str) -> Option<String> {
    let rest = stored.strip_prefix(key_of(stored))?;
    rest.strip_prefix(": ").or_else(|| rest.strip_prefix(':')).map(str::to_owned)
}

/// The group of a key ([`key_of`]) of either table; `None` for one no code
/// of the daemon writes.
pub fn known_group(key: &str) -> Option<Group> {
    match Reason::from_key(key) {
        Some(reason) => reason.group(),
        None => LocalSkip::from_key(key)?.group(),
    }
}

#[cfg(test)]
mod tests;
