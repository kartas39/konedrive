//! The listing cycle over the fixture's store and the tests' fake OneDrive: no mount, and
//! nothing of the folder but its rows.

use std::sync::Arc;
use std::time::Duration;

use konedrive_tree::{ConfirmedOneDriveVersion, FullListingCause, Item, ItemPlace, ItemVersion, Store, TreeStore};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::*;
use crate::mount::item_fixture::{ItemFixture, DRIVE_ID};
use crate::status::snapshot::SyncSnapshot;
use crate::fake_onedrive::FakeGraph;

/// A cycle over the fixture's store and a fake OneDrive whose drive is the store's.
pub(in crate::listing) struct ListedFolder {
    pub(in crate::listing) fixture: ItemFixture,
    pub(in crate::listing) graph: FakeGraph,
    pub(in crate::listing) state: SyncStateHandle,
    pub(in crate::listing) cycle: Arc<ListingCycle>,
    /// How many of the requests so far a test has put behind it ([`ListedFolder::forget_requests`]).
    forgotten_request_count: std::sync::atomic::AtomicUsize,
}

pub(in crate::listing) fn make_cycle(store: &Store, graph: &FakeGraph, sync_plan_wake: &Arc<SyncPlanWake>, state: &SyncStateHandle) -> Arc<ListingCycle> {
    ListingCycle::new(ListingCycleSetup {
        drive: graph.client(),
        store: store.clone(),
        sync_plan_wake: Arc::clone(sync_plan_wake),
        state: state.clone(),
        drive_record: None,
        neighbours: None,
        report: Report::nowhere(),
        after_listing: None,
    })
}

impl ListedFolder {
    /// The fixture as it is made — its root listed, the delta link the fake's — and a
    /// fake OneDrive that got, after that link, the files `a.txt` and `b.txt` and the
    /// directory `docs` with `docs/c.txt`: the rows do not have them yet.
    pub(in crate::listing) async fn start() -> Self {
        let fixture = ItemFixture::new();
        let graph = fixture.start_fake_onedrive().await;
        graph.with(|cloud| {
            cloud.add_file("a.txt", "R", "a.txt", b"a");
            cloud.add_file("b.txt", "R", "b.txt", b"b");
            cloud.add(crate::fake_onedrive::FakeItem { id: "docs".into(), parent: Some("R".into()), name: "docs".into(), folder: true, content: Vec::new(), hash: None, size: 0, etag: "e-docs".into(), ctag: "c-docs".into(), mtime: 0 });
            cloud.add_file("docs!c.txt", "docs", "c.txt", b"c");
        });
        Self::over(fixture, graph)
    }

    /// The same four items, listed into the rows before the fake OneDrive began: the rows
    /// and OneDrive agree, and the store's delta link says nothing changed.
    async fn start_listed() -> Self {
        let fixture = ItemFixture::new();
        fixture.list_onedrive_file("a.txt", b"a");
        fixture.list_onedrive_file("b.txt", b"b");
        fixture.list_onedrive_directory("docs");
        fixture.list_onedrive_file("docs/c.txt", b"c");
        let graph = fixture.start_fake_onedrive().await;
        Self::over(fixture, graph)
    }

    fn over(fixture: ItemFixture, graph: FakeGraph) -> Self {
        let state = SyncStateHandle::new(SyncSnapshot::default());
        let cycle = make_cycle(&fixture.store, &graph, &fixture.sync_plan_wake, &state);
        ListedFolder { fixture, graph, state, cycle, forgotten_request_count: std::sync::atomic::AtomicUsize::new(0) }
    }

    async fn run(&self) -> Result<ListingCycleReport, CycleError> {
        self.cycle.run(&CancellationToken::new()).await
    }

    /// The next requests for a page answer `pages` in turn: each the items of the drive
    /// named, the root first; each but the last with a next link `…/delta/page-N` (N from
    /// 2), the last with the fake's delta link.
    fn script_pages(&self, pages: &[&[&str]]) {
        for (index, page) in pages.iter().enumerate() {
            self.script_page(page, (index + 1 < pages.len()).then(|| index + 2));
        }
    }

    /// The next request for a page that no earlier script answers is answered with
    /// [`page_answer`](Self::page_answer).
    fn script_page(&self, items: &[&str], next: Option<usize>) {
        let answer = self.page_answer(items, next);
        self.graph.with(|cloud| cloud.script("GET", "root/delta", answer, 1));
    }

    /// A page of the items named as the drive has them; `next`: the number of the page
    /// its next link names, or `None` for the delta link.
    fn page_answer(&self, items: &[&str], next: Option<usize>) -> ResponseTemplate {
        let whole = self.graph.with(|cloud| cloud.delta_body());
        let value = self.page_items(items);
        ResponseTemplate::new(200).set_body_json(match next {
            Some(page) => json!({ "value": value, "@odata.nextLink": self.page_link(page) }),
            None => json!({ "value": value, "@odata.deltaLink": whole["@odata.deltaLink"] }),
        })
    }

    /// The items named, as the drive has them now and a page would say them.
    fn page_items(&self, items: &[&str]) -> Vec<Value> {
        let whole = self.graph.with(|cloud| cloud.delta_body());
        items.iter().map(|id| whole["value"].as_array().expect("items").iter().find(|item| item["id"] == *id).unwrap_or_else(|| panic!("the drive has no {id}")).clone()).collect()
    }

    /// [`page_items`](Self::page_items) as the cycle classifies them.
    fn page_changes(&self, items: &[&str]) -> Vec<Change> {
        self.page_items(items).into_iter().map(|item| classify(&serde_json::from_value(item).expect("an item"))).collect()
    }

    /// The rows say OneDrive has `ghost.txt`, which it does not have.
    fn list_ghost(&self) {
        self.fixture.list_onedrive_file("ghost.txt", b"ghost");
        assert_eq!(self.listed_name("ghost.txt").as_deref(), Some("ghost.txt"));
    }

