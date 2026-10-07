//! One intercepted open that still owes its opener an answer.
//!
//! A permission event that is read and never answered leaves its opener
//! suspended until the fanotify group closes (`fanotify(7)`), which in
//! practice means until the helper exits. [`PendingOpen`] is what makes
//! "every open is answered exactly once" a property of the type: it owns the
//! event's descriptor, [`allow`](PendingOpen::allow) and
//! [`deny`](PendingOpen::deny) consume it, and one that is dropped with no
//! answer denies `EIO`. Nothing else in the helper holds an event's
//! descriptor as a bare `OwnedFd`.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use nix::sys::fanotify::FanotifyEvent;

use crate::errno::Errno;
use crate::marks::Marks;

/// A suspended open: the event's descriptor and the group it is answered
/// through.
///
/// The descriptor is lent out ([`AsFd`]) for what is done before the answer:
/// reading the file's state, placing its ignore mark, duplicating it for the
/// daemon. A borrow cannot close it, and it stays open until the answer is
/// written: a response is matched by fd *number*
/// (`docs/kernel-behavior-7.2/interception.md` §5.1), and a number that is
/// closed can be reused, so answering it would answer somebody else's event.
pub struct PendingOpen {
    fd: OwnedFd,
    marks: Arc<Marks>,
    answered: bool,
}

impl PendingOpen {
    /// Takes ownership of a permission event's fd, preserving its exact
    /// number. `None` for an event that carries no descriptor.
    ///
    /// `fanotify_write()` matches a permission response against the fd number
    /// `read_events()` handed out for that event (`fanotify(7)`: "fd — This is
    /// the file descriptor from the structure fanotify_event_metadata"). A
    /// duplicate has a different number, so anything answered only after
    /// this event's iteration of the read loop ends — the "ask the daemon and
    /// wait" path — must keep using this exact descriptor, not a dup of it.
    ///
    /// `FanotifyEvent::drop` would close this fd when the event goes out of
    /// scope at the end of the read loop's iteration; `mem::forget` disarms that
    /// so the `OwnedFd` built from the same raw number is the sole owner.
    pub fn take(event: FanotifyEvent, marks: &Arc<Marks>) -> Option<Self> {
        let raw = event.fd()?.as_raw_fd();
        std::mem::forget(event);
        // SAFETY: `event.fd()` returned a valid, open descriptor owned by
        // `event`; forgetting `event` just above means nothing else will close
        // it, so this `OwnedFd` becomes its sole owner.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        marks.owes_one_more();
        Some(Self { fd, marks: Arc::clone(marks), answered: false })
    }

    /// Lets the open through.
    pub fn allow(mut self) {
        self.answered = true;
        if let Err(e) = self.marks.allow(self.fd.as_fd()) {
            tracing::error!("cannot allow an intercepted open: {e}");
        }
    }

    /// Fails the open with `errno`, clamped to what the kernel delivers
    /// ([`Marks::deny`]).
    pub fn deny(mut self, errno: Errno) {
        self.answered = true;
        self.write_denial(errno);
    }

    fn write_denial(&self, errno: Errno) {
        if let Err(e) = self.marks.deny(self.fd.as_fd(), errno) {
            tracing::error!("cannot deny an intercepted open: {e}");
        }
    }
}

impl AsFd for PendingOpen {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// An open nobody answered is denied `EIO`, never left suspended and never
/// allowed: this is what answers the open a worker panicked over, as the
/// unwind drops it, and whatever a later change forgets.
impl Drop for PendingOpen {
    fn drop(&mut self) {
        if !self.answered {
            // A panic has its own line, where it is caught.
            if !std::thread::panicking() {
                tracing::error!("an intercepted open was dropped with no answer; denying it EIO");
            }
            // Contained: this can run while a panic unwinds, where a second
            // panic let out of a destructor would end the process, and with
            // it release every suspended open as allowed.
            let _ = catch_unwind(AssertUnwindSafe(|| self.write_denial(Errno::EIO)));
        }
        // After the answer is written, whichever way it was: the helper's
        // stop exits once this count is zero, and an open counted out before
        // its answer would be released by that exit as allowed.
        self.marks.owes_one_less();
    }
}
