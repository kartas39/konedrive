//! What an intercepted open is answered: the decision, which touches
//! nothing, and the function that reads what the decision asks for and
//! carries it out.

use std::io;
use std::os::fd::{AsFd, BorrowedFd};

use konedrive_fs::placeholder::{read_item_id, read_state, State, StateError};
use konedrive_helper::errno::Errno;
use konedrive_helper::pending::PendingOpen;

use super::hydration::hydrate;
use super::LOG;
use crate::shared::{fault, Shared};

/// What `read_state` says of a file.
pub(super) type StateRead = Result<Option<State>, StateError>;

/// What an intercepted open is answered.
#[derive(Debug)]
pub(super) enum Decision {
    /// Let through, and nothing else: not a regular file, the owning
    /// daemon's own open, or a file that is not ours.
    Allow,
    /// The file reads `hydrated`: let through with an ignore mark, if it
    /// still reads `hydrated` once the mark is in place (see
    /// [`mark_while_hydrated`]); if it does not, the open is decided again
    /// on what it reads then ([`Decision::by_state`]).
    AllowMarked,
    /// A placeholder whose content is not there: ask its owner's daemon.
    Hydrate,
    /// Refused, `EIO`: nothing says the content is there, and §5.2's last
    /// rule is never to allow zeros.
    Deny(Denied),
}

/// Why an open is denied. Each is answered `EIO`.
#[derive(Debug)]
pub(super) enum Denied {
    /// The file carries an item id and no state attribute: one of ours with
    /// its state missing, and no idea whether its body is there.
    StateMissing { item: String },
    /// No state attribute, and the item id could not be read.
    ItemIdUnreadable(io::Error),
    /// The state attribute holds a value that is not a state.
    StateCorrupt(String),
    /// The state attribute could not be read.
    StateUnreadable(io::Error),
}

impl Denied {
    /// The errno the opener gets.
    pub(super) fn errno(&self) -> Errno {
        Errno::EIO
    }
}

/// What the decision needs to know of an open of a regular file. Each is
/// asked for only when what came before left the question open, and at most
/// once, so that what answers them (`OpenFacts`) reads no more of the file,
/// and takes no more locks, than the decision needs.
pub(super) trait Facts {
    /// Whether the opener is the daemon of the file's owner (see
    /// [`daemon_is_exempt`]).
    fn daemons_own(&self) -> bool;
    /// The file's state attribute.
    fn state(&self) -> StateRead;
    /// The file's item id attribute.
    fn item_id(&self) -> io::Result<Option<String>>;
}

impl Decision {
    /// The decision for an open, from nothing but `facts`: it reads no file,
    /// takes no lock and writes no log line itself.
    ///
    /// The order is the rule. A file that is not regular cannot be a
    /// placeholder. The owning daemon's own opens bypass everything else. And
    /// only then does the file's state decide.
    pub(super) fn of(regular: bool, facts: &impl Facts) -> Decision {
        if !regular {
            return Decision::Allow;
        }
        if facts.daemons_own() {
            return Decision::Allow;
        }
        Decision::by_state(facts.state(), facts)
    }

    /// The decision for a file whose state has been read: the last step of
    /// [`of`](Self::of), and the whole of a second decision, when a file
    /// stopped reading `hydrated` while it was being marked.
    pub(super) fn by_state(state: StateRead, facts: &impl Facts) -> Decision {
        match state {
            // The ignore mark is added only to a file whose state
            // is `hydrated` — its content is actually present. A file with no
            // konedrive xattrs at all is not managed by us and is let through,
            // but it must NOT get an ignore mark, because a placeholder under
            // construction looks exactly like that. `create_placeholder`
            // (konedrive_fs::placeholder) opens a nameless O_TMPFILE in the
            // directory, writes the size, item id, state and mtime through
            // that descriptor, and only then links it in by name — so no name
            // ever shows a half-built file. But the O_TMPFILE open is itself
            // an open in a marked directory and raises a FAN_OPEN_PERM
            // (kernel fact 7, docs/kernel-behavior-7.2/interception.md §7) before a single
            // xattr exists; that is the event that arrives here with none.
            // Answered with an ignore mark, the mark — which survives
            // modification — would still be on the inode when the finished
            // `online-only` placeholder is linked in, and every open of it
            // would be let through to zeros. The owning daemon's own builds
            // are normally allowed by the exemption above and never get this
            // far; this rule covers a build by anyone it does not.
            //
            // And the state is read once more *after* the mark is placed
            //: see `mark_while_hydrated`.
            Ok(Some(State::Hydrated)) => Decision::AllowMarked,
            // A file with no state attribute is not ours — unless it also
            // carries an item id, in which case it is one of ours with its
            // state missing, and we have no idea whether its body is there.
            // §5.2's last rule applies: never allow zeros.
            Ok(None) => match facts.item_id() {
                Ok(None) => Decision::Allow,
                Ok(Some(item)) => Decision::Deny(Denied::StateMissing { item }),
                Err(e) => Decision::Deny(Denied::ItemIdUnreadable(e)),
            },
            Ok(Some(_)) => Decision::Hydrate,
            Err(StateError::Corrupt(value)) => Decision::Deny(Denied::StateCorrupt(value)),
            Err(StateError::Io(e)) => Decision::Deny(Denied::StateUnreadable(e)),
        }
    }
}