    fn page_link(&self, page: usize) -> String {
        format!("{}/me/drive/root/delta/page-{page}", self.graph.server.uri())
    }

    /// Every request for a page or an item since the last
    /// [`forget_requests`](Self::forget_requests), in order: the path below `me/drive/`,
    /// with the query's token where there is one.
    async fn requests(&self) -> Vec<String> {
        let forgotten = self.forgotten_request_count.load(std::sync::atomic::Ordering::SeqCst);
        self.every_request().await.split_off(forgotten)
    }

    async fn every_request(&self) -> Vec<String> {
        let received = self.graph.server.received_requests().await.expect("the requests");
        received
            .iter()
            .filter_map(|request| {
                let below = request.url.path().strip_prefix("/me/drive/")?.to_owned();
                Some(match request.url.query_pairs().find(|(key, _)| key == "token") {
                    Some((_, token)) => format!("{below}?{token}"),
                    None => below,
                })
            })
            .collect()
    }

    async fn forget_requests(&self) {
        self.forgotten_request_count.store(self.every_request().await.len(), std::sync::atomic::Ordering::SeqCst);
    }

    fn item(&self, onedrive_id: &str) -> Option<Item> {
        let onedrive_id = onedrive_id.to_owned();
        self.fixture.list_items().into_iter().find(|item| item.onedrive_item.as_ref().is_some_and(|reference| reference.item_id.as_str() == onedrive_id))
    }

    /// The name OneDrive has the item under, as the rows say; `None` when the rows say
    /// OneDrive does not have it.
    fn listed_name(&self, onedrive_id: &str) -> Option<String> {
        self.item(onedrive_id)?.onedrive?.place.map(|place| place.name.to_string_lossy().into_owned())
    }

    fn listing_under_way(&self) -> Option<ListingUnderWay> {
        self.fixture.read_rows(|reader| reader.find_listing_under_way())
    }

    fn delta_link(&self) -> Option<String> {
        self.fixture.write_rows(|store| store.delta_link())
    }

    fn fake_delta_link(&self) -> String {
        self.graph.with(|cloud| cloud.delta_body())["@odata.deltaLink"].as_str().expect("a delta link").to_owned()
    }

    /// A step's rename of the item to `name` is confirmed, under an eTag no listing gives.
    fn confirm_rename(&self, onedrive_id: &str, name: &str) -> u64 {
        let item = self.item(onedrive_id).expect("the item");
        let place = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()).expect("its place in OneDrive");
        let confirmed = ConfirmedOneDriveVersion::Present { version: ItemVersion { place: Some(ItemPlace { parent_local_item_id: place.parent_local_item_id, name: name.into() }), content: item.onedrive.and_then(|onedrive| onedrive.content) }, etag: format!("e-ours-{name}") };
        self.fixture.write_rows(move |store| store.item_store().confirm_onedrive_version(item.local_item_id, &confirmed))
    }

    fn count(&self) -> u64 {
        self.fixture.write_rows(|store| store.item_store().confirmation_count())
    }

    /// A full listing stopped after its first page (the root and `a.txt`): the cycle was
    /// cancelled while its request for the second page was under way.
    async fn start_stopped_after_first_page() -> Self {
        let folder = Self::start().await;
        folder.fixture.write_rows(|store| store.item_store().request_full_listing(FullListingCause::UserAsked));
        folder.script_page(&["R", "a.txt"], Some(2));
        folder.graph.with(|cloud| cloud.delay("GET", "delta/page-2", Duration::from_secs(30), 1));
        let cancel = CancellationToken::new();
        let (cycle, token) = (Arc::clone(&folder.cycle), cancel.clone());
        let running = tokio::spawn(async move { cycle.run(&token).await });
        folder.wait_for_request("root/delta/page-2").await;
        cancel.cancel();
        assert!(matches!(running.await.expect("the cycle's task"), Err(CycleError::Cancelled)));
        folder.forget_requests().await;
        folder
    }

    async fn wait_for_request(&self, request: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.requests().await.iter().any(|asked| asked == request) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no request for {request}"));
    }
}

fn gone(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "error": { "code": code, "message": code } }))
}

#[tokio::test]
async fn a_store_with_no_delta_link_begins_a_full_listing() {
    // A store that knows its drive and was never listed into: no fixture has one.
    let tree_store = TreeStore::in_memory().expect("a store");
    tree_store.set_drive_id(DRIVE_ID).expect("the drive");
    let store = Store::new(tree_store);
    let folder = ListedFolder::start().await;
    let (wake, state) = (Arc::new(SyncPlanWake::new()), SyncStateHandle::new(SyncSnapshot::default()));
    let cycle = make_cycle(&store, &folder.graph, &wake, &state);
    folder.script_pages(&[&["R", "a.txt"], &["b.txt", "docs"], &["docs!c.txt"]]);

    let report = cycle.run(&CancellationToken::new()).await.expect("the cycle");

    assert_eq!(report, ListingCycleReport { kind: ListingKind::Full, pages: 3, changes: 5, read_again: 0 });
    assert_eq!(folder.requests().await, ["root/delta", "root/delta/page-2", "root/delta/page-3"]);
    let (under_way, delta_link, counts, last_checked) = store.call(|store| Ok((store.item_store().find_listing_under_way()?, store.delta_link()?, store.read_items(|reader| reader.count_items())?, store.last_checked()?))).await.expect("the store");
    assert_eq!((under_way, delta_link), (None, Some(folder.fake_delta_link())));
    assert_eq!(counts.in_onedrive, 4);
    let published = state.get().cycle;
    assert_eq!((published.listing, published.sync_trouble), (false, None));
    assert!(last_checked.is_some_and(|at| at > 0) && published.last_checked > 0);
}

