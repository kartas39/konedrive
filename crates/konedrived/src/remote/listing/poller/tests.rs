use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::*;
use crate::remote::listing::*;
use crate::remote::testing::feed::{folder, root_item};
use crate::remote::testing::{World, PATIENCE};
use crate::status::snapshot::LiveChanges;
use konedrive_tree::Table;

/// A refresh runs a cycle now, not at the next poll; and the poller stops.
#[tokio::test]
async fn the_poller_runs_again_on_refresh_and_stops() {
    let s = World::read_only().await;
    s.feed(None, json!([root_item()]), "L1").await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [], "@odata.deltaLink": s.link_to("L1")})))
        .mount(&s.graph.server).await;
    let poller = Poller::start(s.listing(), Schedule::polled(Duration::from_secs(3600), vec![]));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.delta_requests().await, 1, "the first cycle runs at once");
    poller.refresh();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.delta_requests().await, 2, "Refresh() runs another now, not in an hour");
    tokio::time::timeout(Duration::from_secs(5), poller.stop()).await.expect("stop returns");
}

/// Issue #54: while the notification socket is up the poll waits `live_interval`; once it
/// goes down, the poll runs at its normal `interval` again.
#[tokio::test]
async fn the_poll_waits_longer_while_the_socket_is_up_and_not_once_it_drops() {
    let s = World::read_only().await;
    s.feed(None, json!([root_item()]), "L1").await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [], "@odata.deltaLink": s.link_to("L1")})))
        .mount(&s.graph.server).await;
    let live = crate::remote::live::Timing {
        debounce: Duration::from_millis(100),
        backoff: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        // The fake pings every 25 s: up after this instead.
        settle: Duration::from_millis(100),
        ..Default::default()
    };
    let schedule = Schedule { live_interval: Duration::from_secs(3600), live: Some(live), ..Schedule::polled(Duration::from_millis(400), vec![]) };
    let poller = Poller::start(s.listing(), schedule);
    let mut seen = s.state.subscribe();
    tokio::time::timeout(PATIENCE, seen.wait_for(|state| state.live_changes == LiveChanges::Connected)).await.expect("the socket comes up").unwrap();
    // A cycle that was due as the socket came up may still run.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let while_up = s.delta_requests().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(s.delta_requests().await, while_up, "no cycle at the normal interval while the socket is up");

    s.graph.sockets.refuse(true);
    s.graph.sockets.drop_all();
    tokio::time::timeout(PATIENCE, seen.wait_for(|state| state.live_changes == LiveChanges::Connecting)).await.expect("the socket is down").unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(s.delta_requests().await >= while_up + 2, "the normal interval again: {} then, {} now", while_up, s.delta_requests().await);
    tokio::time::timeout(Duration::from_secs(5), poller.stop()).await.expect("stop returns");
}

/// Forget stops the sync before it takes the lifecycle lock for writing,
/// but whoever holds that lock must never make a stop wait for it. The
/// cycle asks Graph without the lock, and changes nothing without it.
#[tokio::test]
async fn stopping_does_not_wait_for_the_lifecycle_lock() {
    let s = World::read_only().await;
    s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
    let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
    let held = Arc::clone(&lifecycle).write_owned().await;
    let listing = Listing::new(ListingContext { lease: crate::remote::listing::Lease::on(&lifecycle), ..s.context() });
    let poller = Poller::start(listing, Schedule::polled(Duration::from_secs(3600), vec![]));
    let docs = s.root.path.join("docs");
    let mut staged = false;
    for _ in 0..100 {
        staged = s.store.call(|t| t.get(Table::Staging, "D")).await.unwrap().is_some();
        if staged || docs.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!docs.exists(), "the folder is not changed without the lock");
    assert!(staged, "Graph is asked, and its answer staged, without the lock");
    assert_eq!(s.delta_requests().await, 1);
    tokio::time::timeout(Duration::from_secs(2), poller.stop()).await.expect("stop does not wait for the lock's holder");
    drop(held);
}

/// Nor for Graph: a request that hangs is dropped, not waited out.
#[tokio::test]
async fn stopping_does_not_wait_for_a_slow_answer_from_graph() {
    let s = World::read_only().await;
    s.feed_after(None, json!([root_item()]), "L1", Duration::from_secs(30)).await;
    let poller = Poller::start(s.listing(), Schedule::polled(Duration::from_secs(3600), vec![]));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.delta_requests().await, 1, "the listing has asked");
    assert!(s.state.get().listing);
    tokio::time::timeout(Duration::from_secs(2), poller.stop()).await.expect("stop does not wait for the answer");
    assert!(!s.state.get().listing, "a stopped listing is not said to run");
}
