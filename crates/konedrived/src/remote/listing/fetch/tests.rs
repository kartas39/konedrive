use crate::helper::HelperLink;
use std::sync::Arc;

use konedrive_fs::placeholder;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, Request, ResponseTemplate};

use super::*;
use crate::remote::listing::tests::*;
use crate::remote::listing::*;
use konedrive_tree::TreeStore;
/// A first listing places each page as it comes. While page 2
/// is still being asked for, page 1 is in the folder, under the lock,
/// and in the counts, the store knows where to go on from, and the
/// lifecycle lock is free for a Forget or a helper's reconnect.
#[tokio::test]
async fn a_first_listing_shows_each_page_while_the_next_is_asked_for() {
    let s = setup().await;
    s.page(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), folder("E", "R", "extra")]), "P2").await;
    let mut asked = s.held(Some("P2")).await;
    let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
    let listing = Listing::new(ListingContext { lifecycle: Arc::clone(&lifecycle), ..s.context() });
    let cancel = CancellationToken::new();
    let running = spawn_cycle(&listing, &cancel);
    within(asked.recv()).await.unwrap();

    assert!(lifecycle.try_write().is_ok(), "the lifecycle lock is let go between pages");
    assert_eq!(tree_of(&s.root.path), ["docs", "docs/f.txt", "extra"]);
    assert_eq!(mode(&s.root.path.join("docs")), placeholder::LOCKED_DIR_MODE, "the folder is under the read-only lock between pages");
    assert_eq!(mode(&s.root.path), placeholder::LOCKED_DIR_MODE);
    let snapshot = s.state.get();
    assert!(snapshot.listing, "the listing is still said to run");
    assert_eq!((snapshot.items_listed, snapshot.items_placed), (3, 3));
    assert_eq!(s.store.call(move |t| t.listing_next()).await.unwrap(), Some(s.link("P2")));
    assert_eq!(s.store.call(move |t| t.delta_link()).await.unwrap(), None);
    assert!(s.activity().is_empty(), "the one `listed` event comes at the end: {:?}", s.activity());

    cancel.cancel();
    assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
}

/// Across pages: an item whose folder has not come yet waits
/// for it, and is placed with it. One whose folder never comes is listed
/// and not placed, as a Full reconcile leaves it.
#[tokio::test]
async fn an_item_whose_folder_comes_on_a_later_page_waits_for_it() {
    let s = setup().await;
    s.page(None, json!([root_item(), file("C", "P", "c.txt", "c1"), folder("Q", "R", "q"), file("O", "NOWHERE", "o.txt", "c1")]), "P2").await;
    let seen = Arc::new(std::sync::Mutex::new(None));
    let (root, look) = (s.root.path.clone(), Arc::clone(&seen));
    let answer = ResponseTemplate::new(200)
        .set_body_json(json!({"value": [folder("P", "R", "papers")], "@odata.deltaLink": s.link("L1")}));
    s.answer(Some("P2"), move |_: &Request| {
        *look.lock().unwrap() = Some(tree_of(&root));
        answer.clone()
    })
    .await;
    within(s.listing().cycle(&CancellationToken::new())).await.unwrap();

    assert_eq!(seen.lock().unwrap().take().expect("page 2 was asked for"), ["q"], "c.txt waited for its folder");
    assert_eq!(tree_of(&s.root.path), ["papers", "papers/c.txt", "q"]);
    let snapshot = s.state.get();
    assert_eq!((snapshot.items_listed, snapshot.items_placed), (4, 3), "o.txt is listed, and nowhere");
}

