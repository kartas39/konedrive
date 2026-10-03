use std::sync::Arc;
use std::time::Duration;

use url::Url;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::token::StaticToken;
use crate::tree::{Change, Kind, Placement, Row, TreeStore};

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image).write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
    out.into_inner()
}

fn photo(id: &str, name: &str, mime: &str) -> Change {
    Change::Upsert(Row { id: id.into(), parent_id: Some("R".into()), name: name.into(), kind: Kind::File, size: 1000, mtime: 1_700_000_000, etag: None, ctag: Some("c1".into()), quickxor: None, mime: Some(mime.into()), placement: Placement::Placed })
}

struct World {
    server: MockServer,
    folder: tempfile::TempDir,
    cache: tempfile::TempDir,
    store: Store,
}

async fn world(items: &[Change]) -> World {
    let server = MockServer::start().await;
    let folder = tempfile::tempdir().unwrap();
    let store = Store::new(TreeStore::in_memory().unwrap());
    let root = Change::Root(Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed });
    let mut changes = vec![root];
    changes.extend_from_slice(items);
    store.call(move |s| { s.begin_staging(false)?; s.stage(&changes)?; s.commit_staging("L") }).await.unwrap();
    for item in items {
        if let Change::Upsert(row) = item {
            let path = folder.path().join(&row.name);
            std::fs::write(&path, b"").unwrap();
            let times = [libc::timespec { tv_sec: row.mtime, tv_nsec: 0 }; 2];
            let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            // SAFETY: a valid C string and two timespecs.
            assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) }, 0);
        }
    }
    World { server, folder, cache: tempfile::tempdir().unwrap(), store }
}

impl World {
    fn filler(&self) -> ThumbnailFiller {
        // Short throttle waits, so a `503` is given up on in milliseconds.
        let retry = crate::drive::RetryPolicy { attempts: 2, default_wait: Duration::from_millis(10), max_wait: Duration::from_millis(50) };
        let drive = crate::drive::DriveClient::new(Url::parse(&format!("{}/", self.server.uri())).unwrap(), Arc::new(StaticToken::new("T"))).unwrap().with_retry(retry);
        let root = SyncRoot { path: self.folder.path().canonicalize().unwrap(), root_id: "r".into() };
        ThumbnailFiller::new(drive, self.store.clone(), root, self.cache.path().to_path_buf(), Arc::default())
    }

    fn cached(&self, dir: &str, file: &std::path::Path) -> std::path::PathBuf {
        let uri = file_uri(&file.canonicalize().unwrap());
        self.cache.path().join(dir).join(format!("{:x}.png", md5::Md5::digest(uri.as_bytes())))
    }
}

#[test]
fn a_file_uri_escapes_what_a_path_segment_cannot_hold() {
    assert_eq!(file_uri(std::path::Path::new("/home/u/OneDrive/plain.jpg")), "file:///home/u/OneDrive/plain.jpg");
    assert_eq!(file_uri(std::path::Path::new("/h/with space.jpg")), "file:///h/with%20space.jpg");
    assert_eq!(file_uri(std::path::Path::new("/h/фото.jpg")), "file:///h/%D1%84%D0%BE%D1%82%D0%BE.jpg");
    assert_eq!(file_uri(std::path::Path::new("/h/sym (1)+&,;=@:!$'*.jpg")), "file:///h/sym%20(1)+&,;=@:!$'*.jpg");
    assert_eq!(file_uri(std::path::Path::new("/h/100%.jpg")), "file:///h/100%25.jpg");
}

