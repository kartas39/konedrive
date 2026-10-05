use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use super::*;
use crate::remote::listing::*;
use crate::remote::testing::feed::{folder, root_item};
use crate::remote::testing::World;
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

/// While the notification socket is up the poll waits `live_interval`; once it
/// goes down, the next cycle is due `interval` after the last one. The poller's task is run
/// with the test's own "socket is up" channel, in the live task's place.
#[tokio::test]
async fn the_poll_waits_longer_while_the_socket_is_up_and_not_once_it_drops() {
    let s = World::read_only().await;
    s.feed(None, json!([root_item()]), "L1").await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [], "@odata.deltaLink": s.link_to("L1")})))
        .mount(&s.graph.server).await;
    let schedule = Schedule { live_interval: Duration::from_secs(3600), ..Schedule::polled(Duration::from_millis(400), vec![]) };
    let (up, seen) = watch::channel(true);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run(s.listing(), schedule, Arc::new(Notify::new()), cancel.clone(), seen));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(s.delta_requests().await, 1, "the first cycle, then the live interval");
    up.send_replace(false);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.delta_requests().await, 2, "overdue by the normal interval: a cycle at once");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.delta_requests().await, 3, "and every normal interval after");
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("the task ends").unwrap();
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
    assert!(s.state.get().cycle.listing);
    tokio::time::timeout(Duration::from_secs(2), poller.stop()).await.expect("stop does not wait for the answer");
    assert!(!s.state.get().cycle.listing, "a stopped listing is not said to run");
}