#[tokio::test]
async fn a_store_with_a_delta_link_lists_the_changes_since_it() {
    let folder = ListedFolder::start_listed().await;
    let link = folder.delta_link().expect("a delta link");
    folder.graph.with(|cloud| cloud.rename("a.txt", "R", "a2.txt"));

    let report = folder.run().await.expect("the cycle");

    assert_eq!((report.kind, report.pages, report.read_again), (ListingKind::Delta, 1, 0));
    assert!(report.changes >= 1);
    assert_eq!(folder.requests().await, [format!("root/delta?{}", link.rsplit_once('=').expect("a token").1)]);
    assert_eq!(folder.listed_name("a.txt").as_deref(), Some("a2.txt"));
    assert_eq!((folder.listing_under_way(), folder.delta_link()), (None, Some(folder.fake_delta_link())));
}

#[tokio::test]
async fn a_cycle_cancelled_between_two_pages_leaves_the_link_of_the_last_page_written() {
    let folder = ListedFolder::start_stopped_after_first_page().await;

    let under_way = folder.listing_under_way().expect("the listing is under way");
    assert_eq!((under_way.kind, under_way.next_link), (ListingKind::Full, Some(folder.page_link(2))));
    assert_eq!((folder.listed_name("a.txt").as_deref(), folder.item("b.txt")), (Some("a.txt"), None));
    assert_eq!(folder.delta_link(), None);
    // The trouble of a cycle that was stopped is nobody's trouble.
    assert_eq!(folder.state.get().cycle.sync_trouble, None);
    let causes = tokio::time::timeout(Duration::from_secs(5), folder.fixture.sync_plan_wake.wait()).await.expect("the plan is woken");
    assert_eq!(causes.into_iter().collect::<Vec<_>>(), [WakeCause::ListingPageWritten]);
}

#[tokio::test]
async fn a_full_listing_that_was_stopped_goes_on_from_the_link_the_store_kept() {
    let folder = ListedFolder::start_stopped_after_first_page().await;
    let stopped = folder.listing_under_way().expect("the listing is under way");
    folder.script_pages(&[&["b.txt", "docs", "docs!c.txt"]]);

    let report = folder.run().await.expect("the cycle");

    assert_eq!(report, ListingCycleReport { kind: ListingKind::Full, pages: 1, changes: 3, read_again: 0 });
    assert_eq!(folder.requests().await, ["root/delta/page-2"]);
    assert_eq!(folder.listing_under_way(), None);
    assert_eq!((folder.listed_name("a.txt").as_deref(), folder.listed_name("docs!c.txt").as_deref()), (Some("a.txt"), Some("c.txt")));
    // The page was given with the id of the listing that was stopped: no other was begun.
    assert_eq!(folder.fixture.write_rows(|store| store.item_store().begin_delta_listing()).id.to_string().parse::<u64>().expect("a number"), stopped.id.to_string().parse::<u64>().expect("a number") + 1);
}

#[tokio::test]
async fn a_listing_of_changes_that_was_stopped_is_asked_again_from_the_delta_link() {
    let folder = ListedFolder::start_listed().await;
    let link = folder.delta_link().expect("a delta link");
    // A listing of changes was begun and never ended.
    let stopped = folder.fixture.write_rows(|store| store.item_store().begin_delta_listing());
    folder.graph.with(|cloud| cloud.rename("b.txt", "R", "b2.txt"));

    let report = folder.run().await.expect("the cycle");

    assert_eq!(report.kind, ListingKind::Delta);
    assert_eq!(folder.requests().await, [format!("root/delta?{}", link.rsplit_once('=').expect("a token").1)]);
    assert_eq!((folder.listed_name("b.txt").as_deref(), folder.listing_under_way()), (Some("b2.txt"), None));
    // The stopped one was given up: the next listing has an id two after it.
    let next = folder.fixture.write_rows(|store| store.item_store().begin_delta_listing());
    assert_eq!(next.id.to_string().parse::<u64>().expect("a number"), stopped.id.to_string().parse::<u64>().expect("a number") + 2);
}

#[tokio::test]
async fn a_full_listing_asked_for_is_begun_though_the_store_has_a_delta_link() {
    let folder = ListedFolder::start_listed().await;
    // The rows say OneDrive has an item it does not have: only a full listing says so.
    folder.list_ghost();
    assert_eq!(folder.run().await.expect("a listing of changes").kind, ListingKind::Delta);
    assert_eq!(folder.listed_name("ghost.txt").as_deref(), Some("ghost.txt"));
    folder.fixture.write_rows(|store| store.item_store().request_full_listing(FullListingCause::UserAsked));
    folder.forget_requests().await;

    let report = folder.run().await.expect("the cycle");

    assert_eq!(report.kind, ListingKind::Full);
    assert_eq!(folder.requests().await, ["root/delta"]);
    assert_eq!(folder.listed_name("ghost.txt"), None);
    assert!(!folder.fixture.read_rows(|reader| reader.is_full_listing_requested()));
    assert_eq!(folder.run().await.expect("the next cycle").kind, ListingKind::Delta);
}

/// Answers a request for a page with what the drive had before, after a step's rename of
/// `a.txt` was confirmed in the store: the confirmation comes while the fetch is under way.
struct ConfirmedWhileFetched {
    store: Store,
    item: konedrive_tree::LocalItemId,
    confirmed: ConfirmedOneDriveVersion,
    stale_page: ResponseTemplate,
}

impl wiremock::Respond for ConfirmedWhileFetched {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        let (store, item, confirmed) = (self.store.clone(), self.item, self.confirmed.clone());
        std::thread::scope(|scope| scope.spawn(move || store.call_blocking(move |store| store.item_store().confirm_onedrive_version(item, &confirmed))).join()).expect("the thread").expect("the confirmation");
        self.stale_page.clone()
    }
}