#[tokio::test]
async fn an_image_gets_the_thumbnails_kio_looks_for() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "image/jpeg").set_body_bytes(jpeg(600, 400)))
        .expect(1)
        .mount(&w.server).await;
    let filler = w.filler();
    assert_eq!(filler.run_once(&CancellationToken::new(), 100).await.written, 1);
    let file = w.folder.path().join("p.jpg");
    for (dir, edge) in SIZES {
        let png = w.cached(dir, &file);
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&png).unwrap()));
        let reader = decoder.read_info().unwrap();
        let info = reader.info();
        assert!(info.width.max(info.height) <= *edge, "{dir}: {}x{}", info.width, info.height);
        let text = |key: &str| info.uncompressed_latin1_text.iter().find(|t| t.keyword == key).map(|t| t.text.clone());
        assert_eq!(text("Thumb::URI"), Some(file_uri(&file.canonicalize().unwrap())));
        assert_eq!(text("Thumb::MTime"), Some("1700000000".into()));
    }
    assert_eq!(filler.run_once(&CancellationToken::new(), 100).await.written, 0, "nothing to do the second time");
}

#[tokio::test]
async fn an_item_graph_has_no_thumbnail_for_is_not_asked_again() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&w.server).await;
    let filler = w.filler();
    filler.run_once(&CancellationToken::new(), 100).await;
    filler.run_once(&CancellationToken::new(), 100).await;
}

#[tokio::test]
async fn only_images_and_videos_are_asked_for() {
    let w = world(&[photo("T", "notes.txt", "text/plain")]).await;
    assert_eq!(w.filler().run_once(&CancellationToken::new(), 100).await.written, 0);
    assert!(w.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_renamed_image_gets_a_thumbnail_under_its_new_name() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg(64, 64)))
        .expect(2)
        .mount(&w.server).await;
    let filler = w.filler();
    filler.run_once(&CancellationToken::new(), 100).await;
    std::fs::rename(w.folder.path().join("p.jpg"), w.folder.path().join("q.jpg")).unwrap();
    w.store.call(|s| { s.begin_staging(true)?; s.stage(&[photo("P", "q.jpg", "image/jpeg")])?; s.commit_staging("L2") }).await.unwrap();
    assert_eq!(filler.run_once(&CancellationToken::new(), 100).await.written, 1);
    assert!(w.cached("normal", &w.folder.path().join("q.jpg")).is_file());
}

/// promise: a thumbnail made for a version survives the next
/// commit, or every cycle would fetch every thumbnail again.
#[tokio::test]
async fn a_commit_keeps_what_the_thumbnails_were_made_for() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg(64, 64)))
        .expect(1)
        .mount(&w.server).await;
    let filler = w.filler();
    filler.run_once(&CancellationToken::new(), 100).await;
    // A full listing builds `staging` from nothing: only the commit's own
    // update can carry the thumbnail's key across.
    let root = Change::Root(Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed });
    w.store.call(move |s| { s.begin_staging(false)?; s.stage(&[root, photo("P", "p.jpg", "image/jpeg")])?; s.commit_staging("L2") }).await.unwrap();
    assert_eq!(filler.run_once(&CancellationToken::new(), 100).await.written, 0);
}

/// Bytes that Graph answered with 200 but that will not
/// decode (corrupt, wrong format) must be recorded like a 404, or every
/// cycle asks Graph for the same undecodable bytes forever. The mock's
/// `.expect(1)` is the real assertion: a second \`run_once\` that asked
/// Graph again would fail it.
#[tokio::test]
async fn an_undecodable_body_is_recorded_and_not_retried() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"not an image".to_vec()))
        .expect(1)
        .mount(&w.server).await;
    let filler = w.filler();
    assert_eq!(filler.run_once(&CancellationToken::new(), 100).await.written, 0, "nothing to write from bytes that will not decode");
    assert_eq!(filler.run_once(&CancellationToken::new(), 100).await.written, 0, "and not asked for again");
}

