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

/// Quality finding `SY11`: a task the daemon needs for its whole life is seen to go,
/// whether it panicked or returned; one that may end by itself is passed over when it
/// returns, and seen when it panics.
#[tokio::test(start_paused = true)]
async fn a_task_that_is_gone_is_named_and_one_that_may_end_is_not() {
    let mut tasks = Tasks::default();
    tasks.spawn("the watcher", Need::WhileItCan, async {});
    tasks.spawn("the supervisor", Need::Always, std::future::pending());
    let died = tokio::time::timeout(Duration::from_millis(50), tasks.died()).await;
    assert!(died.is_err(), "nothing the daemon needs has gone: {died:?}");

    tasks.spawn("the state's watch", Need::Always, async {});
    assert_eq!(tasks.died().await, Died { name: "the state's watch", panic: None });

    tasks.spawn("the network's watcher", Need::WhileItCan, async { panic!("no route") });
    let died = tasks.died().await;
    assert_eq!(died, Died { name: "the network's watcher", panic: Some("no route".into()) });
    assert_eq!(died.to_string(), "the network's watcher panicked: no route");
}

/// What stops the daemon: a signal, with no failure, or a task it needs that is gone.
#[tokio::test(start_paused = true)]
async fn the_daemon_stops_on_a_signal_or_when_a_task_it_needs_is_gone() {
    let mut tasks = Tasks::default();
    tasks.spawn("the supervisor", Need::Always, std::future::pending());
    assert_eq!(stopped(async {}, &mut tasks).await, None);

    tasks.spawn("the state's watch", Need::Always, async {});
    assert_eq!(stopped(std::future::pending(), &mut tasks).await, Some(Died { name: "the state's watch", panic: None }));
}
