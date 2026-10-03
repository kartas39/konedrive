use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::time::Instant;

use konedrive_fs::placeholder::{read_item_id, read_state, State, StateError};
use konedrive_helper::{jobs, marks};
use konedrive_proto::ToDaemon;
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::fanotify::MaskFlags;

use jobs::{Enrolled, Owner};
use konedrive_helper::outbox::{Outbox, Outgoing};

use crate::pool;
use crate::shared::{
    fault, lock, Daemon, Refusal, Shared, Throttle, WaiterSlot, DAEMON_WAIT, EVENT_FD_FAILED,
    EVENT_QUEUE_DEPTH, EVENT_WORKERS, EXHAUSTION_BACKOFF, GLOBAL_MAX_DAEMON_WAITERS,
    MAX_DAEMON_WAITERS, UNOPENABLE,
};

/// Takes ownership of a permission event's fd, preserving its exact number.
///
/// `fanotify_write()` matches a permission response against the fd number
/// `read_events()` handed out for that event (`fanotify(7)`: "fd — This is
/// the file descriptor from the structure fanotify_event_metadata"). A
/// duplicate has a different number, so anything we may answer only after
/// this event's iteration of the read loop ends — the "ask the daemon and
/// wait" path in `handle_open` — must keep using this exact descriptor, not
/// a dup of it, and must not let it close before the response is written: a
/// permission event that is read but never answered leaves its opener
/// blocked until the whole fanotify group fd is closed (`fanotify(7)`),
/// which in practice means until the helper exits.
///
/// `FanotifyEvent::drop` would close this fd when the event goes out of
/// scope at the end of the read loop's iteration; `mem::forget` disarms that
/// so the `OwnedFd` we build from the same raw number is the sole owner.
fn take_fd(event: nix::sys::fanotify::FanotifyEvent) -> Option<OwnedFd> {
    let raw = event.fd()?.as_raw_fd();
    std::mem::forget(event);
    // SAFETY: `event.fd()` returned a valid, open descriptor owned by
    // `event`; forgetting `event` just above means nothing else will close
    // it, so this `OwnedFd` becomes its sole owner.
    Some(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// What the event loop does about an errno from `read_events`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadFailure {
    /// Everything queued has been read — or the event at the head of the
    /// queue could not be handed over and the kernel answered it itself
    /// — go back to `poll()`, which reports whatever is still
    /// queued at once.
    Drained,
    /// A signal interrupted the read; read again at once.
    Interrupted,
    /// The process, or the machine, is out of file descriptors.
    Exhausted,
    /// The kernel could not open one event's descriptor, answered that
    /// event `FAN_DENY` itself, and handed its errno back instead of it
    ///. Read on at once: the event is gone from the queue.
    EventRefused,
    /// The group's own descriptor, or the buffer it is read into, is broken.
    Fatal,
}

/// **The helper exiting is worse than the helper denying.**
///
/// `fanotify(7)` is explicit that closing the group's descriptor sets every
/// outstanding permission event to *allowed*, so a helper that dies hands
/// every suspended open straight through and each of those applications reads
/// a placeholder full of zeros — silently, with nothing to notice it. A denial
/// is the opposite: visible, answerable, and something the application can
/// retry. So the bar for ending the process is very high, and running out of
/// descriptors does not come near it.
///
/// `EMFILE` and `ENFILE` are the errnos that failure arrives as, and they are
/// self-correcting: the descriptors the helper is short of are the event fds
/// of opens that are still in flight, and every one of them is released as its
/// hydration finishes. The kernel has already denied the events it could not
/// copy out, so the opens caught in the window are answered rather than left
/// hanging; the loop's job is simply to still be there afterwards.
///
/// # Every other errno is one event's
///
/// The kernel creates each permission event's descriptor inside our `read()`
/// — `dentry_open()` with the group's `O_RDWR`, against the **opener's**
/// mount — and when that open fails, `fanotify_read()` answers the event
/// `FAN_DENY` itself, stops, and returns the events before it or, if there
/// were none, that open's errno. The errno describes one event the kernel has
/// already dealt with, not the group. Measured in the VM suite, on Btrfs,
/// ext4 and XFS: `EROFS` for an open through a read-only mount — a read-only
/// bind, `ProtectHome=read-only`, a Flatpak app with `home:ro` — and
/// `ETXTBSY` for a second open of an executable that is running. Both used
/// to fall into `Fatal`: the helper exited, the kernel allowed every open
/// suspended at that moment, and a reader waiting for a hydration got 65 536
/// zero bytes. Any local user could do it to everybody, and `Restart=always`
/// made it a crash loop. The opener of the refused event got `EPERM` in 8 ms
/// either way; the helper now simply reads on.
///
/// It cannot make the loop spin: `fanotify_read()` takes each event off the
/// queue before it tries to open its descriptor, so every such errno stands
/// for one event consumed — read on, and the queue drains to `EAGAIN` as it
/// always does.
///
/// Only what the group's own descriptor can report is fatal: `EBADF`,
/// `EINVAL` (a buffer too small for one event, or a broken group) and
/// `EFAULT` mean the thing this whole process exists to read is broken, and
/// staying alive around it buys nothing, since a group that cannot be read
/// cannot be answered either.
fn classify_read_failure(e: Errno) -> ReadFailure {
    match e {
        Errno::EAGAIN => ReadFailure::Drained,
        Errno::EINTR => ReadFailure::Interrupted,
        Errno::EMFILE | Errno::ENFILE => ReadFailure::Exhausted,
        Errno::EBADF | Errno::EINVAL | Errno::EFAULT => ReadFailure::Fatal,
        _ => ReadFailure::EventRefused,
    }
}

/// Answers permission events. Every event is only inspected here — mask and
/// pid are plain values, and the fd is handed off immediately — because
/// kernel fact 2 means any open our own code causes (a `stat`, an xattr
/// read, anything) inside a marked directory raises another event aimed at
/// us. `handle_open` runs its own file access and, on the "ask the daemon"
/// path, its own bounded wait on a worker thread, never on this one: that
/// wait can take up to `DAEMON_WAIT`, and blocking here would stall every
/// other pending open in the system for that long.
///
/// Nothing here waits on anything but the group: the event
/// descriptors the kernel creates inside `read_events()` are `O_NONBLOCK`, so
/// a file somebody holds a lease on cannot stop the loop — see `Marks::new`.
///
/// `read_events()` is nonblocking (kernel fact 3: the group is created with
/// `FAN_NONBLOCK`, or it would never return `EAGAIN` at all), specifically so
/// this loop can `poll()` the group fd instead of calling a blocking
/// `read()` directly: `poll()` blocks with no CPU cost until there is
/// something to read, then the inner loop drains everything currently
/// queued before polling again. A `read_events()`-then-`continue`-on-EAGAIN
/// loop with no wait in between would be a busy spin pinning a CPU core for
/// as long as the helper runs, which is not acceptable for a permanent
/// system service.
pub(crate) fn event_loop(shared: &Arc<Shared>, pool: &pool::Pool) -> anyhow::Result<()> {
    let mut exhaustion = Throttle::new();
    let own_pid = std::process::id() as i32;
    loop {
        let mut fds = [PollFd::new(shared.marks.group().as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let mut first_read = true;
        loop {
            // Before the read, so that an event is never given a count later
            // than one it could have been queued under (see
            // `mark_while_hydrated`).
            let since = shared.unregistrations.now();
            let events = match shared.marks.group().read_events() {
                Ok(events) => {
                    let unreported = exhaustion.reset();
                    if unreported > 0 {
                        tracing::error!(
                            "descriptors are available again; {unreported} more read(s) of the \
                             fanotify group had failed since the last report"
                        );
                    }
                    first_read = false;
                    events
                }
                Err(e) => match classify_read_failure(e) {
                    // The event descriptors are `O_NONBLOCK`, so
                    // an event whose file is leased cannot be handed over:
                    // the kernel answers it `FAN_DENY` itself and this read
                    // reports `EAGAIN`, as it does for an empty queue. The
                    // opener has its answer (`EPERM`) and the next `poll`
                    // sees whatever is still queued; only the journal can
                    // say it happened, so the first read after `poll`
                    // finding nothing is counted. A lower bound: an event
                    // that fails behind others in the same read is answered
                    // the same way, and the read returns the ones before it.
                    ReadFailure::Drained => {
                        if first_read {
                            shared.refusals.report(Refusal::Unopenable, || UNOPENABLE.to_owned());
                        }
                        break;
                    }
                    ReadFailure::Interrupted => continue,
                    ReadFailure::Exhausted => {
                        // Its own line, below; not also an unopenable one.
                        first_read = false;
                        if let Some(occurrences) = exhaustion.admit() {
                            tracing::error!(
                                "out of file descriptors reading the fanotify group ({e}); \
                                 {occurrences} read(s) failed, the kernel denies the events it \
                                 cannot hand over, and this loop keeps the group open and retries \
                                 every {EXHAUSTION_BACKOFF:?}. Raise LimitNOFILE in the unit if \
                                 this persists"
                            );
                        }
                        std::thread::sleep(EXHAUSTION_BACKOFF);
                        continue;
                    }
                    // The kernel has denied that one event
                    // (`EPERM` at its opener) and taken it off the queue;
                    // whatever is behind it is read next. Its own line, not
                    // also an unopenable one.
                    ReadFailure::EventRefused => {
                        first_read = false;
                        shared.refusals.report(Refusal::EventFdFailed, || {
                            format!(
                                "{EVENT_FD_FAILED} ({e}) — an open through a read-only mount \
                                 (EROFS) or of an executable that is running (ETXTBSY), most \
                                 likely"
                            )
                        });
                        continue;
                    }
                    ReadFailure::Fatal => return Err(e.into()),
                },
            };
            for event in events {
                let mask = event.mask();
                if mask.contains(MaskFlags::FAN_Q_OVERFLOW) {
                    tracing::warn!("queue overflow: some opens were not seen");
                    continue;
                }
                if !mask.contains(MaskFlags::FAN_OPEN_PERM) {
                    // We only ever mark FAN_OPEN_PERM, so this should not
                    // happen. The event simply drops: there is no permission
                    // decision pending on an event of a kind we never asked for.
                    continue;
                }
                let pid = event.pid();
                let Some(fd) = take_fd(event) else {
                    tracing::warn!("a permission event arrived with no descriptor");
                    continue;
                };
                // The helper's own opens: an `OpenByHandle` object in a
                // marked directory, or with a mark of its own, raises
                // an event aimed at this very group, while the connection
                // thread that opened it waits in `open_by_handle_at` and
                // reads nothing more from its daemon — so a hydration asked
                // of that daemon could never be reported back. Allowed here,
                // on this thread, before the pool: no worker, no daemon, and
                // not behind a full queue. The event's pid is the process's,
                // whichever thread opened (no FAN_REPORT_TID; pinned by
                // marks.rs's INIT_FLAGS and its test). The only files
                // the helper opens are those objects, handed straight to
                // their owner's daemon, and the feature probe's nameless
                // file at registration (`docs/design/writes.md` §8.2; SECURITY.md); measured in
                // docs/kernel-behavior-7.2/open-by-handle.md §15.
                if pid == own_pid {
                    respond_allow(shared, fd);
                    continue;
                }
                if let Err(rejected) = pool.submit(pool::OpenEvent { fd, pid, since }) {
                    // Saturation, not failure: EAGAIN tells the application to
                    // try the open again, which is true and is an answer. The
                    // alternative — spawning without bound — ends with the
                    // process dying and the kernel allowing every suspended
                    // open in the system.
                    shared.refusals.report(Refusal::PoolFull, || {
                        format!(
                            "all {EVENT_WORKERS} workers busy and {EVENT_QUEUE_DEPTH} opens \
                             already queued; denying an open with EAGAIN"
                        )
                    });
                    respond_deny(shared, rejected.fd, libc::EAGAIN);
                }
            }
        }
    }
}

/// Takes the event fd out of the slot the worker holds it in.
///
/// The slot exists for. The worker keeps ownership of the
/// descriptor *outside* the `catch_unwind` boundary and lends this function a
/// `&mut Option<OwnedFd>`, so a panic anywhere below does not drop the fd
/// while unwinding — the worker still has it and can deny `EIO` with the
/// original descriptor. That matters because a response is matched by fd
/// *number* (`docs/kernel-behavior-7.2/interception.md` §5.1): once the number is closed
/// it can be recycled, and answering a recycled number would answer somebody
/// else's event. Taking it here, at the exact moment it is consumed, is also
/// what makes "every path answers exactly once" checkable by reading.
fn claim(slot: &mut Option<OwnedFd>) -> OwnedFd {
    slot.take().expect("an intercepted open is answered exactly once")
}

/// Decides one intercepted open and always answers it — allow, deny, or a
/// move into a hydration job that guarantees a later answer from `finish` —
/// before returning. No path may leave `slot` full without one of those
/// three; doing so would leave the opener blocked forever (see `take_fd`).
///
/// `since` is the count of root unregistrations when the event was read (see
/// [`mark_while_hydrated`]).
///
/// # No duplicate outlives the answer
///
/// The file is inspected through duplicates of the event fd — kernel fact 1
/// rules out opening it ourselves, but not `dup()`ing one we did not open —
/// and each is closed as soon as it has been read, never held across an
/// answer. A duplicate shares the event's `O_RDWR` open file, so while one
/// is open the file counts as open for writing: the daemon's registration
/// probe, which creates a file in an already-marked root and at once takes a
/// write lease on it, found the lease refused when this function still held
/// one after it had allowed the probe's open.
pub(crate) fn handle_open(shared: &Shared, slot: &mut Option<OwnedFd>, opener_pid: i32, since: u64) {
    let meta = match metadata_of(slot.as_ref().expect("the event fd is still here").as_fd()) {
        Ok(meta) => meta,
        Err(e) => {
            tracing::error!("cannot stat an intercepted open: {e}");
            respond_deny(shared, claim(slot), libc::EIO);
            return;
        }
    };
    if !meta.is_file() {
        respond_allow(shared, claim(slot));
        return;
    }
    let owner = meta.uid();
    let dev = meta.dev();
    let ino = meta.ino();
    // Compiled in only with `fault-injection`, armed only by the VM suite
    //; an empty function otherwise. Placed after the
    // descriptor has been taken out of `slot`'s reach and before any
    // decision, so the unwind it causes is exactly the one
    // describes: the worker still owns the event fd and can answer `EIO`.
    fault::panic_on_size(meta.len());

    // The owning daemon's own opens bypass everything
    // else: it must be able to re-open files it left `hydrating` or
    // `dehydrating` during startup recovery, and treating that open like any
    // other would mean asking the very daemon that is blocked on it to
    // hydrate the file — a deadlock.
    //
    // The exemption is deliberately narrow. It is not "this pid connected to
    // our socket": anyone can do that, and an unscoped version of this check
    // let any local process be exempted from interception of any file. It is
    // "this pid holds a connection that owns a registered root, and the file
    // being opened belongs to that same user". A process with no root gets
    // nothing, and no daemon is ever exempted from another user's files.
    //
    // Dehydration (`konedrived/src/hydration/dehydrate.rs::dehydrate`) used
    // to depend on this, and deliberately no longer does: it
    // opened the file again, by path, after clearing the ignore mark, and
    // that open was let through only because it hit this exemption first.
    // It now does the whole sequence on the one descriptor it opened before
    // the first check, so it raises no open of its own at all. Nothing else
    // should acquire such a dependency: what the exemption covers is startup
    // recovery, not a way to open a file the state check would have handled.
    if daemon_is_exempt(shared, opener_pid, owner) {
        respond_allow(shared, claim(slot));
        return;
    }

    let mut state = state_of(slot.as_ref().expect("the event fd is still here").as_fd());
    // At most two turns: the second only when the file stopped reading
    // `hydrated` between the first read and the mark, and then it is not
    // `hydrated` any more.
    loop {
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
            Ok(Some(State::Hydrated)) => {
                // `fault-injection` builds only: the VM suite's I1 scenario.
                fault::delay_before_ignore_mark();
                let fd = slot.as_ref().expect("the event fd is still here").as_fd();
                match mark_while_hydrated(shared, fd, FileId { owner: Some(owner), dev, ino }, since) {
                    Ok(()) => respond_allow(shared, claim(slot)),
                    Err(now) => {
                        tracing::info!(
                            "dev={dev} ino={ino} stopped reading hydrated while its open was \
                             being decided (it now reads {now:?}); deciding again"
                        );
                        state = now;
                        continue;
                    }
                }
            }
            Ok(None) => {
                // A file with no state attribute is not ours — unless it also
                // carries an item id, in which case it is one of ours with its
                // state missing, and we have no idea whether its body is there.
                // §5.2's last rule applies: never allow zeros.
                match item_id_of(slot.as_ref().expect("the event fd is still here").as_fd()) {
                    Ok(None) => respond_allow(shared, claim(slot)),
                    Ok(Some(item)) => {
                        tracing::error!(
                            "dev={dev} ino={ino} carries item id {item} but no state attribute; \
                             denying rather than risk serving an unfilled placeholder"
                        );
                        respond_deny(shared, claim(slot), libc::EIO);
                    }
                    Err(e) => {
                        tracing::error!("cannot read the item id on dev={dev} ino={ino}: {e}");
                        respond_deny(shared, claim(slot), libc::EIO);
                    }
                }
            }
            Ok(Some(_)) => hydrate(shared, slot, owner, dev, ino, since),
            Err(StateError::Corrupt(value)) => {
                tracing::error!(
                    "dev={dev} ino={ino} has an unrecognised state {value:?}; denying rather than \
                     risk serving an unfilled placeholder"
                );
                respond_deny(shared, claim(slot), libc::EIO);
            }
            Err(StateError::Io(e)) => {
                tracing::error!("unreadable xattrs on dev={dev} ino={ino}: {e}");
                respond_deny(shared, claim(slot), libc::EIO);
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
fn mark_while_hydrated(
    shared: &Shared,
    fd: BorrowedFd<'_>,
    file: FileId,
    since: u64,
) -> Result<(), Result<Option<State>, StateError>> {
    let FileId { owner, dev, ino } = file;
    place_ignore_mark(shared, fd, dev, ino);
    let now = state_of(fd);
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
            "cannot take the ignore mark off dev={dev} ino={ino} again ({e}); it reads {now:?}"
        );
    }
    if hydrated {
        tracing::info!(
            "dev={dev} ino={ino} was let through without an ignore mark: a root was unregistered \
             while its open was being decided"
        );
        return Ok(());
    }
    Err(now)
}

/// The "ask the daemon" path: coalesce by inode, register the opener, then
/// send. Registering before sending is the whole point — a daemon that
/// answers immediately would otherwise find an empty job, finish it, and
/// leave this opener suspended with nothing left to answer it.
fn hydrate(
    shared: &Shared,
    slot: &mut Option<OwnedFd>,
    owner_uid: u32,
    dev: u64,
    ino: u64,
    since: u64,
) {
    // The descriptor stays in `slot` across the wait — the step here that
    // takes locks and sleeps, and could therefore panic on somebody else's
    // bug — so guarantee still holds over it.
    let daemon = match wait_for_daemon(shared, owner_uid) {
        Ok(daemon) => daemon,
        Err(why) => {
            // One message per refusal. All three used to print
            // "no daemon for uid X after 30s", which was caught claiming a
            // thirty-second wait for an open that was answered in 185 µs — a
            // log line that sends whoever reads it looking for a daemon that
            // was never going to be asked for.
            //
            // Throttled: each of the three can come thousands
            // at a time, and the first line of an interval keeps the uid.
            match why {
                NoDaemon::NoRoot => shared.refusals.report(Refusal::NoRoot, || {
                    format!(
                        "uid {owner_uid} has no registered root, so no daemon of theirs could \
                         hydrate this file; denying EIO without waiting"
                    )
                }),
                NoDaemon::TooManyWaiters => shared.refusals.report(Refusal::TooManyWaiters, || {
                    format!(
                        "uid {owner_uid}'s own {MAX_DAEMON_WAITERS}-waiter budget, or the \
                         machine-wide {GLOBAL_MAX_DAEMON_WAITERS}-waiter backstop, is already \
                         full; denying this open EIO at once rather than queueing behind them"
                    )
                }),
                NoDaemon::TimedOut => shared.refusals.report(Refusal::TimedOut, || {
                    format!(
                        "uid {owner_uid}'s daemon did not connect within {DAEMON_WAIT:?}; denying \
                         EIO"
                    )
                }),
            }
            respond_deny(shared, claim(slot), libc::EIO);
            return;
        }
    };
    let owner = Owner { uid: daemon.uid, conn: daemon.conn };

    // The event fd itself goes into the job, before anything is sent; the
    // daemon is sent a duplicate, made under the jobs lock (see
    // `jobs::Dispatch`). A `SCM_RIGHTS` copy of either is the same open file
    // description.
    let enrollment = lock(&shared.jobs).enroll((dev, ino), owner, claim(slot), since);
    let gone = matches!(enrollment.outcome, Enrolled::ConnectionGone);
    for stranded in enrollment.evicted {
        if gone {
            // This connection's cleanup already ran, so nothing
            // would ever answer a job created on it.
            tracing::warn!("the daemon connection went away while this open was being handled");
        } else {
            tracing::warn!(
                "a hydration of this file was in hand for another uid, which no longer owns it; \
                 denying its openers EIO"
            );
        }
        respond_deny(shared, stranded, libc::EIO);
    }
    // `New` comes with its request to send. `Queued` has none yet: the
    // opener is enrolled and stays suspended until a returning credit sends
    // it. `Existing` asked for nothing, and `ConnectionGone`
    // was answered above.
    dispatch(shared, &daemon.outbox, owner, enrollment.dispatch);
}

/// Sends a hydration request that has just been given a credit — and, if it
/// cannot be sent, answers its openers and passes the credit on, for as long
/// as the next one cannot be sent either.
///
/// Queued, never written here: a worker thread must not be able
/// to block on a socket the peer controls. The request's room in the outbox
/// is its credit, so a refusal is not a slow daemon: the
/// connection is over — `EIO`, as its disconnect guard answers everything
/// else it had — or a peer that answered a request before it was sent
/// returned a credit early, and its own openers get `EAGAIN`. A descriptor
/// that cannot be duplicated (the helper is out of them) is `EIO`, as it
/// always was.
///
/// A loop, not recursion: when every send fails, as it does once the
/// connection is over, the whole queue drains through here one hydration at
/// a time.
pub(crate) fn dispatch(shared: &Shared, outbox: &Outbox, owner: Owner, mut next: Option<jobs::Dispatch>) {
    while let Some(jobs::Dispatch { req_id, fd }) = next.take() {
        let (errno, why) = match fd {
            Ok(fd) => {
                let request = Outgoing { message: ToDaemon::HydrateRequest { req_id }, fd: Some(fd) };
                match outbox.try_send(request) {
                    Ok(()) => return,
                    Err(_) if outbox.is_closed() => (libc::EIO, "the connection is over".to_owned()),
                    Err(_) => (libc::EAGAIN, "its request capacity is taken".to_owned()),
                }
            }
            Err(e) => (libc::EIO, format!("cannot duplicate an event fd for the daemon: {e}")),
        };
        shared.refusals.report(Refusal::Undeliverable, || {
            format!(
                "a hydration request could not be queued for uid {} connection {} ({why}); \
                 denying its openers errno {errno}",
                owner.uid, owner.conn
            )
        });
        // Every opener that has joined this job is answered, not just the
        // first: they are all waiting on a request that was never delivered.
        next = settle(shared, req_id, owner, errno, Finish::Undeliverable);
    }
}

/// narrow exemption. `SO_PEERCRED` supplies the pid, so the
/// caller cannot claim to be a daemon it is not; owning a registered root is
/// what separates the user's daemon from any process that merely connected.
///
/// Only the file owner's **top** connection is exempt: the one
/// its hydrations go to, which is the daemon in every case but a transient
/// same-uid connection sitting on top of it — and that one is exempt only for
/// files of its own uid, which it can read anyway.
fn daemon_is_exempt(shared: &Shared, opener_pid: i32, file_owner: u32) -> bool {
    let on_top = lock(&shared.daemons).is_top_pid(file_owner, opener_pid);
    on_top && lock(&shared.roots).has_root_for(file_owner)
}

fn place_ignore_mark(shared: &Shared, fd: BorrowedFd<'_>, dev: u64, ino: u64) {
    // Not verified by reading `/proc/self/fdinfo/<group>`: that is O(marks)
    // per hydration, and the helper holds one mark per directory in every
    // sync tree on the machine. VM suite asserts on fdinfo instead,
    // where the cost does not matter and the assertion is worth making — the
    // syscall's return value is known to lie about this (M3).
    if let Err(e) = shared.marks.ignore_file(fd) {
        tracing::error!(
            "cannot place the ignore mark on dev={dev} ino={ino}: {e}; every open of this file \
             will keep raising a permission event"
        );
    }
}

fn respond_allow(shared: &Shared, fd: OwnedFd) {
    if let Err(e) = shared.marks.allow(fd.as_fd()) {
        tracing::error!("cannot allow an intercepted open: {e}");
    }
}

pub(crate) fn respond_deny(shared: &Shared, fd: OwnedFd, errno: i32) {
    if let Err(e) = shared.marks.deny(fd.as_fd(), errno) {
        tracing::error!("cannot deny an intercepted open: {e}");
    }
}

/// Why there is no daemon to ask. Three different facts about the system,
/// which used to be reported as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoDaemon {
    /// This uid has registered no root, so no daemon of theirs could hydrate
    /// anything. Answered immediately; nothing was waited for.
    NoRoot,
    /// Either this uid's [`MAX_DAEMON_WAITERS`] slots, or the machine-wide
    /// [`GLOBAL_MAX_DAEMON_WAITERS`] backstop, are all taken. Answered
    /// immediately; nothing was waited for.
    TooManyWaiters,
    /// Waited the full [`DAEMON_WAIT`] and no daemon connected.
    TimedOut,
}

/// Waits for the owning user's daemon to connect, up to `DAEMON_WAIT`.
/// Woken by `daemon_arrived` the instant one registers, rather than polling.
///
/// puts two limits on the waiting, because this is the only place
/// a worker sleeps for tens of seconds and therefore the only lever an
/// unprivileged caller has on the pool:
///
/// - **a user with no registered root is never waited for.** A daemon that
///   has never registered anything cannot hydrate anything either, so the
///   wait could only ever end in the same `EIO` thirty seconds later. This is
///   what stops someone parking workers by opening files belonging to a uid
///   that does not run konedrive at all.
/// - **at most [`MAX_DAEMON_WAITERS`] workers wait for any one uid at once,**
///   and **at most [`GLOBAL_MAX_DAEMON_WAITERS`] wait for any combination of
///   uids at once.** Beyond either cap the open is denied immediately rather
///   than queueing behind the others, so waiting can never consume the pool
///   and stall interception for everybody else on the machine — not for one
///   uid pinned against its own cap, and not for the machine as a whole no
///   matter how many uids are waiting at once.
///
/// The refusal says which of the three happened. `hydrate` logs it; the reason
/// never changes the answer, which is always `EIO`.
fn wait_for_daemon(shared: &Shared, uid: u32) -> Result<Daemon, NoDaemon> {
    // The overwhelmingly common case: the daemon is already there, and
    // nothing below applies.
    if let Some(daemon) = lock(&shared.daemons).top(uid) {
        return Ok(daemon.clone());
    }
    if !lock(&shared.roots).has_root_for(uid) {
        return Err(NoDaemon::NoRoot);
    }
    let Some(_slot) = WaiterSlot::take(&shared.daemon_waiters, uid) else {
        return Err(NoDaemon::TooManyWaiters);
    };

    let deadline = Instant::now() + DAEMON_WAIT;
    let mut daemons = lock(&shared.daemons);
    loop {
        if let Some(daemon) = daemons.top(uid) {
            return Ok(daemon.clone());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(NoDaemon::TimedOut);
        }
        let (guard, _) = shared
            .daemon_arrived
            .wait_timeout(daemons, deadline - now)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        daemons = guard;
    }
}

/// What brought us into [`finish`]. It changes nothing about what the function
/// does and everything about what "there is no such job" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Finish {
    /// The daemon sent `HydrateDone`.
    Reported,
    /// The helper is draining a request it could not deliver to the daemon in
    /// the first place. No `HydrateDone` was ever involved, and saying one was
    /// sends the reader looking for a message that does not exist.
    Undeliverable,
}

/// Answers every opener waiting on a finished hydration, and returns the
/// hydration its credit now goes to, whose request the caller
/// must send — see [`dispatch`].
pub(crate) fn settle(
    shared: &Shared,
    req_id: u64,
    owner: Owner,
    errno: i32,
    why: Finish,
) -> Option<jobs::Dispatch> {
    let Some(jobs::Finished { waiters, since, next }) = lock(&shared.jobs).finish(req_id, owner)
    else {
        // Unknown, already finished, or another connection's. A request id is
        // a small sequential integer, so "another connection's" is the case
        // that matters: without this check any local user could connect to the
        // 0666 socket and force-allow every suspended open in the system by
        // guessing numbers from 1 upwards.
        match why {
            Finish::Reported => shared.refusals.report(Refusal::StrayDone, || {
                format!(
                    "ignoring HydrateDone for request {req_id} from uid {} connection {}: \
                     unknown, already finished, never sent, or not this connection's",
                    owner.uid, owner.conn
                )
            }),
            Finish::Undeliverable => tracing::warn!(
                "request {req_id} for uid {} connection {} could not be sent to the daemon, and \
                 by the time it was drained it was already gone; its openers were answered \
                 elsewhere",
                owner.uid,
                owner.conn
            ),
        }
        return None;
    };
    answer(shared, req_id, waiters, errno, since);
    next
}

/// Answers the openers of one hydration with its outcome. `since` is the
/// count of root unregistrations when the open that started it was read.
fn answer(shared: &Shared, req_id: u64, waiters: Vec<OwnedFd>, errno: i32, since: u64) {
    if errno != 0 {
        let delivered = marks::clamp_deny_errno(errno);
        if delivered != errno {
            tracing::warn!(
                "the daemon reported errno {errno} for request {req_id}, which the kernel will \
                 not deliver; denying with {delivered} instead"
            );
        }
        for fd in waiters {
            respond_deny(shared, fd, delivered);
        }
        return;
    }

    // The ignore mark goes on only after the file's state has
    // been read again, from the event fd itself — the exact inode the opener
    // is about to get, with no path in between and so nothing to race. A
    // hydration that reports success but does not leave the file `hydrated`
    // has not put the content there as far as we can tell, and §5.2 is
    // unconditional about what happens then.
    //
    // The mark then goes through `mark_while_hydrated`, like every other
    // mark: a dehydration that began after the read above
    // takes it off again here, and a hydration that began before a root was
    // unregistered leaves no mark behind it.
    let Some(first) = waiters.first() else { return };
    let verdict = state_of(first.as_fd());
    let verdict = match verdict {
        Ok(Some(State::Hydrated)) => {
            let file = file_of(first.as_fd());
            mark_while_hydrated(shared, first.as_fd(), file, since).map_err(|now| {
                tracing::error!(
                    "request {req_id}: the file read hydrated, and then {now:?} once it was \
                     marked — a dehydration began in between"
                );
                now
            })
        }
        other => Err(other),
    };
    match verdict {
        Ok(()) => {
            for fd in waiters {
                respond_allow(shared, fd);
            }
        }
        Err(other) => {
            tracing::error!(
                "request {req_id} was reported successful but the file does not read as \
                 hydrated ({other:?}); denying EIO rather than risk serving zeros"
            );
            for fd in waiters {
                respond_deny(shared, fd, libc::EIO);
            }
        }
    }
}

/// Reads the state of the inode behind an event fd, through a duplicate so
/// that nothing here can close the descriptor that still owes a response.
/// The duplicate is closed before this returns (see `handle_open` on m1).
fn state_of(fd: BorrowedFd<'_>) -> Result<Option<State>, StateError> {
    let probe = File::from(fd.try_clone_to_owned()?);
    read_state(&probe)
}

/// [`state_of`] for the item id.
fn item_id_of(fd: BorrowedFd<'_>) -> io::Result<Option<String>> {
    read_item_id(&File::from(fd.try_clone_to_owned()?))
}

/// [`state_of`] for `fstat`.
fn metadata_of(fd: BorrowedFd<'_>) -> io::Result<std::fs::Metadata> {
    File::from(fd.try_clone_to_owned()?).metadata()
}

/// Who owns the file behind a descriptor, and which inode it is.
#[derive(Debug, Clone, Copy)]
struct FileId {
    /// `None` when it could not be read: then every unregistration since
    /// counts against the file.
    owner: Option<u32>,
    dev: u64,
    ino: u64,
}

/// [`FileId`] behind an event fd. The inode is for log lines only, and is
/// zeros if it cannot be read.
fn file_of(fd: BorrowedFd<'_>) -> FileId {
    match metadata_of(fd) {
        Ok(meta) => FileId { owner: Some(meta.uid()), dev: meta.dev(), ino: meta.ino() },
        Err(_) => FileId { owner: None, dev: 0, ino: 0 },
    }
}

#[cfg(test)]
mod tests;