/// A full batch (`limit` candidates) that includes one
/// Graph refuses still leaves `taken == limit`, so a drain keyed off
/// `taken` (not `written`) does not stop before the next batch — here,
/// one more candidate past the first 200.
#[tokio::test]
async fn a_full_batch_with_a_refusal_in_it_still_drains_the_next_batch() {
    let mut items = vec![photo("BAD", "bad.jpg", "image/jpeg")];
    for i in 0..200 {
        items.push(photo(&format!("OK{i}"), &format!("ok{i}.jpg"), "image/jpeg"));
    }
    let w = world(&items).await;
    Mock::given(method("GET")).and(path("/me/drive/items/BAD/thumbnails/0/c512x512/content"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&w.server).await;
    Mock::given(method("GET")).and(path_regex(r"^/me/drive/items/OK\d+/thumbnails/0/c512x512/content$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg(8, 8)))
        .mount(&w.server).await;
    let filler = w.filler();
    let outcome = filler.drain(&CancellationToken::new(), 200).await;
    assert_eq!(outcome.taken, 201, "every candidate was looked at, not just the first full batch");
    assert_eq!(outcome.written, 200, "everything but the 404 was written");
}

/// Issue #39: batches go on where the last one stopped — each candidate
/// is looked at once per drain, the ones Graph fails for this time
/// included — and the next drain starts from the beginning again.
#[tokio::test]
async fn batches_go_on_where_the_last_stopped() {
    let items: Vec<Change> = (0..5).map(|i| photo(&format!("P{i}"), &format!("p{i}.jpg"), "image/jpeg")).collect();
    let w = world(&items).await;
    // Every request fails in a way worth trying again (a server error).
    Mock::given(method("GET")).and(path_regex(r"^/me/drive/items/P\d/thumbnails/0/c512x512/content$"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&w.server).await;
    let filler = w.filler();
    let (first, next) = filler.run_from(&CancellationToken::new(), 2, String::new()).await;
    assert_eq!((first.taken, next.as_deref()), (2, Some("P1")));
    let (second, next) = filler.run_from(&CancellationToken::new(), 2, next.unwrap()).await;
    assert_eq!((second.taken, next.as_deref()), (2, Some("P3")), "not P0 and P1 again");
    let total = filler.drain(&CancellationToken::new(), 2).await;
    assert_eq!(total.taken, 5, "a drain looks at each once, and ends");
}

/// Issue #39: a thumbnail that cannot be cached here (the cache is not a
/// directory) is recorded like the other failures: a batch of them ends
/// the drain, and the item is not asked for again until it changes.
#[tokio::test]
async fn a_batch_of_local_failures_ends_the_drain() {
    let items: Vec<Change> = (0..3).map(|i| photo(&format!("P{i}"), &format!("p{i}.jpg"), "image/jpeg")).collect();
    let w = world(&items).await;
    Mock::given(method("GET")).and(path_regex(r"^/me/drive/items/P\d/thumbnails/0/c512x512/content$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg(8, 8)))
        .expect(3)
        .mount(&w.server).await;
    let cache = w.cache.path().join("not-a-directory");
    std::fs::write(&cache, b"").unwrap();
    let drive = crate::drive::DriveClient::new(Url::parse(&format!("{}/", w.server.uri())).unwrap(), Arc::new(StaticToken::new("T"))).unwrap();
    let root = SyncRoot { path: w.folder.path().canonicalize().unwrap(), root_id: "r".into() };
    let filler = ThumbnailFiller::new(drive, w.store.clone(), root, cache, Arc::default());
    let total = filler.drain(&CancellationToken::new(), 2).await;
    assert_eq!((total.taken, total.written), (3, 0));
    assert_eq!(filler.drain(&CancellationToken::new(), 2).await.taken, 0, "recorded: not asked for again");
}

/// A deterministic replacement for the old timing-based
/// test: `write_thumbnail` must run off the async task, or nothing
/// else on this single-threaded runtime — not even a task spawned
/// moments before and waiting to say so — can run while it does. A
/// passing run costs a rendezvous, not a stopwatch: it finishes in
/// milliseconds. Only a failing run (write blocking the one runtime
/// thread) waits out the hook's 2 s timeout.
/// a stop no longer waits out a thumbnail
/// request — which can sit through Graph's `Retry-After` for minutes —
/// or the pause after one. A Forget waited for it.
#[tokio::test]
async fn a_stop_does_not_wait_for_a_thumbnail_request() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg(8, 8)).set_delay(Duration::from_secs(60)))
        .mount(&w.server).await;
    let (kick, cancel) = (Arc::new(Notify::new()), CancellationToken::new());
    let task = w.filler().spawn(Arc::clone(&kick), cancel.clone());
    kick.notify_one();
    for _ in 0..200 {
        if !w.server.received_requests().await.unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("the stop waited for the request").unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn write_runs_off_the_async_task_not_inline() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path("/me/drive/items/P/thumbnails/0/c512x512/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg(8, 8)))
        .mount(&w.server).await;
    let file = w.folder.path().canonicalize().unwrap().join("p.jpg");
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    register_write_hook(file, go_rx, ready_tx);
    // Waits for write_thumbnail's own "I've reached the blocking part"
    // signal before answering "go" — a rendezvous, not a race against
    // whatever else `run_once` awaits on the way there (the store, the
    // mock HTTP call): those would otherwise let this task run, and
    // send "go", before write_thumbnail is even called, proving nothing.
    let ponger = tokio::spawn(async move {
        let _ = ready_rx.await;
        let _ = go_tx.send(());
    });
    let filler = w.filler();
    let outcome = filler.run_once(&CancellationToken::new(), 100).await;
    ponger.await.unwrap();
    assert_eq!(outcome.written, 1);
}

fn thumb_path(id: &str, size: &str) -> String {
    format!("/me/drive/items/{id}/thumbnails/0/{size}/content")
}

/// Issue #80: Graph refuses `c512x512` for some items with `406`; its
/// named size `large` is asked once instead, and scaled down the same way.
#[tokio::test]
async fn a_406_is_asked_again_at_graphs_named_size() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path(thumb_path("P", "c512x512")))
        .respond_with(ResponseTemplate::new(406))
        .expect(1)
        .mount(&w.server).await;
    Mock::given(method("GET")).and(path(thumb_path("P", "large")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg(800, 600)))
        .expect(1)
        .mount(&w.server).await;
    let filler = w.filler();
    assert_eq!(filler.run_once(&CancellationToken::new(), 100).await.written, 1);
    let png = w.cached("x-large", &w.folder.path().join("p.jpg"));
    let info = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(png).unwrap())).read_info().unwrap().info().clone();
    assert_eq!((info.width, info.height), (512, 384), "scaled down to the largest size filled");
    assert_eq!(filler.run_once(&CancellationToken::new(), 100).await.taken, 0);
}

/// Issue #80: refused at both sizes, the item is recorded, and the next
/// drain does not ask for it again.
#[tokio::test]
async fn a_406_refused_at_both_sizes_is_recorded() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path(thumb_path("P", "c512x512")))
        .respond_with(ResponseTemplate::new(406))
        .expect(1)
        .mount(&w.server).await;
    Mock::given(method("GET")).and(path(thumb_path("P", "large")))
        .respond_with(ResponseTemplate::new(406))
        .expect(1)
        .mount(&w.server).await;
    let filler = w.filler();
    assert_eq!(filler.drain(&CancellationToken::new(), 100).await, RunOutcome { taken: 1, written: 0 });
    assert_eq!(filler.drain(&CancellationToken::new(), 100).await.taken, 0, "not asked for again");
}

