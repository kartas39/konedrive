//! The poller over a real cycle, the fixture's store and the tests' fake OneDrive, on the
//! real clock with short waits: a request to the fake is real I/O, which a paused clock
//! would run ahead of.

use std::sync::atomic::Ordering;
use std::time::Duration;

use konedrive_tree::FullListingCause;

use super::*;
use crate::conditions::running::{Settings, SystemClock};
use crate::remote::listing_cycle::tests::ListedFolder;

const LONG: Duration = Duration::from_secs(3600);

/// How many cycles asked OneDrive for a page so far.
fn count_page_requests(folder: &ListedFolder) -> usize {
    folder.graph.with(|cloud| cloud.count("GET", "root/delta"))
}

/// Waits until `is_so` answers yes, for ten seconds at most.
async fn wait_until(what: &str, mut is_so: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !is_so() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never: {what}"));
}

/// Waits until a cycle went through whole: `LastChecked` is written.
async fn wait_for_first_cycle(folder: &ListedFolder) {
    wait_until("a first cycle ended", || folder.state.get().cycle.last_checked > 0).await;
}

fn start(folder: &ListedFolder, schedule: Schedule) -> ListingPoller {
    ListingPoller::start(Arc::clone(&folder.cycle), schedule, Arc::new(Running::default()))
}

#[tokio::test]
async fn a_cycle_runs_at_once_and_then_every_interval() {
    let folder = ListedFolder::start().await;
    let poller = start(&folder, Schedule::polled(Duration::from_millis(30), Vec::new()));

    wait_until("three cycles asked for a page", || count_page_requests(&folder) >= 3).await;

    poller.stop().await;
}

#[tokio::test]
async fn a_refresh_runs_a_cycle_now() {
    let folder = ListedFolder::start().await;
    let poller = start(&folder, Schedule::polled(LONG, Vec::new()));
    wait_for_first_cycle(&folder).await;
    let asked = count_page_requests(&folder);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(count_page_requests(&folder), asked, "no cycle runs before the interval");

    poller.handle().refresh();

    wait_until("the refresh ran a cycle", || count_page_requests(&folder) > asked).await;
    poller.stop().await;
}

#[tokio::test]
async fn a_failed_cycle_is_tried_again_on_the_retry_schedule() {
    let folder = ListedFolder::start().await;
    // The delta link is refused once: the cycle fails, and the link is kept.
    folder.graph.with(|cloud| cloud.script("GET", "root/delta", wiremock::ResponseTemplate::new(400), 1));
    let poller = start(&folder, Schedule::polled(LONG, vec![Duration::from_millis(20)]));

    wait_for_first_cycle(&folder).await;

    assert_eq!(folder.state.get().cycle.sync_trouble, None);
    poller.stop().await;
}

#[tokio::test]
async fn a_full_refresh_asks_the_store_for_a_full_listing_before_the_cycle() {
    let folder = ListedFolder::start().await;
    let poller = start(&folder, Schedule::polled(LONG, Vec::new()));
    wait_for_first_cycle(&folder).await;
    let asked = count_page_requests(&folder);
    // The caller asks the store; the handle only wakes the poller.
    folder.fixture.write_rows(|store| store.item_store().request_full_listing(FullListingCause::UserAsked));

    poller.handle().refresh_after_full_listing_request();

    wait_until("the full listing was begun", || !folder.fixture.read_rows(|reader| reader.is_full_listing_requested())).await;
    wait_until("it ended", || folder.fixture.read_rows(|reader| reader.find_listing_under_way()).is_none()).await;
    assert!(count_page_requests(&folder) > asked);
    // From the start, with no token: a full listing.
    assert!(folder.graph.server.received_requests().await.expect("the requests").last().is_some_and(|request| request.url.path() == "/me/drive/root/delta" && request.url.query().is_none()));
    poller.stop().await;
}

#[tokio::test]
async fn a_stop_cancels_the_cycle_between_two_requests_and_waits_for_the_page_being_written() {
    let folder = ListedFolder::start().await;
    folder.graph.with(|cloud| cloud.delay("GET", "root/delta", Duration::from_secs(60), 1));
    let poller = start(&folder, Schedule::polled(LONG, Vec::new()));
    let handle = poller.handle();
    wait_until("the cycle asked for its page", || count_page_requests(&folder) == 1).await;

    tokio::time::timeout(Duration::from_secs(10), poller.stop()).await.expect("the stop does not wait for OneDrive");

    // The cycle was under way and wrote nothing; its listing is the store's to go on with.
    assert_eq!(folder.state.get().cycle.last_checked, 0);
    assert!(folder.fixture.read_rows(|reader| reader.find_listing_under_way()).is_some());
    // Asking a stopped poller does nothing.
    handle.refresh();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(count_page_requests(&folder), 1);
}

#[tokio::test]
async fn no_cycle_runs_while_the_accounts_work_is_stopped() {
    let folder = ListedFolder::start().await;
    let running = Arc::new(Running::new(Settings { paused_until: Some(0), ..Settings::default() }, Arc::new(SystemClock)));
    let poller = ListingPoller::start(Arc::clone(&folder.cycle), Schedule::polled(Duration::from_millis(20), Vec::new()), Arc::clone(&running));

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(folder.graph.server.received_requests().await.expect("the requests").is_empty(), "OneDrive is not asked while paused");

    running.change(|settings| settings.paused_until = None);
    wait_for_first_cycle(&folder).await;
    poller.stop().await;
}

#[test]
fn a_handle_with_no_poller_counts_the_cycles_it_is_asked_for() {
    let (handle, asked_cycle_count) = ListingPollHandle::for_tests();

    handle.refresh();
    handle.refresh_after_full_listing_request();
    handle.wake_live();

    assert_eq!(asked_cycle_count.load(Ordering::SeqCst), 2);
}
