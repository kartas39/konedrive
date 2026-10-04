//! The daemon's stop (issue #84): on SIGTERM or SIGINT nothing new is sent,
//! and the requests in flight get up to [`STOP_BOUND`] to return — an opened
//! upload session to be persisted, a fragment to be answered — before the
//! process exits. What is still in flight then is cut, as before: the
//! recorded place of a new file's session covers it
//! (`docs/design/writes.md` §6.1). A second signal exits at once.
//!
//! The daemon stops the same way, and exits with a failure, when one of the
//! tasks it runs for its whole life is gone ([`Tasks`], quality finding
//! `SY11`): systemd then starts it again (`Restart=on-failure`).

use std::future::Future;
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use futures_util::future::BoxFuture;

/// How long a stop waits for the requests in flight: systemd's own
/// `TimeoutStopSec` is far longer. A guess (limitations log F177).
pub const STOP_BOUND: Duration = Duration::from_secs(10);

/// How a stop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// Everything in flight finished.
    Finished,
    /// The bound passed first.
    Bound,
    /// A second signal came first.
    Again,
}

/// Waits for `finished` — at most `bound`, and not past `again`.
pub async fn wind_down(finished: impl Future<Output = ()>, bound: Duration, again: impl Future<Output = ()>) -> Ended {
    tokio::select! {
        _ = finished => Ended::Finished,
        _ = tokio::time::sleep(bound) => Ended::Bound,
        _ = again => Ended::Again,
    }
}

/// The next SIGTERM or SIGINT.
pub struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

impl Signals {
    /// Installs the handlers: from now on the signals no longer end the
    /// process by themselves.
    pub fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(Self { term: signal(SignalKind::terminate())?, int: signal(SignalKind::interrupt())? })
    }

    pub async fn next(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.int.recv() => {}
        }
    }
}

/// How long the daemon needs a task of [`Tasks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// For the daemon's whole life: the daemon cannot do its work once the task has
    /// ended, however it ended.
    Always,
    /// While the task has something to follow: it may end by itself (no system bus, a
    /// source whose signals ended), having said so, and the daemon goes on without it.
    /// Only a panic of it is a failure.
    WhileItCan,
}

/// A task of [`Tasks`] that is gone, and the daemon cannot go on without.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Died {
    /// What the task was, in words.
    pub name: &'static str,
    /// The panic's message, or `None` for a task that returned.
    pub panic: Option<String>,
}

impl std::fmt::Display for Died {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.panic {
            Some(panic) => write!(f, "{} panicked: {panic}", self.name),
            None => write!(f, "{} ended", self.name),
        }
    }
}

/// The tasks the daemon runs beside its accounts, for its whole life: each one spawned
/// here and kept, so that the end of one is seen.
#[derive(Default)]
pub struct Tasks {
    running: FuturesUnordered<BoxFuture<'static, Option<Died>>>,
}

impl Tasks {
    /// Starts `task` as a task of its own.
    pub fn spawn(&mut self, name: &'static str, need: Need, task: impl Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(task);
        self.running.push(Box::pin(async move {
            match handle.await {
                Ok(()) if need == Need::WhileItCan => None,
                Ok(()) => Some(Died { name, panic: None }),
                Err(e) if e.is_panic() => Some(Died { name, panic: Some(panic_message(e.into_panic())) }),
                // Cancelled: the runtime is going down, and the daemon with it.
                Err(_) => None,
            }
        }));
    }

    /// The first task to go that the daemon needs. Never returns while none has: a task
    /// that ended as it may is passed over.
    pub async fn died(&mut self) -> Died {
        while let Some(ended) = self.running.next().await {
            if let Some(died) = ended {
                return died;
            }
        }
        std::future::pending().await
    }
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    match panic.downcast::<String>() {
        Ok(message) => *message,
        Err(panic) => panic.downcast::<&'static str>().map(|message| (*message).to_owned()).unwrap_or_else(|_| "no message".into()),
    }
}

#[cfg(test)]
mod tests;