/// The riskiest case of across pages: an entry whose folder has
/// not come yet is held only in `items` once its page is committed. A
/// stop before its folder comes must not lose it: the listing resumed in
/// a new `Listing`, as after a restart, places it with its folder.
#[tokio::test]
async fn an_entry_waiting_for_its_folder_survives_a_stop() {
    let s = setup().await;
    s.page(None, json!([root_item(), file("C", "P", "c.txt", "c1")]), "P2").await;
    let mut asked = s.held(Some("P2")).await;
    let cancel = CancellationToken::new();
    let running = spawn_cycle(&s.listing(), &cancel);
    within(asked.recv()).await.unwrap();
    cancel.cancel();
    assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
    assert_eq!(tree_of(&s.root.path), Vec::<String>::new(), "c.txt waits for its folder");

    s.feed(Some("P2"), json!([folder("P", "R", "papers")]), "L1").await;
    within(s.listing().cycle(&CancellationToken::new())).await.unwrap();
    assert_eq!(tree_of(&s.root.path), ["papers", "papers/c.txt"]);
    assert_eq!(s.delta_tokens().await, [None, Some("P2".to_owned()), Some("P2".to_owned())]);
}

/// Ruling 1 of: a listing stopped between pages resumes where it
/// stopped — in a new `Listing`, as after a restart — and asks for no
/// page it placed again. It still ends in one `listed` event.
#[tokio::test]
async fn a_listing_stopped_part_way_resumes_where_it_stopped() {
    let s = setup().await;
    s.page(None, json!([root_item(), folder("D", "R", "docs")]), "P2").await;
    s.page(Some("P2"), json!([file("F", "D", "f.txt", "c1")]), "P3").await;
    let mut asked = s.held(Some("P3")).await;
    let cancel = CancellationToken::new();
    let running = spawn_cycle(&s.listing(), &cancel);
    within(asked.recv()).await.unwrap();
    cancel.cancel();
    assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
    assert!(!s.state.get().listing);

    s.feed(Some("P3"), json!([folder("E", "R", "extra")]), "L1").await;
    let report = within(s.listing().cycle(&CancellationToken::new())).await.unwrap();
    assert!(report.full);
    let from = |t: &str| Some(t.to_owned());
    assert_eq!(s.delta_tokens().await, [None, from("P2"), from("P3"), from("P3")], "pages 1 and 2 are not asked for again");
    assert_eq!(tree_of(&s.root.path), ["docs", "docs/f.txt", "extra"]);
    assert_eq!(s.store.call(move |t| Ok((t.delta_link()?, t.listing_next()?))).await.unwrap(), (Some(s.link("L1")), None));
    let folder = s.root.path.display().to_string();
    assert_eq!(s.activity(), vec![("listed".to_owned(), folder, "3 items".to_owned())]);
    let snapshot = s.state.get();
    assert_eq!((snapshot.listing, snapshot.items_listed, snapshot.items_placed), (false, 3, 3));
}

/// A stopped listing whose resume link Graph refuses — expired (`410`),
/// or a token it no longer takes (`400`) — lists the drive again from
/// the start and reconciles the folder once in full, as after any
/// expired feed: what is placed is found by its id, not made again, and
/// none of it is rescued.
#[tokio::test]
async fn a_refused_resume_link_lists_again_from_the_start_without_duplicates() {
    for refusal in [410, 400] {
        let s = setup().await;
        s.page(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1")]), "P2").await;
        let mut asked = s.held(Some("P2")).await;
        let cancel = CancellationToken::new();
        let running = spawn_cycle(&s.listing(), &cancel);
        within(asked.recv()).await.unwrap();
        cancel.cancel();
        assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
        let placed = ino(&s.root.path.join("docs/f.txt"));

        s.answer(Some("P2"), ResponseTemplate::new(refusal)).await;
        s.feed(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), folder("E", "R", "extra")]), "L1").await;
        let report = within(s.listing().cycle(&CancellationToken::new())).await.unwrap();

        assert!(report.full, "{refusal}");
        assert!(report.applied.rescued.is_empty(), "{refusal}: {:?}", report.applied.rescued);
        assert!(konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap().is_empty(), "{refusal}");
        assert_eq!(tree_of(&s.root.path), ["docs", "docs/f.txt", "extra"], "{refusal}");
        assert_eq!(ino(&s.root.path.join("docs/f.txt")), placed, "{refusal}: the placeholder was found, not made again");
        let from = |t: &str| Some(t.to_owned());
        assert_eq!(s.delta_tokens().await, [None, from("P2"), from("P2"), None], "{refusal}");
        assert_eq!(s.store.call(move |t| Ok((t.delta_link()?, t.listing_next()?))).await.unwrap(), (Some(s.link("L1")), None), "{refusal}");
        let folder = s.root.path.display().to_string();
        assert_eq!(s.activity(), vec![("listed".to_owned(), folder, "3 items".to_owned())], "{refusal}");
    }
}