/// Issue #80: any other 4xx is final at once — no second size, no retry.
#[tokio::test]
async fn a_403_is_recorded_at_once() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path(thumb_path("P", "c512x512")))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&w.server).await;
    Mock::given(method("GET")).and(path(thumb_path("P", "large")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg(8, 8)))
        .expect(0)
        .mount(&w.server).await;
    let filler = w.filler();
    filler.drain(&CancellationToken::new(), 100).await;
    assert_eq!(filler.drain(&CancellationToken::new(), 100).await.taken, 0);
}

/// Issue #80: a passing trouble — a sign-in trouble, a server error, a
/// dropped connection — is not recorded: the next drain asks again.
#[tokio::test]
async fn a_passing_trouble_is_asked_again_at_the_next_drain() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dropped = format!("http://{}/blob", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });
    let w = world(&[photo("A", "a.jpg", "image/jpeg"), photo("B", "b.jpg", "image/jpeg"), photo("C", "c.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path(thumb_path("A", "c512x512")))
        .respond_with(ResponseTemplate::new(401))
        .mount(&w.server).await;
    Mock::given(method("GET")).and(path(thumb_path("B", "c512x512")))
        .respond_with(ResponseTemplate::new(503))
        .mount(&w.server).await;
    Mock::given(method("GET")).and(path(thumb_path("C", "c512x512")))
        .respond_with(ResponseTemplate::new(302).insert_header("location", dropped.as_str()))
        .mount(&w.server).await;
    let filler = w.filler();
    assert_eq!(filler.drain(&CancellationToken::new(), 100).await, RunOutcome { taken: 3, written: 0 });
    assert_eq!(filler.drain(&CancellationToken::new(), 100).await.taken, 3, "none of them recorded");
    let asked_large = w.server.received_requests().await.unwrap().iter().any(|r| r.url.path().ends_with("/large/content"));
    assert!(!asked_large, "only a 406 is asked at the other size");
}

/// Issue #80: a recorded refusal holds only for that version of the
/// item; once it changes, it is asked for again.
#[tokio::test]
async fn a_refused_item_is_asked_again_once_it_changes() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path(thumb_path("P", "c512x512")))
        .respond_with(ResponseTemplate::new(403))
        .expect(2)
        .mount(&w.server).await;
    let filler = w.filler();
    filler.drain(&CancellationToken::new(), 100).await;
    assert_eq!(filler.drain(&CancellationToken::new(), 100).await.taken, 0);
    let Change::Upsert(mut row) = photo("P", "p.jpg", "image/jpeg") else { unreachable!() };
    row.ctag = Some("c2".into());
    w.store.call(move |s| { s.begin_staging(true)?; s.stage(&[Change::Upsert(row)])?; s.commit_staging("L2") }).await.unwrap();
    assert_eq!(filler.drain(&CancellationToken::new(), 100).await.taken, 1, "a new version is asked for");
}

