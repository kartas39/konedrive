use std::time::Duration;

use konedrive_graph::pool::Throughput;

use super::*;
use crate::status::snapshot::SyncSnapshot;

fn wait_of(seconds_left: u32) -> (ThrottleWait, SyncStateHandle) {
    let state = SyncStateHandle::new(SyncSnapshot::default());
    state.set_throughput(Throughput { retry_after: seconds_left, ..Throughput::default() });
    (ThrottleWait::new(state.clone()), state)
}

fn publish(state: &SyncStateHandle, seconds_left: u32) {
    state.set_throughput(Throughput { retry_after: seconds_left, ..Throughput::default() });
}

#[test]
fn a_program_is_refused_while_the_wait_runs_and_background_work_never() {
    assert_eq!(decide_content_request(ContentAsker::Program, 0), ContentRequest::Asked, "no wait runs");
    assert_eq!(decide_content_request(ContentAsker::Program, 300), ContentRequest::Refused { seconds_left: 300 }, "a wait runs and a program waits");
    assert_eq!(decide_content_request(ContentAsker::Program, 1), ContentRequest::Refused { seconds_left: 1 }, "to the last second of it");
    assert_eq!(decide_content_request(ContentAsker::BackgroundWork, 300), ContentRequest::Asked, "a wait runs and nobody waits");
    assert_eq!(decide_content_request(ContentAsker::BackgroundWork, 0), ContentRequest::Asked);
}

#[tokio::test]
async fn an_asking_under_way_is_given_up_when_the_wait_starts() {
    let (throttle_wait, state) = wait_of(0);
    assert_eq!(throttle_wait.ask_unless_refused(ContentAsker::Program, async { 7 }).await, Ok(7), "no wait runs: the answer");
    let never = std::future::pending::<()>();
    let asked = tokio::spawn({
        let throttle_wait = throttle_wait.clone();
        async move { throttle_wait.ask_unless_refused(ContentAsker::Program, never).await }
    });
    tokio::task::yield_now().await;
    assert!(!asked.is_finished());
    publish(&state, 300);
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), asked).await.expect("given up at once").unwrap(), Err(300));
    // One that runs already refuses an asking that would answer at once.
    assert_eq!(throttle_wait.ask_unless_refused(ContentAsker::Program, async { 7 }).await, Err(300));
}

#[tokio::test]
async fn an_asking_of_background_work_goes_on_through_the_wait() {
    let (throttle_wait, state) = wait_of(300);
    assert_eq!(throttle_wait.ask_unless_refused(ContentAsker::BackgroundWork, async { 7 }).await, Ok(7));
    let (answer, answered) = tokio::sync::oneshot::channel::<u8>();
    let asked = tokio::spawn({
        let throttle_wait = throttle_wait.clone();
        async move { throttle_wait.ask_unless_refused(ContentAsker::BackgroundWork, answered).await }
    });
    publish(&state, 299);
    tokio::task::yield_now().await;
    assert!(!asked.is_finished());
    answer.send(9).unwrap();
    assert_eq!(asked.await.unwrap().map(Result::unwrap), Ok(9));
}

#[test]
fn the_first_file_refused_is_kept_for_the_wait_and_gone_with_it() {
    let (throttle_wait, state) = wait_of(0);
    assert!(!throttle_wait.is_refusal_untold(), "no wait runs");
    assert!(!throttle_wait.tell_refusal("/r/a.xlsx"), "nor is anything kept then");
    assert_eq!(state.get().transfers.retry_after_refusal, "");

    publish(&state, 300);
    assert_eq!(throttle_wait.seconds_left(), 300);
    assert!(throttle_wait.is_refusal_untold());
    assert!(throttle_wait.tell_refusal("/r/a.xlsx"));
    assert!(!throttle_wait.is_refusal_untold());
    assert!(throttle_wait.tell_refusal("/r/b.docx"), "a wait runs");
    publish(&state, 120);
    assert_eq!(state.get().transfers.retry_after_refusal, "/r/a.xlsx", "the first, for the whole wait");

    publish(&state, 0);
    assert_eq!(state.get().transfers.retry_after_refusal, "", "gone with the wait");
    publish(&state, 30);
    assert!(throttle_wait.is_refusal_untold(), "the next wait has its own");
}
