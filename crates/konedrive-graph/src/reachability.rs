//! Whether Microsoft answers the account's requests ([`Reachability`]), for whoever holds
//! back what can wait while nothing answers.
//!
//! **State owned:** whether the account's requests go unanswered now, and when the last
//! answer came. Changed by [`DriveClient`](crate::drive::DriveClient)'s one way out
//! (`drive::send`) alone, at the end of each try of a request: an answer of any status —
//! a `500` is an answer — or none, the request could not be sent or nothing came back (no
//! network, a name that does not resolve, a connection refused or timed out). The access
//! token a request is sent with is part of the try: a token endpoint that does not answer
//! (`AuthError::Unreachable`) is a try with no answer.
//!
//! **A request that began before the last answer came says nothing when it goes
//! unanswered**: it was sent into a network that may be back since, and only an answer
//! is a proof. An answer always counts, whenever its request began.
//!
//! **The observer** (one, set by whoever holds work back: the daemon's sync of the
//! folder) is called at once with what holds, and then at every change, under this
//! part's lock, so that it is told the changes in the order they were made: it may ask
//! nothing of this `Reachability`.
//!
//! **Tested without the rest:**
// test: an_answer_of_any_status_makes_microsoft_reachable
// test: a_request_with_no_answer_makes_microsoft_unreachable
// test: a_request_that_began_before_the_last_answer_says_nothing
// test: the_observer_is_told_at_once_and_at_every_change_alone

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// What is told of every change: whether Microsoft is unreachable now.
pub type ReachObserver = Arc<dyn Fn(bool) + Send + Sync>;

/// How one try of a request ended, for what it says of whether Microsoft answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestEnd {
    /// An answer came, whatever its status.
    Answered,
    /// No answer came to the try that began at `began`.
    Unanswered { began: Instant },
}

/// What the account's requests said last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Reach {
    is_unreachable: bool,
    /// When the last answer came.
    answered_at: Option<Instant>,
}

/// What `reach` becomes when a try ends as `end` at `now`: an answer makes Microsoft
/// reachable; a try with no answer makes it unreachable, unless an answer came after the
/// try began. Pure.
fn decide_reach(reach: Reach, end: RequestEnd, now: Instant) -> Reach {
    match end {
        RequestEnd::Answered => Reach { is_unreachable: false, answered_at: Some(now) },
        RequestEnd::Unanswered { began } if reach.answered_at.is_some_and(|answered_at| answered_at > began) => reach,
        RequestEnd::Unanswered { .. } => Reach { is_unreachable: true, ..reach },
    }
}

/// Whether Microsoft answers one account's requests.
#[derive(Default)]
pub struct Reachability {
    inner: Mutex<(Reach, Option<ReachObserver>)>,
}

impl Reachability {
    /// Reachable until a request says otherwise.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Whether the account's requests go unanswered now.
    pub fn is_unreachable(&self) -> bool {
        crate::lock(&self.inner).0.is_unreachable
    }

    /// A try of a request ended as `end`. Called by `drive::send`, and by a test that
    /// stands for it.
    pub fn note(&self, end: RequestEnd) {
        self.note_at(end, Instant::now());
    }

    fn note_at(&self, end: RequestEnd, now: Instant) {
        let mut inner = crate::lock(&self.inner);
        let (reach, observer) = &mut *inner;
        let was_unreachable = reach.is_unreachable;
        *reach = decide_reach(*reach, end, now);
        if reach.is_unreachable != was_unreachable {
            if let Some(observer) = observer {
                observer(reach.is_unreachable);
            }
        }
    }

    /// Where the changes go from now on, in place of any observer before: `observer` is
    /// called at once with what holds.
    pub fn set_observer(&self, observer: ReachObserver) {
        let mut inner = crate::lock(&self.inner);
        observer(inner.0.is_unreachable);
        inner.1 = Some(observer);
    }
}

#[cfg(test)]
mod tests;