/// Only the resume link a stopped listing left is one Graph may refuse
/// and send the listing back to the start. A next-page link handed out
/// earlier in the same cycle that Graph turns down fails the cycle as
/// any trouble with Graph does; the listing stays page by page, and the
/// next cycle resumes at that page.
#[tokio::test]
async fn a_next_page_turned_down_fails_the_cycle_and_the_listing_resumes_there() {
    let s = setup().await;
    s.page(None, json!([root_item(), folder("D", "R", "docs")]), "P2").await;
    s.answer(Some("P2"), ResponseTemplate::new(400)).await;
    let listing = s.listing();
    let err = within(listing.cycle(&CancellationToken::new())).await.unwrap_err();
    assert!(matches!(err, CycleError::Offline(_)), "{err:?}");
    assert_eq!(s.store.call(move |t| t.listing_next()).await.unwrap(), Some(s.link("P2")), "still page by page, at page 2");
    assert_eq!(tree_of(&s.root.path), ["docs"]);

    s.feed(Some("P2"), json!([folder("E", "R", "extra")]), "L1").await;
    within(listing.cycle(&CancellationToken::new())).await.unwrap();
    let from = |t: &str| Some(t.to_owned());
    assert_eq!(s.delta_tokens().await, [None, from("P2"), from("P2")], "page 1 is not asked for again");
    assert_eq!(tree_of(&s.root.path), ["docs", "extra"]);
    assert_eq!(s.store.call(move |t| Ok((t.delta_link()?, t.listing_next()?))).await.unwrap(), (Some(s.link("L1")), None));
}

/// Only the first listing is placed page by page (Ruling 2 of):
/// a later cycle's delta, however many pages it has, still goes into
/// `staging` and changes the folder only once all of it is in.
#[tokio::test]
async fn a_later_cycle_still_changes_the_folder_only_once_its_delta_is_all_in() {
    let s = setup().await;
    let listing = listed(&s).await;
    s.page(Some("L1"), json!([folder("N", "R", "new")]), "P2").await;
    let mut asked = s.held(Some("P2")).await;
    let cancel = CancellationToken::new();
    let running = spawn_cycle(&listing, &cancel);
    within(asked.recv()).await.unwrap();

    assert!(!s.root.path.join("new").exists(), "nothing of the delta is placed before all of it is in");
    assert_eq!(s.store.call(move |t| Ok((t.delta_link()?, t.listing_next()?))).await.unwrap(), (Some(s.link("L1")), None));
    cancel.cancel();
    assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
}

