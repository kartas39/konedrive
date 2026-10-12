//! The wait OneDrive asked an account for with a throttle (`429` or `503` and a
//! `Retry-After`), as whoever asks OneDrive for content on behalf of a program sees it.
//!
//! **State owned:** none of its own. The seconds left are the transfer pool's, as it last
//! published them into the folder's state (`TransferStatus::throughput`, `retry_after`); the
//! file first refused during the wait is the folder's state too
//! (`TransferStatus::retry_after_refusal`), written here and taken off with the wait
//! (`SyncStateHandle::set_throughput`).
//!
//! **Answers for:** a program not being kept waiting for the whole wait. The pool hands out
//! no slot until the wait is over, and a request OneDrive refused sleeps it out; a program
//! that reads a file is answered at once instead ([`decide_content_request`]), and what
//! it asked under way is given up the moment the wait starts
//! ([`ThrottleWait::ask_unless_refused`]). The work nobody waits for — the plan's
//! downloads — waits the time out as before.
//!
//! **Tested without the rest**, with a folder's state the test holds:
// test: a_program_is_refused_while_the_wait_runs_and_background_work_never
// test: an_asking_under_way_is_given_up_when_the_wait_starts
// test: an_asking_of_background_work_goes_on_through_the_wait
// test: the_first_file_refused_is_kept_for_the_wait_and_gone_with_it

use std::future::Future;

use crate::status::snapshot::SyncStateHandle;

/// Who waits for the content that is asked of OneDrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentAsker {
    /// A program: a read, an open for writing, a truncation, `Files.Hydrate`.
    Program,
    /// Nobody: the plan's download of a pinned file.
    BackgroundWork,
}

/// What becomes of a request for content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentRequest {
    /// OneDrive is asked.
    Asked,
    /// It is refused at once: OneDrive asked the account to wait, and this many seconds
    /// of the wait are left.
    Refused { seconds_left: u32 },
}

/// What becomes of a request for content that `asker` waits for while `seconds_left` of a
/// throttle's wait are left (0: no wait runs). Pure.
///
/// - No wait runs: OneDrive is asked, whoever waits.
/// - A wait runs and a program waits: refused. The program is told now and asks again
///   after the wait.
/// - A wait runs and nobody waits: asked. The pool holds the request back until the wait
///   is over.
pub fn decide_content_request(asker: ContentAsker, seconds_left: u32) -> ContentRequest {
    match (asker, seconds_left) {
        (_, 0) | (ContentAsker::BackgroundWork, _) => ContentRequest::Asked,
        (ContentAsker::Program, seconds_left) => ContentRequest::Refused { seconds_left },
    }
}

/// The throttle's wait of one account. Cheap to clone; every clone reads the same state.
#[derive(Clone)]
pub struct ThrottleWait {
    state: SyncStateHandle,
}

impl ThrottleWait {
    /// The wait as `state`, the folder's published state, has it.
    pub fn new(state: SyncStateHandle) -> Self {
        Self { state }
    }

    /// Seconds left of the wait, as the pool last published them; 0 when none runs.
    pub fn seconds_left(&self) -> u32 {
        self.state.read(|s| s.transfers.throughput.retry_after)
    }

    /// Waits until a request for content `asker` waits for is refused
    /// ([`decide_content_request`]) — at once while a wait runs — and says the seconds
    /// left of the wait. For ever for background work, which is never refused, and when
    /// the state is gone.
    pub async fn wait_for_refusal(&self, asker: ContentAsker) -> u32 {
        let mut published = self.state.subscribe();
        let is_refused = |seconds_left| matches!(decide_content_request(asker, seconds_left), ContentRequest::Refused { .. });
        let refused = published.wait_for(|s| is_refused(s.transfers.throughput.retry_after)).await.map(|s| s.transfers.throughput.retry_after);
        match refused {
            Ok(seconds_left) => seconds_left,
            Err(_) => std::future::pending().await,
        }
    }

    /// The answer of `asking`, which asks OneDrive for content `asker` waits for; or, for
    /// a program, the seconds left of a wait that runs already or starts before `asking`
    /// has answered: `asking` is dropped then, so it must be safe to drop where it
    /// waits.
    pub async fn ask_unless_refused<T>(&self, asker: ContentAsker, asking: impl Future<Output = T>) -> Result<T, u32> {
        tokio::select! {
            biased;
            seconds_left = self.wait_for_refusal(asker) => Err(seconds_left),
            answer = asking => Ok(answer),
        }
    }

    /// Whether a wait runs and no file refused during it is told yet: whoever refuses a
    /// read looks the file's path up only then.
    pub fn is_refusal_untold(&self) -> bool {
        self.state.read(|s| s.transfers.throughput.retry_after > 0 && s.transfers.retry_after_refusal.is_empty())
    }

    /// A program was refused the content of the file at `path` because of the wait: kept
    /// as the wait's refusal when it is the first of this wait, and said in the journal
    /// then. Whether a wait runs: with none, nothing is kept.
    pub fn tell_refusal(&self, path: &str) -> bool {
        let mut seconds_left = 0;
        let mut is_first = false;
        self.state.update_if_changed(|s| {
            seconds_left = s.transfers.throughput.retry_after;
            if seconds_left > 0 && s.transfers.retry_after_refusal.is_empty() {
                s.transfers.retry_after_refusal = path.to_owned();
                is_first = true;
            }
        });
        if is_first {
            tracing::warn!("OneDrive asked to wait {seconds_left} s: {path} is not read from OneDrive until then, nor is any other file that is not on this computer");
        }
        seconds_left > 0
    }
}

#[cfg(test)]
mod tests;
