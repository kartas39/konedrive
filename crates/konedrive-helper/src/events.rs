//! The answers to intercepted opens, in three files: the loop that reads
//! the fanotify group (here), what an open is answered (`decision`), and
//! asking the owner's daemon for the content (`hydration`).

mod decision;
mod hydration;

use std::os::fd::AsFd;
use std::sync::Arc;

use konedrive_helper::pending::PendingOpen;
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::fanotify::MaskFlags;

pub(crate) use decision::handle_open;
pub(crate) use hydration::{dispatch, settle, Finish};

use crate::pool;
use crate::shared::{
    Refusal, Shared, Throttle, EVENT_FD_FAILED, EVENT_QUEUE_DEPTH, EVENT_WORKERS,
    EXHAUSTION_BACKOFF, UNOPENABLE,
};

/// The target of every line this module logs, whichever of its files writes
/// it.
const LOG: &str = module_path!();

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
                let Some(open) = PendingOpen::take(event, &shared.marks) else {
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
                    open.allow();
                    continue;
                }
                if let Err(rejected) = pool.submit(pool::OpenEvent { open, pid, since }) {
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
                    rejected.open.deny(libc::EAGAIN);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