/// The facts of one intercepted open, read through its event fd and from
/// the helper's own tables when the decision asks.
struct OpenFacts<'a> {
    shared: &'a Shared,
    open: &'a PendingOpen,
    opener_pid: i32,
    owner: u32,
}

impl Facts for OpenFacts<'_> {
    fn daemons_own(&self) -> bool {
        daemon_is_exempt(self.shared, self.opener_pid, self.owner)
    }

    fn state(&self) -> StateRead {
        read_state(self.open)
    }

    fn item_id(&self) -> io::Result<Option<String>> {
        read_item_id(self.open)
    }
}

/// Decides one intercepted open ([`Decision`]) and always answers it —
/// allow, deny, or a move into a hydration job that guarantees a later
/// answer from `finish` — before returning. `open` is consumed by whichever of the three it is; a
/// path that took none, or a panic, drops it, and that denies `EIO`
/// (`PendingOpen`).
///
/// `since` is the count of root unregistrations when the event was read (see
/// [`mark_while_hydrated`]).
///
/// # Nothing of the file outlives the answer
///
/// The file is inspected through the event fd itself — kernel fact 1 rules
/// out opening it ourselves — and no duplicate of it is made here: the only
/// one is the daemon's (`jobs::Dispatch`). A duplicate shares the event's
/// `O_RDWR` open file, so while one is open the file counts as open for
/// writing: the daemon's registration probe, which creates a file in an
/// already-marked root and at once takes a write lease on it, found the
/// lease refused when this function still held one after it had allowed the
/// probe's open.
pub(crate) fn handle_open(shared: &Shared, open: PendingOpen, opener_pid: i32, since: u64) {
    let seen = match stat_of(open.as_fd()) {
        Ok(seen) => seen,
        Err(e) => {
            tracing::error!(target: LOG, "cannot stat an intercepted open: {e}");
            open.deny(Errno::EIO);
            return;
        }
    };
    let Seen { regular, owner, dev, ino, len } = seen;
    if regular {
        // Compiled in only with `fault-injection`, armed only by the VM
        // suite; an empty function otherwise. Placed before any decision
        // about a regular file, so the unwind it causes drops an open
        // nothing has answered yet, and the opener gets `EIO`.
        fault::panic_on_size(len);
    }

    let mut decision = {
        let facts = OpenFacts { shared, open: &open, opener_pid, owner };
        Decision::of(regular, &facts)
    };
    // At most two turns: the second only when the file stopped reading
    // `hydrated` between the first read and the mark, and then it is not
    // `hydrated` any more.
    loop {
        match decision {
            Decision::Allow => open.allow(),
            Decision::AllowMarked => {
                // `fault-injection` builds only: the VM suite's I1 scenario.
                fault::delay_before_ignore_mark();
                let file = FileId { owner: Some(owner), dev, ino };
                match mark_while_hydrated(shared, open.as_fd(), file, since) {
                    Ok(()) => open.allow(),
                    Err(now) => {
                        tracing::info!(
                            target: LOG,
                            "dev={dev} ino={ino} stopped reading hydrated while its open was \
                             being decided (it now reads {now:?}); deciding again"
                        );
                        let facts = OpenFacts { shared, open: &open, opener_pid, owner };
                        decision = Decision::by_state(now, &facts);
                        continue;
                    }
                }
            }
            Decision::Hydrate => hydrate(shared, open, owner, dev, ino, since),
            Decision::Deny(why) => {
                match &why {
                    Denied::StateMissing { item } => tracing::error!(
                        target: LOG,
                        "dev={dev} ino={ino} carries item id {item} but no state attribute; \
                         denying rather than risk serving an unfilled placeholder"
                    ),
                    Denied::ItemIdUnreadable(e) => {
                        tracing::error!(
                            target: LOG,
                            "cannot read the item id on dev={dev} ino={ino}: {e}"
                        )
                    }
                    Denied::StateCorrupt(value) => tracing::error!(
                        target: LOG,
                        "dev={dev} ino={ino} has an unrecognised state {value:?}; denying rather \
                         than risk serving an unfilled placeholder"
                    ),
                    Denied::StateUnreadable(e) => {
                        tracing::error!(target: LOG, "unreadable xattrs on dev={dev} ino={ino}: {e}")
                    }
                }
                open.deny(why.errno());
            }
        }
        return;
    }
}

