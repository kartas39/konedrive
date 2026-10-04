//! Why a change is kept back: an outbox row's [`Reason`], and the
//! [`LocalSkip`] of what an examination never uploads (`local_skipped`).
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
//! which re-exports it in `outbox`) and the daemon hold these, and
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

/// What an outbox row's `reason` says. The examination writes the first
/// seven; the outbox worker the rest.
///
/// A variant with an `Option<String>` is stored `<key>: <detail>` when it
/// has one, and as the bare key when not.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Reason {
    /// A writer has the file open (§4.3).
    OpenForWriting,
    /// A removal held by the mass-delete guard (§3.4).
    MassDelete,
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
    /// A change inside a folder no longer synced here whose item OneDrive
    /// answers `404` for while its listing still has it (issue #104):
    /// blocked until the listing says it is gone (the row goes) or it is
    /// changed again. The one reason the store itself reads
    /// (`TreeStore::outbox_settle_not_found`).
    LeavingNotFound,
    /// The file is not downloaded (WR1).
    NotLocal,
    /// Its size or time moved while it was being sent (§4.3).
    Changed,
    /// The folder it goes into is not in OneDrive (yet, or any more).
    Parent,
    /// OneDrive holds other content than was sent: sent again from zero.
    Hash,
    /// A move out of the folder, with nothing to reach it by.
    MoveOut,
    /// A moved-out object waits for the helper, which alone can reach it by
    /// its handle.
    NoHelper,
    /// A moved-out object the helper will not hand over, or whose place
    /// cannot be told: kept, never taken for gone (F90). With another errno
    /// than `EPERM`, `moved-out-unreachable: errno <n>`.
    Unreachable(Option<String>),
    /// A moved-out object is back in the folder: the examination's.
    BackInside,
    /// Where a moved-out object is cannot be proved: nothing is taken off
    /// or deleted until it can.
    PlaceUnknown,
    /// A moved-out placeholder cannot be opened for writing to download it:
    /// `moved-out-not-opened: <the error>`. In backoff.
    NotOpened(Option<String>),
    /// A moved-out placeholder's download failed: `download-failed: errno
    /// <n>`. Its item stays in OneDrive.
    Download(Option<String>),
    /// `ESTALE` once for a moved-out object: asked again before it is
    /// believed.
    GoneOnce,
    /// `ESTALE` for a handle taken on another filesystem than the folder's
    /// now: it says nothing, so nothing goes.
    StaleHandle,
    /// `ESTALE` twice, but where the object was last proved to be it may
    /// still stand, or that place is not known: not gone.
    GoneUnproved,
    /// A read lease cannot be probed: `lease-probe-failed: <the error>`. A
    /// writer cannot be ruled out, so nothing is filled.
    NoLease(Option<String>),
    /// The account is paused: an upload in fragments stopped after the
    /// fragment it was sending, its session kept. Waiting, never a failure.
    Paused,
    /// A new file's name is held in OneDrive by the empty placeholder of an
    /// upload session of this folder (issue #47). Tried again later.
    SessionOpen,
    /// The row's name in OneDrive is held by an empty file the delta feed
    /// never listed: an upload session's placeholder (issue #89). Tried
    /// again later.
    NameHeld,
    /// The item changed in OneDrive each time its removal was sent: in
    /// backoff.
    ChangedAgain,
    /// A row rewritten and sent again at once too often in a row: in
    /// backoff, like a failure.
    ChangingAgain,
    /// The upload session ended under the upload twice in one run: in
    /// backoff.
    SessionEnded,
    /// The write gate closed between two fragments: `not allowed now:
    /// <why>`. Waiting until it opens; the session is kept.
    NotAllowed(Option<String>),
    /// The file carries a `user.konedrive.state` no konedrive writes, or
    /// one that cannot be read: `state-unreadable: <the error>`. Blocked.
    BadState(Option<String>),
    /// Blocked: the row's place has no name.
    NoName,
    /// Blocked: a change, move or removal whose row names no item, or no
    /// base.
    NoItem,
    /// Blocked: the row's base has neither an eTag nor a cTag to send with.
    NoGuard,
    /// Blocked: a moved-out object's row has no handle to find it by.
    NoHandle,
    /// Blocked: the helper refuses the handle of a moved-out object.
    BadHandle,
    /// Blocked: the object a `move-out` row's handle opens carries another
    /// item's id.
    AnotherItem,
    /// What a blocked row with no reason is listed under.
    Blocked,
    /// OneDrive could not be reached (issue #87). In backoff; the error's
    /// own text is in the journal only.
    Network,
    /// The local file could not be read or written. In backoff.
    LocalIo,
    /// The daemon's own index (the store) failed. In backoff.
    Store,
    /// Any other failure of a step. In backoff.
    Failed,
    /// Refused while OneDrive is full: ready, in its place, not taken until
    /// a quota read lets it go (issue #2).
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
    pub const ALL: [Reason; 49] = [
        Self::OpenForWriting,
        Self::MassDelete,
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
        Self::LeavingNotFound,
        Self::NotLocal,
        Self::Changed,
        Self::Parent,
        Self::Hash,
        Self::MoveOut,
        Self::NoHelper,
        Self::Unreachable(None),
        Self::BackInside,
        Self::PlaceUnknown,
        Self::NotOpened(None),
        Self::Download(None),
        Self::GoneOnce,
        Self::StaleHandle,
        Self::GoneUnproved,
        Self::NoLease(None),
        Self::Paused,
        Self::SessionOpen,
        Self::NameHeld,
        Self::ChangedAgain,
        Self::ChangingAgain,
        Self::SessionEnded,
        Self::NotAllowed(None),
        Self::BadState(None),
        Self::NoName,
        Self::NoItem,
        Self::NoGuard,
        Self::NoHandle,
        Self::BadHandle,
        Self::AnotherItem,
        Self::Blocked,
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
            Self::MassDelete => "mass-delete",
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
            Self::LeavingNotFound => "leaving-not-found",
            Self::NotLocal => "not-downloaded",
            Self::Changed => "changed-while-sending",
            Self::Parent => "parent-not-in-onedrive",
            Self::Hash => "hash-mismatch",
            Self::MoveOut => "move-out-not-yet",
            Self::NoHelper => "waiting-for-the-helper",
            Self::Unreachable(_) => "moved-out-unreachable",
            Self::BackInside => "back-in-the-folder",
            Self::PlaceUnknown => "moved-out-place-unknown",
            Self::NotOpened(_) => "moved-out-not-opened",
            Self::Download(_) => "download-failed",
            Self::GoneOnce => "gone-once",
            Self::StaleHandle => "handle-from-another-filesystem",
            Self::GoneUnproved => "gone-unproved",
            Self::NoLease(_) => "lease-probe-failed",
            Self::Paused => "paused",
            Self::SessionOpen => "upload-session-open",
            Self::NameHeld => "name-held-by-an-upload",
            Self::ChangedAgain => "changed in OneDrive again and again",
            Self::ChangingAgain => "changing in OneDrive again and again",
            Self::SessionEnded => "the upload session ended twice",
            Self::NotAllowed(_) => "not allowed now",
            Self::BadState(_) => "state-unreadable",
            Self::NoName => "no-name",
            Self::NoItem => "no-item",
            Self::NoGuard => "no-guard",
            Self::NoHandle => "no-handle",
            Self::BadHandle => "bad-handle",
            Self::AnotherItem => "another-item",
            Self::Blocked => "blocked",
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
            | Self::Unreachable(detail)
            | Self::NotOpened(detail)
            | Self::Download(detail)
            | Self::NoLease(detail)
            | Self::NotAllowed(detail)
            | Self::BadState(detail) => detail.clone(),
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
            Self::Unreachable(_) => Self::Unreachable(detail),
            Self::NotOpened(_) => Self::NotOpened(detail),
            Self::Download(_) => Self::Download(detail),
            Self::NoLease(_) => Self::NoLease(detail),
            Self::NotAllowed(_) => Self::NotAllowed(detail),
            Self::BadState(_) => Self::BadState(detail),
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
    /// `kept_back::group_of`). `None` for a held removal, which has its own
    /// question, and for a string in no table.
    pub fn group(&self) -> Option<Group> {
        Some(match self {
            // `quota-exceeded` only until a start converts it to `waiting-for-space` (#2).
            Self::Quota | Self::WaitingForSpace | Self::TooBig(_) | Self::Forbidden => Group::OneAction,
            Self::NameCharacters | Self::NameSpaces | Self::NameReserved | Self::NameNotUtf8 | Self::TooLarge | Self::Refused(_) => {
                Group::PerFile
            }
            // What keeps a folder no longer synced here on disk (issue #104).
            Self::LeavingNotFound => Group::PerFile,
            // What the worker blocks a row with beside those: the row itself, or the
            // file's state, is not what a step can work with. `Blocked`: no reason at all.
            Self::NoName
            | Self::NoItem
            | Self::NoGuard
            | Self::NoHandle
            | Self::BadHandle
            | Self::AnotherItem
            | Self::BadState(_)
            | Self::Blocked => Group::PerFile,
            Self::OpenForWriting
            | Self::Locked
            | Self::NotFound
            | Self::NotLocal
            | Self::Changed
            | Self::Parent
            | Self::Hash
            | Self::MoveOut
            | Self::NoHelper
            | Self::Unreachable(_)
            | Self::BackInside
            | Self::PlaceUnknown
            | Self::Download(_)
            | Self::GoneOnce
            | Self::StaleHandle
            | Self::GoneUnproved
            | Self::NoLease(_)
            | Self::Network
            | Self::LocalIo
            | Self::Store
            | Self::Failed
            | Self::NotOpened(_)
            | Self::Paused
            | Self::SessionOpen
            | Self::NameHeld
            | Self::ChangedAgain
            | Self::ChangingAgain
            | Self::SessionEnded
            | Self::NotAllowed(_) => Group::Waiting,
            Self::MassDelete => return None,
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

/// Why an examination never uploads something (`local_skipped`, §3.4 rule 2).
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
    /// A file of ours whose konedrive state cannot be read, inside a folder
    /// that is no longer placed (issue #104): the folder stays on disk until
    /// it can be read.
    UnknownState,
    /// Another filesystem mounted inside a folder that is no longer placed
    /// (issue #104): the folder stays on disk until it is unmounted.
    MountedInside,
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
        Self::UnknownState,
        Self::MountedInside,
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
            Self::UnknownState => "unknown-state",
            Self::MountedInside => "mounted-inside",
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
    /// `not-downloaded` is also a row's reason ([`Reason::NotLocal`]), and
    /// is grouped as that is.
    pub fn group(&self) -> Option<Group> {
        Some(match self {
            Self::Symlink | Self::Fifo | Self::Socket | Self::Device | Self::OtherDevice | Self::ReservedName | Self::HardLink | Self::Ignored => {
                Group::Never
            }
            // What keeps a folder no longer synced here on disk (issue #104):
            // the user unmounts, or fixes or removes the file.
            Self::UnknownState | Self::MountedInside => Group::PerFile,
            Self::NotDownloaded => Group::Waiting,
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
/// of the daemon writes, and for `mass-delete`.
pub fn known_group(key: &str) -> Option<Group> {
    match Reason::from_key(key) {
        Some(reason) => reason.group(),
        None => LocalSkip::from_key(key)?.group(),
    }
}

#[cfg(test)]
mod tests;
