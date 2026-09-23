//! A bounded pool of threads that answer permission events.
//!
//! One thread per event was not survivable. A permission event suspends the
//! opener until it is answered, and answering can take as long as a download,
//! so the thread count follows however many opens happen to be in flight —
//! which an ordinary `grep -r` across a sync folder pushes into the thousands.
//! `std::thread::spawn` **panics** when the system will not give it another
//! thread, that panic unwinds out of the event loop and kills the process, the
//! process's death closes the fanotify group, and `fanotify(7)` is explicit
//! about what that means: *"Upon close(2), outstanding permission events will
//! be set to allowed"*. Every suspended open in the system is then allowed at
//! once, and every one of them reads a placeholder that was never filled. The
//! failure mode of running out of threads was handing applications zeros.
//!
//! A fixed pool with a bounded queue cannot do that. When the queue is full
//! the caller gets the event back and denies it with `EAGAIN` — a real answer,
//! one the kernel accepts (M2), and one that means "try that again" to the
//! application rather than "this file is empty".

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use crate::Shared;

/// One suspended open, on its way to a worker.
pub struct OpenEvent {
    /// The exact descriptor `read_events()` handed out — see `take_fd`.
    pub fd: OwnedFd,
    pub pid: i32,
    /// The helper's count of root unregistrations when the event was read
    /// (`main.rs`, `mark_while_hydrated`).
    pub since: u64,
}

pub struct Pool {
    queue: SyncSender<OpenEvent>,
}

impl Pool {
    pub fn new(shared: Arc<Shared>, threads: usize, depth: usize) -> io::Result<Self> {
        let (queue, receiver) = sync_channel::<OpenEvent>(depth);
        let receiver: Arc<Mutex<Receiver<OpenEvent>>> = Arc::new(Mutex::new(receiver));
        for n in 0..threads {
            let receiver = Arc::clone(&receiver);
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(format!("konedrive-open-{n}"))
                .spawn(move || loop {
                    // The lock is held only across `recv`, and `recv` returns
                    // at once whenever there is anything queued, so workers
                    // hand off rather than queue up behind each other. Taken
                    // poison-tolerantly for the same reason the shared maps
                    // are: a worker that panicked must not take the rest of
                    // the pool with it.
                    let next = receiver
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv();
                    let Ok(event) = next else {
                        // Every sender is gone: the event loop has stopped.
                        break;
                    };
                    // Ruling H37. The descriptor stays here, outside the
                    // unwind boundary, so a panic below cannot drop it while
                    // unwinding: this thread still owns the exact fd number
                    // the kernel handed out, and a response is matched by
                    // number, so a duplicate or a recycled number would not
                    // do. The worker then answers EIO and carries on, instead
                    // of dying and leaving the pool one thread weaker with
                    // an opener suspended forever.
                    let mut slot = Some(event.fd);
                    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        crate::handle_open(&shared, &mut slot, event.pid, event.since);
                    }));
                    if outcome.is_err() {
                        tracing::error!(
                            "a worker panicked while deciding an intercepted open; denying it \
                             EIO and carrying on"
                        );
                    }
                    // Non-empty only when the decision did not get as far as
                    // answering — a panic, since every ordinary path takes it.
                    if let Some(fd) = slot {
                        if let Err(e) = shared.marks.deny(fd.as_fd(), libc::EIO) {
                            tracing::error!("cannot deny an intercepted open: {e}");
                        }
                    }
                })?;
        }
        Ok(Self { queue })
    }

    /// Hands one event to a worker, or gives it straight back when there is no
    /// room. The event comes back rather than being dropped because dropping
    /// an event fd without writing a response leaves its opener suspended
    /// until the helper exits.
    pub fn submit(&self, event: OpenEvent) -> Result<(), OpenEvent> {
        match self.queue.try_send(event) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(event)) | Err(TrySendError::Disconnected(event)) => Err(event),
        }
    }
}