/// Invariant M1, page by page: every folder is marked through the helper
/// while it is still empty, the folder above it before it — page 1's
/// before page 2 is even asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_folder_placed_page_by_page_is_marked_before_anything_is_put_in_it() {
    let s = setup().await;
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let marks = recording_helper(&socket_path);
    let link = HelperLink::connect(&socket_path).await.unwrap().0;
    let listing = Listing::new(ListingContext { intercepted: true, link: Arc::new(std::sync::Mutex::new(Some(link))), ..s.context() });
    s.page(None, json!([root_item(), folder("A", "R", "a"), file("AF", "A", "a.txt", "c1"), folder("C", "B", "c"), file("CF", "C", "c.txt", "c1")]), "P2").await;
    let seen = Arc::new(std::sync::Mutex::new(None));
    let (look, marked) = (Arc::clone(&seen), Arc::clone(&marks));
    let answer = ResponseTemplate::new(200).set_body_json(json!({"value": [folder("B", "R", "b")], "@odata.deltaLink": s.link("L1")}));
    s.answer(Some("P2"), move |_: &Request| {
        *look.lock().unwrap() = Some(marked.lock().unwrap().clone());
        answer.clone()
    })
    .await;
    within(listing.cycle(&CancellationToken::new())).await.unwrap();

    let id = |s: &str| Some(s.to_owned());
    assert_eq!(seen.lock().unwrap().take().expect("page 2 was asked for"), [(id("A"), 0)], "page 1's folder was marked, empty, before page 2");
    assert_eq!(*marks.lock().unwrap(), [(id("A"), 0), (id("B"), 0), (id("C"), 0)], "each folder marked while empty, b before the c inside it");
    assert_eq!(tree_of(&s.root.path), ["a", "a/a.txt", "b", "b/c", "b/c/c.txt"]);
}

/// A page stopped while it was being placed is not committed: the
/// listing resumes at that page, and what it had placed already is found
/// by its id — nothing made twice, nothing rescued, the lock back on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_stopped_while_it_was_being_placed_is_placed_again_without_duplicates() {
    let s = setup().await;
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let (reached, release) = stalling_helper(&socket_path, "/.konedrive-new-G");
    let link = HelperLink::connect(&socket_path).await.unwrap().0;
    let context = || ListingContext { intercepted: true, link: Arc::new(std::sync::Mutex::new(Some(link.clone()))), ..s.context() };
    s.page(None, json!([root_item(), folder("D", "R", "docs")]), "P2").await;
    let page_two = json!({"value": [folder("G", "R", "g"), file("Y", "G", "y.txt", "c1")], "@odata.deltaLink": s.link("L1")});
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "P2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page_two))
        .with_priority(1)
        .mount(&s.server).await;
    let cancel = CancellationToken::new();
    let running = spawn_cycle(&Listing::new(context()), &cancel);
    tokio::task::spawn_blocking(move || reached.recv_timeout(PATIENCE).unwrap()).await.unwrap();
    cancel.cancel();
    release.send(()).unwrap();
    assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
    assert_eq!(s.store.call(move |t| t.listing_next()).await.unwrap(), Some(s.link("P2")), "page 2 was not committed");
    assert_eq!(tree_of(&s.root.path), ["docs", "g"], "g was placed before the stop was seen");
    let g = ino(&s.root.path.join("g"));

    let report = within(Listing::new(context()).cycle(&CancellationToken::new())).await.unwrap();
    let from = |t: &str| Some(t.to_owned());
    assert_eq!(s.delta_tokens().await, [None, from("P2"), from("P2")]);
    assert_eq!(tree_of(&s.root.path), ["docs", "g", "g/y.txt"]);
    assert_eq!(ino(&s.root.path.join("g")), g, "g was found by its id, not made again");
    assert_eq!(mode(&s.root.path.join("g")), placeholder::LOCKED_DIR_MODE);
    assert!(report.applied.rescued.is_empty(), "{:?}", report.applied.rescued);
}

