//! The daemon's stop (issue #84): on SIGTERM or SIGINT nothing new is sent,
//! and the requests in flight get up to [`STOP_BOUND`] to return — an opened
//! upload session to be persisted, a fragment to be answered — before the
//! process exits. What is still in flight then is cut, as before: the
//! recorded place of a new file's session covers it
//! (`docs/design/writes.md` §6.1). A second signal exits at once.

use std::future::Future;
use std::time::Duration;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_stop_never_waits_beyond_the_bound() {
        let started = tokio::time::Instant::now();
        let ended = wind_down(std::future::pending(), STOP_BOUND, std::future::pending()).await;
        assert_eq!(ended, Ended::Bound);
        assert_eq!(started.elapsed(), STOP_BOUND);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_signal_ends_the_wait_at_once() {
        let started = tokio::time::Instant::now();
        let ended = wind_down(std::future::pending(), STOP_BOUND, async {}).await;
        assert_eq!(ended, Ended::Again);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_ends_as_soon_as_what_is_in_flight_finished() {
        let started = tokio::time::Instant::now();
        let ended = wind_down(tokio::time::sleep(Duration::from_secs(2)), STOP_BOUND, std::future::pending()).await;
        assert_eq!(ended, Ended::Finished);
        assert_eq!(started.elapsed(), Duration::from_secs(2));
    }
}