#[tokio::test]
async fn each_page_is_written_with_the_count_read_before_its_fetch() {
    let folder = ListedFolder::start_listed().await;
    let item = folder.item("a.txt").expect("a.txt");
    // Another client renamed the file; the page says so. Then this folder's own rename
    // went through, which the page does not say and OneDrive has.
    folder.graph.with(|cloud| cloud.rename("a.txt", "R", "theirs.txt"));
    let stale_page = folder.page_answer(&["a.txt"], None);
    folder.graph.with(|cloud| cloud.rename("a.txt", "R", "ours.txt"));
    let place = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()).expect("its place");
    let confirmed = ConfirmedOneDriveVersion::Present { version: ItemVersion { place: Some(ItemPlace { parent_local_item_id: place.parent_local_item_id, name: "ours.txt".into() }), content: item.onedrive.and_then(|onedrive| onedrive.content) }, etag: "e-ours".into() };
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).respond_with(ConfirmedWhileFetched { store: folder.fixture.store.clone(), item: item.local_item_id, confirmed, stale_page }).up_to_n_times(1).mount(&folder.graph.server).await;

    let report = folder.run().await.expect("the cycle");

    // The page was given with the count from before the confirmation: the store left the
    // row, and the item was read again.
    assert_eq!(report, ListingCycleReport { kind: ListingKind::Delta, pages: 1, changes: 1, read_again: 1 });
    assert_eq!(folder.listed_name("a.txt").as_deref(), Some("ours.txt"));
    let requests = folder.requests().await;
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(requests[1], "items/a.txt");
    assert_eq!(folder.listing_under_way(), None);
}

#[tokio::test]
async fn an_item_the_store_answers_is_outdated_is_read_again_and_given_with_the_same_link() {
    let folder = ListedFolder::start_listed().await;
    let listing = folder.fixture.write_rows(|store| store.item_store().begin_delta_listing());
    let changes = folder.page_changes(&["a.txt"]);
    assert_eq!(folder.confirm_rename("a.txt", "ours.txt"), listing.began_at_count + 1);
    folder.graph.with(|cloud| cloud.rename("a.txt", "R", "ours.txt"));
    let page = FetchedPage { changes, link: ListingLink::DeltaLink("the-same-link".into()), fetch_began_at_count: listing.began_at_count };

    let written = folder.cycle.write_page(listing.id, page, &CancellationToken::new()).await.expect("the page");

    assert_eq!(written, PageWrite::LinkWritten { read_again: 1 });
    assert_eq!(folder.requests().await, ["items/a.txt"]);
    assert_eq!(folder.listed_name("a.txt").as_deref(), Some("ours.txt"));
    assert_eq!((folder.listing_under_way(), folder.delta_link().as_deref()), (None, Some("the-same-link")));
}

#[tokio::test]
async fn an_outdated_item_that_is_gone_in_onedrive_is_given_back_as_gone() {
    let folder = ListedFolder::start_listed().await;
    let listing = folder.fixture.write_rows(|store| store.item_store().begin_delta_listing());
    let changes = folder.page_changes(&["a.txt"]);
    folder.confirm_rename("a.txt", "ours.txt");
    folder.graph.with(|cloud| cloud.trash("a.txt"));
    let page = FetchedPage { changes, link: ListingLink::DeltaLink("the-same-link".into()), fetch_began_at_count: listing.began_at_count };

    let written = folder.cycle.write_page(listing.id, page, &CancellationToken::new()).await.expect("the page");

    assert_eq!(written, PageWrite::LinkWritten { read_again: 1 });
    assert_eq!(folder.listed_name("a.txt"), None);
    assert_eq!(folder.delta_link().as_deref(), Some("the-same-link"));
}

#[tokio::test]
async fn no_next_page_is_asked_before_the_link_was_written() {
    let folder = ListedFolder::start_listed().await;
    let item = folder.item("a.txt").expect("a.txt");
    folder.fixture.write_rows(|store| store.item_store().request_full_listing(FullListingCause::UserAsked));
    let first_page = folder.page_answer(&["R", "a.txt"], Some(2));
    folder.graph.with(|cloud| cloud.rename("a.txt", "R", "ours.txt"));
    let place = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()).expect("its place");
    let confirmed = ConfirmedOneDriveVersion::Present { version: ItemVersion { place: Some(ItemPlace { parent_local_item_id: place.parent_local_item_id, name: "ours.txt".into() }), content: item.onedrive.and_then(|onedrive| onedrive.content) }, etag: "e-ours".into() };
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).respond_with(ConfirmedWhileFetched { store: folder.fixture.store.clone(), item: item.local_item_id, confirmed, stale_page: first_page }).up_to_n_times(1).mount(&folder.graph.server).await;
    folder.script_pages(&[&["b.txt", "docs", "docs!c.txt"]]);

    let report = folder.run().await.expect("the cycle");

    assert_eq!(report, ListingCycleReport { kind: ListingKind::Full, pages: 2, changes: 5, read_again: 1 });
    assert_eq!(folder.requests().await, ["root/delta", "items/a.txt", "root/delta/page-2"]);
    assert_eq!(folder.listed_name("a.txt").as_deref(), Some("ours.txt"));
}

#[tokio::test]
async fn a_410_begins_a_full_listing_in_the_same_cycle() {
    let folder = ListedFolder::start_listed().await;
    let link = folder.delta_link().expect("a delta link");
    folder.list_ghost();
    folder.graph.with(|cloud| cloud.script("GET", "root/delta", gone(410, "resyncRequired"), 1));

    let report = folder.run().await.expect("the cycle");

    assert_eq!(report.kind, ListingKind::Full);
    assert_eq!(folder.requests().await, [format!("root/delta?{}", link.rsplit_once('=').expect("a token").1), "root/delta".to_owned()]);
    assert_eq!((folder.listed_name("ghost.txt"), folder.listing_under_way()), (None, None));
    assert_eq!(folder.listed_name("a.txt").as_deref(), Some("a.txt"));
}

