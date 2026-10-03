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