/// Issue #80: with thumbnails off the filler asks Graph for nothing, kick or not, and
/// nothing else stops; turned on again, it asks for the items without one at once.
#[tokio::test]
async fn thumbnails_off_ask_for_nothing_and_on_again_ask_for_what_is_missing() {
    let w = world(&[photo("P", "p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path(thumb_path("P", "c512x512")))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "image/jpeg").set_body_bytes(jpeg(64, 64)))
        .expect(1)
        .mount(&w.server).await;
    let running = Arc::new(Running::default());
    running.change(|s| s.thumbnails = false);
    let filler = ThumbnailFiller { running: Arc::clone(&running), ..w.filler() };
    assert_eq!(filler.run_once(&CancellationToken::new(), 100).await.written, 0);
    let (kick, cancel) = (Arc::new(Notify::new()), CancellationToken::new());
    let task = filler.spawn(Arc::clone(&kick), cancel.clone());
    kick.notify_one();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(w.server.received_requests().await.unwrap().is_empty(), "off: no request");
    assert!(!running.stopped(&w.store), "the rest of the account runs");

    running.change(|s| s.thumbnails = true);
    let cached = w.cached("normal", &w.folder.path().join("p.jpg"));
    for _ in 0..200 {
        if cached.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cached.exists(), "on again, the missing one is asked for without a kick");
    cancel.cancel();
    task.await.unwrap();
}