/// Places the ignore mark on a file just read `hydrated`, and keeps it only
/// if the file still reads `hydrated` **after** the mark is in place, and no
/// root has been unregistered since its open was read. `Err` carries what
/// the file reads now, when that is not `hydrated`; the mark is off again by
/// then. Every place the helper marks a file goes through here.
///
/// # Why after
///
/// "Read `hydrated`, then mark" is two steps, and a dehydration can fall
/// between them: it makes `dehydrating` durable and then has the helper
/// `ClearIgnore` (step 2). A mark placed after that `ClearIgnore`,
/// on the strength of a read made before the `dehydrating`, was outlived by
/// the punch: measured with a 1.5 s stall injected between the two steps,
/// the next reader got 65 536 zero bytes after no fetch, on Btrfs, ext4 and
/// XFS. Reading again after the mark closes it from both sides. If the
/// `dehydrating` came first, the second read sees it, and the mark comes off
/// here. If the mark came first, it was there for the `ClearIgnore` to
/// remove. So a mark is left only on a file that read `hydrated` at a moment
/// the mark was already in place.
///
/// # Why "no unregistration since" (second guard)
///
/// A root's unregistration walk takes the ignore mark off every file it
/// passes, and a hydration still in flight then — or an open read off the
/// queue before its directory was unmarked — used to mark its file after
/// the walk had gone by. None of that can empty a marked file any more: the
/// daemon's local rule has every punch clear the mark first, or not punch
///. This guard, like the registration walk's clearing
/// (`marks::walk_and_mark`), is defence in depth: a mark it withholds is one
/// nothing has to clear later. `unregistrations` is bumped
/// before an unregistration's walk begins and again after it ends. Whatever
/// was read before either bump for the file owner's uid is not marked: the file is still let
/// through, since its content is there, but its next open is simply decided
/// again. The helper cannot tell from a descriptor which root a file is in,
/// so the count is kept per uid and matched by the file's owner (see
/// [`Unregistrations`]): an unregistration costs that user's files being
/// decided at that moment one extra event each, later, and nobody else's.
///
/// What this guard cannot see is an open that the kernel queued before its
/// directory was unmarked and the event loop read only after the walk had
/// ended; the mark that leaves is harmless on a file with content, and the
/// daemon clears it before it ever empties the file.
pub(super) fn mark_while_hydrated(
    shared: &Shared,
    fd: BorrowedFd<'_>,
    file: FileId,
    since: u64,
) -> Result<(), StateRead> {
    let FileId { owner, dev, ino } = file;
    place_ignore_mark(shared, fd, dev, ino);
    let now = read_state(&fd);
    let hydrated = matches!(now, Ok(Some(State::Hydrated)));
    let unregistered = shared.unregistrations.since(since, owner);
    if hydrated && !unregistered {
        return Ok(());
    }
    if let Err(e) = shared.marks.clear_ignore(fd) {
        // Practically unreachable (a removal allocates nothing, and the
        // descriptor is the one just marked through), and not left
        // unguarded if it happens: a file that is not `hydrated` is not let
        // through here, and a dehydration of it cannot take its lease while
        // this open's descriptor is held; a stale mark on a `hydrated` file
        // is harmless while the file holds its content, and the daemon clears
        // it before it empties the file.
        tracing::error!(
            target: LOG,
            "cannot take the ignore mark off dev={dev} ino={ino} again ({e}); it reads {now:?}"
        );
    }
    if hydrated {
        tracing::info!(
            target: LOG,
            "dev={dev} ino={ino} was let through without an ignore mark: a root was unregistered \
             while its open was being decided"
        );
        return Ok(());
    }
    Err(now)
}
/// The owning daemon's own opens bypass everything
/// else: it must be able to re-open files it left `hydrating` or
/// `dehydrating` during startup recovery, and treating that open like any
/// other would mean asking the very daemon that is blocked on it to
/// hydrate the file — a deadlock.
///
/// The exemption is deliberately narrow. It is not "this pid connected to
/// our socket": anyone can do that, and an unscoped version of this check
/// let any local process be exempted from interception of any file. It is
/// "this pid holds a connection that owns a registered root, and the file
/// being opened belongs to that same user". A process with no root gets
/// nothing, and no daemon is ever exempted from another user's files.
///
/// Dehydration (`konedrived/src/hydration/dehydrate.rs::dehydrate`) used
/// to depend on this, and deliberately no longer does: it
/// opened the file again, by path, after clearing the ignore mark, and
/// that open was let through only because it hit this exemption first.
/// It now does the whole sequence on the one descriptor it opened before
/// the first check, so it raises no open of its own at all. Nothing else
/// should acquire such a dependency: what the exemption covers is startup
/// recovery, not a way to open a file the state check would have handled.
///
/// `SO_PEERCRED` supplies the pid, so the
/// caller cannot claim to be a daemon it is not; owning a registered root is
/// what separates the user's daemon from any process that merely connected.
///
/// Only the file owner's **top** connection is exempt: the one
/// its hydrations go to, which is the daemon in every case but a transient
/// same-uid connection sitting on top of it — and that one is exempt only for
/// files of its own uid, which it can read anyway.
fn daemon_is_exempt(shared: &Shared, opener_pid: i32, file_owner: u32) -> bool {
    let on_top = shared.daemons.is_top_pid(file_owner, opener_pid);
    on_top && shared.roots.has_root_for(file_owner)
}