#[tokio::test]
async fn a_resume_link_onedrive_refuses_begins_a_full_listing() {
    let folder = ListedFolder::start_stopped_after_first_page().await;
    let stopped = folder.listing_under_way().expect("the listing is under way");
    folder.graph.with(|cloud| cloud.script("GET", "delta/page-2", gone(404, "itemNotFound"), 1));

    let report = folder.run().await.expect("the cycle");

    assert_eq!(report.kind, ListingKind::Full);
    assert_eq!(folder.requests().await, ["root/delta/page-2", "root/delta"]);
    assert_eq!((folder.listing_under_way(), folder.delta_link()), (None, Some(folder.fake_delta_link())));
    assert_eq!(folder.listed_name("docs!c.txt").as_deref(), Some("c.txt"));
    // Another listing than the one that was stopped.
    assert!(folder.fixture.write_rows(|store| store.item_store().begin_delta_listing()).id > stopped.id);
}

#[tokio::test]
async fn a_delta_link_onedrive_refuses_fails_the_cycle_and_is_kept() {
    let folder = ListedFolder::start_listed().await;
    let link = folder.delta_link().expect("a delta link");
    folder.graph.with(|cloud| cloud.rename("a.txt", "R", "a2.txt"));
    folder.graph.with(|cloud| cloud.script("GET", "root/delta", gone(404, "itemNotFound"), 1));

    let failed = folder.run().await;

    assert!(matches!(failed, Err(CycleError::Offline(_))), "{failed:?}");
    assert_eq!(folder.delta_link(), Some(link.clone()));
    assert_eq!(folder.listing_under_way().map(|under_way| under_way.kind), Some(ListingKind::Delta));
    assert_eq!(folder.requests().await, [format!("root/delta?{}", link.rsplit_once('=').expect("a token").1)]);

    assert_eq!(folder.run().await.expect("the next cycle").kind, ListingKind::Delta);
    assert_eq!(folder.listed_name("a.txt").as_deref(), Some("a2.txt"));
}

#[tokio::test]
async fn a_next_link_of_this_cycle_that_onedrive_refuses_fails_the_cycle_and_is_resumed_from() {
    let folder = ListedFolder::start().await;
    folder.fixture.write_rows(|store| store.item_store().request_full_listing(FullListingCause::UserAsked));
    folder.script_page(&["R", "a.txt"], Some(2));
    folder.graph.with(|cloud| cloud.script("GET", "delta/page-2", gone(404, "itemNotFound"), 1));

    let failed = folder.run().await;

    assert!(matches!(failed, Err(CycleError::Offline(_))), "{failed:?}");
    let trouble = folder.state.get().cycle.sync_trouble.expect("the trouble is said");
    assert!(!trouble.blocking);
    assert_eq!(folder.listing_under_way().expect("the listing is under way").next_link, Some(folder.page_link(2)));

    folder.forget_requests().await;
    folder.script_pages(&[&["b.txt", "docs", "docs!c.txt"]]);
    folder.run().await.expect("the next cycle");
    assert_eq!(folder.requests().await, ["root/delta/page-2"]);
    assert_eq!((folder.listing_under_way(), folder.state.get().cycle.sync_trouble), (None, None));
}

#[tokio::test]
async fn the_plan_is_woken_after_every_page_and_when_the_listing_ends() {
    let folder = ListedFolder::start().await;
    let after_listing = Arc::new(tokio::sync::Notify::new());
    let cycle = ListingCycle::new(ListingCycleSetup { drive: folder.graph.client(), store: folder.fixture.store.clone(), sync_plan_wake: Arc::clone(&folder.fixture.sync_plan_wake), state: folder.state.clone(), drive_record: None, neighbours: None, report: Report::nowhere(), after_listing: Some(Arc::clone(&after_listing)) });

    cycle.run(&CancellationToken::new()).await.expect("the cycle");

    let causes = tokio::time::timeout(Duration::from_secs(5), folder.fixture.sync_plan_wake.wait()).await.expect("the plan is woken");
    assert_eq!(causes.into_iter().collect::<Vec<_>>(), [WakeCause::ListingPageWritten, WakeCause::ListingEnded]);
    tokio::time::timeout(Duration::from_secs(5), after_listing.notified()).await.expect("the thumbnail filler is nudged");
}

#[tokio::test]
async fn the_cycle_takes_no_tree_lock() {
    let folder = ListedFolder::start().await;
    let _held = folder.fixture.tree_lock.lock().await;

    let report = tokio::time::timeout(Duration::from_secs(20), folder.run()).await.expect("the cycle does not wait for the lock").expect("the cycle");

    assert_eq!(report.kind, ListingKind::Delta);
    assert_eq!(folder.listed_name("docs!c.txt").as_deref(), Some("c.txt"));
}

#[tokio::test]
async fn a_page_of_a_listing_the_store_gave_up_is_dropped_without_a_fault() {
    let folder = ListedFolder::start_listed().await;
    let given_up = folder.fixture.write_rows(|store| store.item_store().begin_delta_listing());
    let later = folder.fixture.write_rows(|store| store.item_store().begin_delta_listing());
    let before = folder.fixture.list_items();
    let page = FetchedPage { changes: vec![Change::Delete("a.txt".into())], link: ListingLink::DeltaLink("a-link-of-the-listing-given-up".into()), fetch_began_at_count: given_up.began_at_count };

    let written = folder.cycle.write_page(given_up.id, page, &CancellationToken::new()).await.expect("no fault");

    assert_eq!(written, PageWrite::ListingGivenUp);
    assert_eq!(folder.fixture.list_items(), before);
    assert_eq!(folder.listing_under_way().map(|under_way| under_way.id), Some(later.id));
    assert_ne!(folder.delta_link().as_deref(), Some("a-link-of-the-listing-given-up"));
}