/// A first page stopped while it was being placed has committed nothing,
/// yet the folder holds what it placed: the listing is still one placed
/// page by page, and starts again from the start as one — page 1's
/// items found by their ids, page 1 placed before page 2 is asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_page_stopped_while_it_was_being_placed_starts_again_page_by_page() {
    let s = setup().await;
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let (reached, release) = stalling_helper(&socket_path, "/.konedrive-new-G");
    let link = HelperLink::connect(&socket_path).await.unwrap().0;
    let context = || ListingContext { intercepted: true, link: Arc::new(std::sync::Mutex::new(Some(link.clone()))), ..s.context() };
    let page_one = json!([root_item(), folder("A", "R", "a"), file("AF", "A", "a.txt", "c1"), folder("G", "R", "g")]);
    s.page(None, page_one.clone(), "P2").await;
    let cancel = CancellationToken::new();
    let running = spawn_cycle(&Listing::new(context()), &cancel);
    tokio::task::spawn_blocking(move || reached.recv_timeout(PATIENCE).unwrap()).await.unwrap();
    cancel.cancel();
    release.send(()).unwrap();
    assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
    assert_eq!(tree_of(&s.root.path), ["a", "g"], "the stop was seen before a.txt");
    let g = ino(&s.root.path.join("g"));

    s.page(None, page_one, "P2").await;
    let mut asked = s.held(Some("P2")).await;
    let cancel = CancellationToken::new();
    let running = spawn_cycle(&Listing::new(context()), &cancel);
    within(asked.recv()).await.unwrap();
    assert_eq!(tree_of(&s.root.path), ["a", "a/a.txt", "g"], "page 1 was placed before page 2 was asked for");
    assert_eq!(ino(&s.root.path.join("g")), g, "g was found by its id, not made again");
    cancel.cancel();
    assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
    assert_eq!(s.delta_tokens().await, [None, None, Some("P2".to_owned())]);
}

/// A folder that already shows the drive — its tree store lost, or the
/// folder forgotten and registered again — is not placed page by page:
/// part-way through, an item not listed yet cannot be told from one that
/// is gone. It is reconciled once, when the whole listing is in, and
/// what it has is found by its id.
#[tokio::test]
async fn a_folder_that_already_shows_the_drive_is_reconciled_once_the_listing_is_in() {
    let s = setup().await;
    listed(&s).await;
    let placed = ino(&s.root.path.join("docs/f.txt"));
    let fresh = Store::new(TreeStore::in_memory().unwrap());
    let listing = Listing::new(ListingContext { store: fresh.clone(), ..s.context() });
    s.page(None, json!([root_item(), folder("E", "R", "extra")]), "P2").await;
    let seen = Arc::new(std::sync::Mutex::new(None));
    let (root, look) = (s.root.path.clone(), Arc::clone(&seen));
    let answer = ResponseTemplate::new(200).set_body_json(
        json!({"value": [folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), vault()], "@odata.deltaLink": s.link("L2")}),
    );
    s.answer(Some("P2"), move |_: &Request| {
        *look.lock().unwrap() = Some(tree_of(&root));
        answer.clone()
    })
    .await;
    within(listing.cycle(&CancellationToken::new())).await.unwrap();

    assert_eq!(seen.lock().unwrap().take().expect("page 2 was asked for"), ["docs", "docs/f.txt"], "nothing changed part-way");
    assert_eq!(tree_of(&s.root.path), ["docs", "docs/f.txt", "extra"]);
    assert_eq!(ino(&s.root.path.join("docs/f.txt")), placed);
    assert_eq!(fresh.call(move |t| t.delta_link()).await.unwrap(), Some(s.link("L2")));
}

/// A rescue made while a page is placed is a conflict at once (spec
/// §16.2), not at the end of the listing: a listing that never ends must
/// still say where the file went.
#[tokio::test]
async fn a_rescue_made_by_a_page_is_a_conflict_before_the_listing_ends() {
    let s = setup().await;
    std::fs::write(s.root.path.join("docs"), b"mine").unwrap();
    s.page(None, json!([root_item(), folder("D", "R", "docs")]), "P2").await;
    let mut asked = s.held(Some("P2")).await;
    let cancel = CancellationToken::new();
    let running = spawn_cycle(&s.listing(), &cancel);
    within(asked.recv()).await.unwrap();

    let conflicts = konedrive_tree::off_runtime(|| s.report.activity.conflicts()).unwrap();
    assert_eq!(conflicts.iter().map(|c| c.original.clone()).collect::<Vec<_>>(), [s.full("docs")]);
    assert_eq!(std::fs::read(&conflicts[0].rescued).unwrap(), b"mine");
    assert!(s.activity().contains(&("conflict".to_owned(), s.full("docs"), conflicts[0].rescued.clone())), "{:?}", s.activity());
    cancel.cancel();
    assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
}
