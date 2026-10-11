use std::sync::Arc;

use futures_util::FutureExt;

use super::*;

fn causes(causes: &[WakeCause]) -> BTreeSet<WakeCause> {
    causes.iter().copied().collect()
}

#[tokio::test]
async fn a_cause_given_before_the_wait_is_answered_at_once() {
    let wake = SyncPlanWake::new();
    wake.wake(WakeCause::StepFinished);
    assert_eq!(wake.wait().now_or_never(), Some(causes(&[WakeCause::StepFinished])));
}

#[tokio::test]
async fn causes_given_while_nobody_waits_are_answered_together_and_once() {
    let wake = SyncPlanWake::new();
    wake.wake(WakeCause::ListingPageWritten);
    wake.wake(WakeCause::LocalVersionWritten);
    wake.wake(WakeCause::ListingPageWritten);
    assert_eq!(wake.wait().now_or_never(), Some(causes(&[WakeCause::LocalVersionWritten, WakeCause::ListingPageWritten])));
    assert_eq!(wake.wait().now_or_never(), None, "what was taken is not answered again");
    // A wait that was given up loses nothing of what comes after it.
    wake.wake(WakeCause::QuotaRead);
    assert_eq!(wake.wait().now_or_never(), Some(causes(&[WakeCause::QuotaRead])));
}

#[tokio::test]
async fn a_wait_with_nothing_given_ends_when_a_cause_is_given() {
    let wake = Arc::new(SyncPlanWake::new());
    let waiting = Arc::clone(&wake);
    let waiter = tokio::spawn(async move { waiting.wait().await });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished(), "nothing was given yet");
    // From a thread that is no task's: a write operation of the mount.
    let waking = Arc::clone(&wake);
    std::thread::spawn(move || waking.wake(WakeCause::LocalVersionWritten)).join().unwrap();
    assert_eq!(waiter.await.unwrap(), causes(&[WakeCause::LocalVersionWritten]));
}