#[tokio::test]
async fn a_page_the_store_refuses_of_the_listing_under_way_fails_the_cycle() {
    let folder = ListedFolder::start_listed().await;
    let listing = folder.fixture.write_rows(|store| store.item_store().begin_delta_listing());
    // A count the store has not reached.
    let page = FetchedPage { changes: Vec::new(), link: ListingLink::DeltaLink("link".into()), fetch_began_at_count: folder.count() + 7 };

    let refused = folder.cycle.write_page(listing.id, page, &CancellationToken::new()).await;

    assert!(matches!(refused, Err(CycleError::Store(_))), "{refused:?}");
}

#[test]
fn a_listing_is_given_up_when_another_or_none_is_under_way() {
    let folder = ItemFixture::new();
    let first = folder.write_rows(|store| store.item_store().begin_delta_listing());
    let second = folder.write_rows(|store| store.item_store().begin_delta_listing());
    assert!(!is_listing_given_up(first.id, Some(first.id)));
    assert!(is_listing_given_up(first.id, Some(second.id)));
    assert!(is_listing_given_up(first.id, None));
}

/// Holds the request it answers until the test lets it go, and says that it came: a way
/// to stand inside a cycle's request with no wait of a guessed length. The fake's server
/// answers nothing else meanwhile, nor what requests it received.
pub(in crate::listing) struct HeldRequest {
    has_come: Arc<std::sync::atomic::AtomicBool>,
    is_let_go: Arc<std::sync::atomic::AtomicBool>,
    answer: ResponseTemplate,
}

/// The test's end of a [`HeldRequest`].
pub(in crate::listing) struct RequestHold {
    has_come: Arc<std::sync::atomic::AtomicBool>,
    is_let_go: Arc<std::sync::atomic::AtomicBool>,
}

impl wiremock::Respond for HeldRequest {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        self.has_come.store(true, std::sync::atomic::Ordering::SeqCst);
        while !self.is_let_go.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.answer.clone()
    }
}

impl RequestHold {
    /// Returns once the request is being held.
    pub(in crate::listing) async fn wait_for_request(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.has_come.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the request to hold came");
    }

    pub(in crate::listing) fn let_go(&self) {
        self.is_let_go.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for RequestHold {
    /// A test that ends with the request held lets it go: the fake's server would wait
    /// for it otherwise.
    fn drop(&mut self) {
        self.let_go();
    }
}

impl ListedFolder {
    /// The next request for a page is held until the test lets it go, and then answers
    /// that nothing changed, with the fake's delta link.
    pub(in crate::listing) async fn hold_next_page_request(&self) -> RequestHold {
        let hold = RequestHold { has_come: Arc::default(), is_let_go: Arc::default() };
        let held = HeldRequest { has_come: Arc::clone(&hold.has_come), is_let_go: Arc::clone(&hold.is_let_go), answer: self.page_answer(&[], None) };
        Mock::given(method("GET")).and(path("/me/drive/root/delta")).respond_with(held).up_to_n_times(1).mount(&self.graph.server).await;
        hold
    }
}

#[tokio::test]
async fn two_cycles_never_run_at_once() {
    let folder = ListedFolder::start_listed().await;
    let asked_for_the_drive = |requests: Vec<wiremock::Request>| requests.iter().filter(|request| request.url.path() == "/me/drive").count();
    let asked_before = asked_for_the_drive(folder.graph.server.received_requests().await.expect("the requests"));
    let hold = folder.hold_next_page_request().await;
    let first = tokio::spawn({
        let cycle = Arc::clone(&folder.cycle);
        async move { cycle.run(&CancellationToken::new()).await }
    });
    hold.wait_for_request().await;

    // The first cycle stands in its request with the turn taken, and the turn is the
    // first thing a cycle waits for: a second one asks nothing before it has it.
    assert!(folder.cycle.turn.try_lock().is_err(), "a cycle under way has the turn");
    let second = tokio::spawn({
        let cycle = Arc::clone(&folder.cycle);
        async move { cycle.run(&CancellationToken::new()).await }
    });
    tokio::task::yield_now().await;
    assert!(!second.is_finished());

    hold.let_go();
    first.await.expect("the first task").expect("the first cycle");
    second.await.expect("the second task").expect("the second cycle");
    assert_eq!(asked_for_the_drive(folder.graph.server.received_requests().await.expect("the requests")), asked_before + 2);
    assert!(folder.cycle.turn.try_lock().is_ok());
}

#[tokio::test]
async fn a_drive_that_is_not_the_folders_stops_the_cycle_before_anything_is_listed() {
    let fixture = ItemFixture::new();
    let graph = fixture.start_fake_onedrive().await;
    // The token reaches another drive than the store's now.
    Mock::given(method("GET")).and(path("/me/drive")).respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "another-drive" }))).with_priority(1).mount(&graph.server).await;
    let state = SyncStateHandle::new(SyncSnapshot::default());
    let cycle = make_cycle(&fixture.store, &graph, &fixture.sync_plan_wake, &state);

    let refused = cycle.run(&CancellationToken::new()).await;

    assert!(matches!(&refused, Err(CycleError::OtherAccount(drive)) if drive == DRIVE_ID), "{refused:?}");
    assert!(state.get().cycle.sync_trouble.is_some_and(|trouble| trouble.blocking));
    assert_eq!(graph.with(|cloud| cloud.count("GET", "delta")), 0);
}