fn place_ignore_mark(shared: &Shared, fd: BorrowedFd<'_>, dev: u64, ino: u64) {
    // Not verified by reading `/proc/self/fdinfo/<group>`: that is O(marks)
    // per hydration, and the helper holds one mark per directory in every
    // sync tree on the machine. VM suite asserts on fdinfo instead,
    // where the cost does not matter and the assertion is worth making — the
    // syscall's return value is known to lie about this (M3).
    if let Err(e) = shared.marks.ignore_file(fd) {
        tracing::error!(
            target: LOG,
            "cannot place the ignore mark on dev={dev} ino={ino}: {e}; every open of this file \
             will keep raising a permission event"
        );
    }
}
/// What `fstat` says of the file behind an event fd.
struct Seen {
    /// A regular file: the only kind that can be a placeholder.
    regular: bool,
    owner: u32,
    dev: u64,
    ino: u64,
    len: u64,
}

/// `fstat` on the event fd itself: nothing is opened and nothing duplicated,
/// so it cannot fail for want of a descriptor.
fn stat_of(fd: BorrowedFd<'_>) -> io::Result<Seen> {
    let stat = nix::sys::stat::fstat(fd)?;
    Ok(Seen {
        regular: stat.st_mode & libc::S_IFMT == libc::S_IFREG,
        owner: stat.st_uid,
        dev: stat.st_dev,
        ino: stat.st_ino,
        len: u64::try_from(stat.st_size).unwrap_or(0),
    })
}

/// Who owns the file behind a descriptor, and which inode it is.
#[derive(Debug, Clone, Copy)]
pub(super) struct FileId {
    /// `None` when it could not be read: then every unregistration since
    /// counts against the file.
    pub(super) owner: Option<u32>,
    pub(super) dev: u64,
    pub(super) ino: u64,
}

/// [`FileId`] behind an event fd. The inode is for log lines only, and is
/// zeros if it cannot be read.
pub(super) fn file_of(fd: BorrowedFd<'_>) -> FileId {
    match stat_of(fd) {
        Ok(seen) => FileId { owner: Some(seen.owner), dev: seen.dev, ino: seen.ino },
        Err(_) => FileId { owner: None, dev: 0, ino: 0 },
    }
}

#[cfg(test)]
mod tests;