#[tokio::test]
async fn a_full_listing_stopped_before_its_first_link_was_written_is_begun_anew() {
    let folder = ListedFolder::start_listed().await;
    let item = folder.item("a.txt").expect("a.txt");
    folder.fixture.write_rows(|store| store.item_store().request_full_listing(FullListingCause::UserAsked));
    // The first page names the root, a.txt and b.txt. While it is fetched a step's rename
    // of a.txt is confirmed: b.txt is written and marked as seen, a.txt is left to be
    // read again, and no link is written.
    let first_page = folder.page_answer(&["R", "a.txt", "b.txt"], Some(2));
    folder.graph.with(|cloud| cloud.rename("a.txt", "R", "ours.txt"));
    let place = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()).expect("its place");
    let confirmed = ConfirmedOneDriveVersion::Present { version: ItemVersion { place: Some(ItemPlace { parent_local_item_id: place.parent_local_item_id, name: "ours.txt".into() }), content: item.onedrive.and_then(|onedrive| onedrive.content) }, etag: "e-ours".into() };
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).respond_with(ConfirmedWhileFetched { store: folder.fixture.store.clone(), item: item.local_item_id, confirmed, stale_page: first_page }).up_to_n_times(1).mount(&folder.graph.server).await;
    // The reading again fails: the cycle ends there.
    folder.graph.with(|cloud| cloud.script("GET", "items/a.txt", ResponseTemplate::new(400), 1));

    let failed = folder.run().await;

    assert!(matches!(failed, Err(CycleError::Offline(_))), "{failed:?}");
    assert_eq!(folder.requests().await, ["root/delta", "items/a.txt"]);
    let stopped = folder.listing_under_way().expect("the listing is under way");
    assert_eq!((stopped.kind, stopped.next_link.as_ref()), (ListingKind::Full, None));
    assert_eq!(folder.listed_name("b.txt").as_deref(), Some("b.txt"));

    // b.txt is removed in OneDrive. What is asked from the start now is another
    // enumeration, which does not name it.
    folder.graph.with(|cloud| cloud.trash("b.txt"));
    folder.forget_requests().await;
    folder.script_page(&["R", "a.txt", "docs", "docs!c.txt"], None);

    let report = folder.run().await.expect("the next cycle");

    assert_eq!((report.kind, report.pages), (ListingKind::Full, 1));
    assert_eq!(folder.requests().await, ["root/delta"]);
    assert_eq!((folder.listing_under_way(), folder.delta_link()), (None, Some(folder.fake_delta_link())));
    // The rows do not say OneDrive has what the listing that ended did not name.
    assert_eq!(folder.listed_name("b.txt"), None);
    assert_eq!(folder.listed_name("a.txt").as_deref(), Some("ours.txt"));
    // The listing that ended is another than the one that was stopped.
    assert_eq!(folder.fixture.write_rows(|store| store.item_store().begin_delta_listing()).id.to_string().parse::<u64>().expect("a number"), stopped.id.to_string().parse::<u64>().expect("a number") + 2);
}

#[tokio::test]
async fn a_second_410_in_one_cycle_fails_the_cycle_and_the_next_begins_a_full_listing_anew() {
    let folder = ListedFolder::start_listed().await;
    let link = folder.delta_link().expect("a delta link");
    folder.graph.with(|cloud| cloud.script("GET", "root/delta", gone(410, "resyncRequired"), 2));

    let failed = folder.run().await;

    assert!(matches!(failed, Err(CycleError::Offline(_))), "{failed:?}");
    assert_eq!(folder.requests().await, [format!("root/delta?{}", link.rsplit_once('=').expect("a token").1), "root/delta".to_owned()]);
    let begun = folder.listing_under_way().expect("the full listing begun for the first 410");
    assert_eq!((begun.kind, begun.next_link, folder.delta_link()), (ListingKind::Full, None, None));

    folder.forget_requests().await;
    assert_eq!(folder.run().await.expect("the next cycle").kind, ListingKind::Full);
    assert_eq!(folder.requests().await, ["root/delta"]);
    assert_eq!(folder.listing_under_way(), None);
    assert!(folder.fixture.write_rows(|store| store.item_store().begin_delta_listing()).id > begun.id);
}

/// Answers every reading of `a.txt` with what OneDrive had before, after one more rename
/// of it by a step was confirmed in the store: each reading again is outdated in its turn.
struct ConfirmedAtEachRead {
    store: Store,
    item: Item,
    stale_item: Value,
    read_count: std::sync::atomic::AtomicUsize,
}

impl wiremock::Respond for ConfirmedAtEachRead {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        let read = self.read_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let onedrive = self.item.onedrive.clone().expect("what OneDrive has of it");
        let place = onedrive.place.expect("its place");
        let confirmed = ConfirmedOneDriveVersion::Present { version: ItemVersion { place: Some(ItemPlace { parent_local_item_id: place.parent_local_item_id, name: format!("ours-{read}.txt").into() }), content: onedrive.content }, etag: format!("e-ours-{read}") };
        let (store, item) = (self.store.clone(), self.item.local_item_id);
        std::thread::scope(|scope| scope.spawn(move || store.call_blocking(move |store| store.item_store().confirm_onedrive_version(item, &confirmed))).join()).expect("the thread").expect("the confirmation");
        ResponseTemplate::new(200).set_body_json(self.stale_item.clone())
    }
}

#[tokio::test]
async fn items_that_stay_outdated_are_read_again_so_many_rounds_and_then_the_cycle_fails() {
    let folder = ListedFolder::start_listed().await;
    let link = folder.delta_link();
    let listing = folder.fixture.write_rows(|store| store.item_store().begin_delta_listing());
    let changes = folder.page_changes(&["a.txt"]);
    let stale_item = folder.page_items(&["a.txt"]).remove(0);
    folder.confirm_rename("a.txt", "ours.txt");
    let responder = ConfirmedAtEachRead { store: folder.fixture.store.clone(), item: folder.item("a.txt").expect("a.txt"), stale_item, read_count: Default::default() };
    Mock::given(method("GET")).and(path("/me/drive/items/a.txt")).respond_with(responder).with_priority(1).mount(&folder.graph.server).await;
    let page = FetchedPage { changes, link: ListingLink::DeltaLink("the-link-of-the-page".into()), fetch_began_at_count: listing.began_at_count };

    let failed = folder.cycle.write_page(listing.id, page, &CancellationToken::new()).await;

    // Not a fault that stops the folder: the next cycle asks the page again.
    assert!(matches!(&failed, Err(error @ CycleError::Apply(_)) if !error.blocking()), "{failed:?}");
    assert_eq!(folder.requests().await, vec!["items/a.txt"; READ_AGAIN_ROUNDS]);
    assert_eq!((folder.listing_under_way().map(|under_way| under_way.id), folder.delta_link()), (Some(listing.id), link));
}

#[tokio::test]
async fn an_item_read_again_is_given_back_under_the_id_that_was_asked() {
    let folder = ListedFolder::start_listed().await;
    let listing = folder.fixture.write_rows(|store| store.item_store().begin_delta_listing());
    let changes = folder.page_changes(&["a.txt"]);
    folder.confirm_rename("a.txt", "ours.txt");
    folder.graph.with(|cloud| cloud.rename("a.txt", "R", "theirs.txt"));
    // OneDrive answers the reading with the id spelled another way than it was asked.
    let mut read = folder.page_items(&["a.txt"]).remove(0);
    read["id"] = json!("A.TXT");
    Mock::given(method("GET")).and(path("/me/drive/items/a.txt")).respond_with(ResponseTemplate::new(200).set_body_json(read)).with_priority(1).mount(&folder.graph.server).await;
    let row_count = folder.fixture.list_items().len();
    let page = FetchedPage { changes, link: ListingLink::DeltaLink("the-same-link".into()), fetch_began_at_count: listing.began_at_count };

    let written = folder.cycle.write_page(listing.id, page, &CancellationToken::new()).await.expect("the page");

    assert_eq!(written, PageWrite::LinkWritten { read_again: 1 });
    // What was read is of the row that was asked for, and no row is made for the spelling.
    assert_eq!(folder.item("A.TXT"), None);
    assert_eq!(folder.listed_name("a.txt").as_deref(), Some("theirs.txt"));
    assert_eq!(folder.fixture.list_items().len(), row_count);
}

/// A configuration of its own in a temporary directory, with the accounts `Mine` and
/// `Other`: the record of `Mine`'s drive, and `Other`'s id.
async fn open_config(directory: &std::path::Path) -> (DriveRecord, crate::config::AccountId) {
    let config = Arc::new(crate::config::ConfigStore::open(&crate::config::Paths::in_dir(directory), async { false }).await);
    let mine = config.add_account("Mine").expect("an account").id;
    let other = config.add_account("Other").expect("another account").id;
    (DriveRecord { store: config, account: mine, recorded: None }, other)
}

fn make_cycle_of_account(folder: &ListedFolder, drive_record: DriveRecord) -> Arc<ListingCycle> {
    ListingCycle::new(ListingCycleSetup { drive: folder.graph.client(), store: folder.fixture.store.clone(), sync_plan_wake: Arc::clone(&folder.fixture.sync_plan_wake), state: folder.state.clone(), drive_record: Some(drive_record), neighbours: None, report: Report::nowhere(), after_listing: None })
}

#[tokio::test]
async fn a_drive_another_account_has_recorded_is_not_listed_into_this_folder() {
    let folder = ListedFolder::start_listed().await;
    let directory = tempfile::tempdir().expect("a temporary directory");
    let (record, other) = open_config(directory.path()).await;
    record.store.record_drive(&other, &crate::config::DriveId::new(DRIVE_ID).expect("a drive id")).expect("the drive is recorded");
    let cycle = make_cycle_of_account(&folder, record.clone());

    let refused = cycle.run(&CancellationToken::new()).await;

    assert!(matches!(&refused, Err(CycleError::DriveTaken(label)) if label == "Other"), "{refused:?}");
    assert!(folder.state.get().cycle.sync_trouble.is_some_and(|trouble| trouble.blocking));
    assert_eq!(folder.graph.with(|cloud| cloud.count("GET", "delta")), 0);
    assert_eq!(record.store.account(&record.account).expect("the account").drive_id, None);
}

#[tokio::test]
async fn a_drive_is_the_same_however_its_id_is_spelled() {
    let folder = ListedFolder::start_listed().await;
    let directory = tempfile::tempdir().expect("a temporary directory");
    let (mut record, _) = open_config(directory.path()).await;
    // The store and `config.toml` have the drive in lower case; OneDrive answers it in
    // upper case.
    record.store.record_drive(&record.account, &crate::config::DriveId::new(DRIVE_ID).expect("a drive id")).expect("the drive is recorded");
    record.recorded = Some(DRIVE_ID.to_owned());
    Mock::given(method("GET")).and(path("/me/drive")).respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": DRIVE_ID.to_uppercase() }))).with_priority(1).mount(&folder.graph.server).await;
    assert_ne!(DRIVE_ID, DRIVE_ID.to_uppercase());
    let cycle = make_cycle_of_account(&folder, record);

    let report = cycle.run(&CancellationToken::new()).await.expect("the cycle");

    assert_eq!(report.kind, ListingKind::Delta);
}

#[tokio::test]
async fn the_first_cycle_records_the_accounts_drive_in_config_toml() {
    let folder = ListedFolder::start_listed().await;
    let directory = tempfile::tempdir().expect("a temporary directory");
    let (record, _) = open_config(directory.path()).await;
    let cycle = make_cycle_of_account(&folder, record.clone());

    cycle.run(&CancellationToken::new()).await.expect("the cycle");

    assert_eq!(record.store.account(&record.account).expect("the account").drive_id, crate::config::DriveId::new(DRIVE_ID));
    let written = std::fs::read_to_string(directory.path().join("config.toml")).expect("config.toml");
    assert!(written.contains(&format!("drive_id = \"{DRIVE_ID}\"")), "{written}");
    // The next cycle finds it recorded, and is the same account's.
    cycle.run(&CancellationToken::new()).await.expect("the next cycle");
}
