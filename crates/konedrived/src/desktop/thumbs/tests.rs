use std::sync::Arc;
use std::time::Duration;

use url::Url;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::mount::item_fixture::{onedrive_id_of, ItemFixture, ONEDRIVE_MTIME};
use konedrive_graph::token::StaticToken;

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image).write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
    out.into_inner()
}

/// A folder's rows with the files `names` shown, each a file OneDrive has with the MIME type
/// given, a fake Graph and an empty thumbnail cache.
struct World {
    server: MockServer,
    fixture: ItemFixture,
    cache: tempfile::TempDir,
    state: SyncStateHandle,
}

async fn world(files: &[(&str, &str)]) -> World {
    let fixture = ItemFixture::new();
    for (name, mime) in files {
        let shown = fixture.show_onedrive_file(name, name.as_bytes());
        let (item, mime) = (shown.local_item_id.as_i64(), (*mime).to_owned());
        // The listing's row carries no MIME type in the fixture: it is written as a listing would have.
        fixture.write_rows(move |store| store.bench_sql(&format!("UPDATE items SET onedrive_mime = '{mime}' WHERE local_item_id = {item}")));
    }
    World { server: MockServer::start().await, fixture, cache: tempfile::tempdir().unwrap(), state: SyncStateHandle::new(Default::default()) }
}

fn thumb_path(name: &str, size: &str) -> String {
    format!("/me/drive/items/{}/thumbnails/0/{size}/content", onedrive_id_of(name))
}

impl World {
    fn root(&self) -> std::path::PathBuf {
        self.fixture.folder().canonicalize().unwrap()
    }

    fn filler(&self) -> ThumbnailFiller {
        // Short throttle waits, so a `503` is given up on in milliseconds.
        let retry = konedrive_graph::drive::RetryPolicy { attempts: 2, default_wait: Duration::from_millis(10), max_wait: Duration::from_millis(50) };
        let drive = konedrive_graph::drive::DriveClient::new(Url::parse(&format!("{}/", self.server.uri())).unwrap(), Arc::new(StaticToken::new("T"))).unwrap().with_retry(retry);
        let root = SyncRoot { path: self.root(), root_id: "r".into() };
        ThumbnailFiller::new(drive, self.fixture.store.clone(), root, self.cache.path().to_path_buf(), Arc::default(), self.state.clone())
    }

    fn cached(&self, dir: &str, name: &str) -> std::path::PathBuf {
        let uri = file_uri(&self.root().join(name));
        self.cache.path().join(dir).join(format!("{:x}.png", md5::Md5::digest(uri.as_bytes())))
    }

    fn item(&self, name: &str) -> LocalItemId {
        self.fixture.find_item_at(name).expect("the row").local_item_id
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

/// An image the folder shows gets the thumbnails KIO looks for, named by the path the folder
/// shows it at and stamped with the time of the content OneDrive has; made once, it is not
/// asked for again.
#[tokio::test]
async fn an_image_gets_the_thumbnails_kio_looks_for_and_is_asked_for_once() {
    let w = world(&[("p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path(thumb_path("p.jpg", "c512x512")))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "image/jpeg").set_body_bytes(jpeg(600, 400)))
        .expect(1)
        .mount(&w.server).await;
    let filler = w.filler();
    let made = filler.run_once(&CancellationToken::new(), 100).await;
    assert_eq!((made.taken, made.written), (1, 1));
    for (dir, edge) in SIZES {
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(w.cached(dir, "p.jpg")).unwrap()));
        let reader = decoder.read_info().unwrap();
        let info = reader.info();
        assert!(info.width.max(info.height) <= *edge, "{dir}: {}x{}", info.width, info.height);
        let text = |key: &str| info.uncompressed_latin1_text.iter().find(|t| t.keyword == key).map(|t| t.text.clone());
        assert_eq!(text("Thumb::URI"), Some(file_uri(&w.root().join("p.jpg"))));
        assert_eq!(text("Thumb::MTime"), Some(ONEDRIVE_MTIME.to_string()));
    }
    let again = filler.run_once(&CancellationToken::new(), 100).await;
    assert_eq!((again.taken, again.written), (0, 0), "no candidate the second time");
}

/// What settles the question is recorded for the content, and not asked again: Graph has
/// no thumbnail (404), refuses (403), or answers bytes that are no image.
#[tokio::test]
async fn an_item_with_no_usable_thumbnail_is_not_asked_again() {
    let w = world(&[("none.jpg", "image/jpeg"), ("refused.png", "image/png"), ("broken.mp4", "video/mp4")]).await;
    for (name, answer) in [("none.jpg", ResponseTemplate::new(404)), ("refused.png", ResponseTemplate::new(403)), ("broken.mp4", ResponseTemplate::new(200).set_body_bytes(b"no image at all".to_vec()))] {
        Mock::given(method("GET")).and(path(thumb_path(name, "c512x512"))).respond_with(answer).expect(1).mount(&w.server).await;
    }
    let filler = w.filler();
    let first = filler.run_once(&CancellationToken::new(), 100).await;
    assert_eq!((first.taken, first.written), (3, 0));
    assert_eq!(filler.run_once(&CancellationToken::new(), 100).await.taken, 0);
}

/// The thumbnails being downloaded are counted as their requests start and end, and none is
/// once the run has ended.
#[tokio::test]
async fn the_thumbnails_being_downloaded_are_counted_and_none_after_the_run() {
    let w = world(&[("a.jpg", "image/jpeg"), ("b.jpg", "image/jpeg")]).await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/me/drive/items/[ab]\.jpg/thumbnails/0/c512x512/content$"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "image/jpeg").set_body_bytes(jpeg(64, 64)).set_delay(Duration::from_millis(300)))
        .mount(&w.server)
        .await;
    let mut counts = w.state.subscribe();
    let seen = tokio::spawn(async move {
        let mut seen = Vec::new();
        while counts.changed().await.is_ok() {
            let count = counts.borrow_and_update().transfers.thumbnail_downloads;
            seen.push(count);
            if count == 0 {
                break;
            }
        }
        seen
    });
    let made = w.filler().run_once(&CancellationToken::new(), 100).await;
    assert_eq!(made.written, 2);
    assert_eq!(w.state.get().transfers.thumbnail_downloads, 0, "none after the run");
    let seen = seen.await.unwrap();
    assert!(seen.contains(&2), "both requests under way at once: {seen:?}");
}

#[tokio::test]
async fn only_images_and_videos_are_asked_for() {
    let w = world(&[("notes.txt", "text/plain")]).await;
    assert_eq!(w.filler().run_once(&CancellationToken::new(), 100).await.taken, 0);
    assert!(w.server.received_requests().await.unwrap().is_empty());
}

/// Batches go on where the last one stopped, by local item id — each candidate is looked at
/// once per drain, the ones Graph fails for this time included — and the next drain starts
/// from the beginning again.
#[tokio::test]
async fn batches_go_on_where_the_last_stopped() {
    let names: Vec<String> = (0..5).map(|i| format!("p{i}.jpg")).collect();
    let files: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), "image/jpeg")).collect();
    let w = world(&files).await;
    // Every request fails in a way worth trying again (a server error): nothing is recorded.
    Mock::given(method("GET")).and(path_regex(r"^/me/drive/items/p\d\.jpg/thumbnails/0/c512x512/content$")).respond_with(ResponseTemplate::new(500)).mount(&w.server).await;
    let filler = w.filler();
    let (first, next) = filler.run_from(&CancellationToken::new(), 2, None).await;
    assert_eq!((first.taken, next), (2, Some(w.item("p1.jpg"))));
    let (second, next) = filler.run_from(&CancellationToken::new(), 2, next).await;
    assert_eq!((second.taken, next), (2, Some(w.item("p3.jpg"))), "not the first two again");
    let (third, next) = filler.run_from(&CancellationToken::new(), 2, next).await;
    assert_eq!((third.taken, next), (1, None), "a batch that is not full was the last");
    let total = filler.drain(&CancellationToken::new(), 2).await;
    assert_eq!(total.taken, 5, "a drain looks at each once, and ends");
    // A passing trouble settled nothing: the next drain asks for all of them again.
    assert_eq!(filler.drain(&CancellationToken::new(), 2).await.taken, 5);
}

/// A full batch whose every candidate was settled leaves nothing for the next one, and the
/// drain ends there: it does not go round.
#[tokio::test]
async fn a_drain_ends_when_the_batch_after_a_full_one_is_empty() {
    let w = world(&[("a.jpg", "image/jpeg"), ("b.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path_regex(r"^/me/drive/items/[ab]\.jpg/thumbnails/0/c512x512/content$")).respond_with(ResponseTemplate::new(404)).expect(2).mount(&w.server).await;
    let total = w.filler().drain(&CancellationToken::new(), 2).await;
    assert_eq!(total.taken, 2);
}

/// Graph refuses `c512x512` for some items with `406`; its named size `large` is asked once
/// instead.
#[tokio::test]
async fn a_406_is_asked_again_at_graphs_named_size() {
    let w = world(&[("p.jpg", "image/jpeg")]).await;
    Mock::given(method("GET")).and(path(thumb_path("p.jpg", "c512x512"))).respond_with(ResponseTemplate::new(406)).expect(1).mount(&w.server).await;
    Mock::given(method("GET")).and(path(thumb_path("p.jpg", "large"))).respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg(64, 64))).expect(1).mount(&w.server).await;
    assert_eq!(w.filler().run_once(&CancellationToken::new(), 100).await.written, 1);
    assert!(w.cached("normal", "p.jpg").is_file());
}

/// A stop does not wait for candidates that were not asked yet: with the stop given before
/// the batch, nothing is asked of Graph.
#[tokio::test]
async fn a_filler_that_is_told_to_stop_asks_for_nothing_more() {
    let w = world(&[("p.jpg", "image/jpeg")]).await;
    let stop = CancellationToken::new();
    stop.cancel();
    let made = w.filler().run_once(&stop, 100).await;
    assert_eq!(made.written, 0);
    assert!(w.server.received_requests().await.unwrap().is_empty());
}

/// The requests Graph was asked for thumbnails.
async fn asked_count(w: &World) -> usize {
    w.server.received_requests().await.unwrap().len()
}

/// A run that would wait for its turn is given a quarter of a second, then stopped.
async fn run_briefly(filler: &ThumbnailFiller) -> RunOutcome {
    let stop = CancellationToken::new();
    let stopper = {
        let stop = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            stop.cancel();
        })
    };
    let made = filler.run_once(&stop, 100).await;
    stopper.abort();
    made
}

/// No more than `REQUESTS_AT_ONCE` requests are under way together, however many
/// candidates there are and however many slots the pool has free; none takes a slot.
#[tokio::test]
async fn a_few_thumbnails_are_asked_for_at_once_and_none_takes_a_slot_of_the_pool() {
    let names: Vec<String> = (0..5).map(|i| format!("p{i}.jpg")).collect();
    let files: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), "image/jpeg")).collect();
    let w = world(&files).await;
    // Each answer takes long enough for the first two to be under way together, and for
    // the filler to ask once more whether the third may start.
    Mock::given(method("GET")).and(path_regex(r"^/me/drive/items/p\d\.jpg/thumbnails/0/c512x512/content$")).respond_with(ResponseTemplate::new(404).set_delay(Duration::from_millis(600))).mount(&w.server).await;
    let filler = Arc::new(w.filler());
    let pool = Arc::clone(filler.drive.pool());
    let run = {
        let filler = Arc::clone(&filler);
        tokio::spawn(async move { filler.run_once(&CancellationToken::new(), 100).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(asked_count(&w).await, request_start::REQUESTS_AT_ONCE, "the rest wait for these");
    assert_eq!(pool.held(Class::Download), 0, "a thumbnail takes no slot");
    assert_eq!(run.await.unwrap().taken, 5);
    assert_eq!(asked_count(&w).await, 5, "each was asked in its turn");
    assert_eq!(pool.size(), konedrive_graph::pool::START, "and none made the pool grow");
}

/// While a listing is under way nothing is asked, and the run ends at once: the
/// listing's end nudges the filler.
#[tokio::test]
async fn nothing_is_asked_while_a_listing_is_under_way() {
    let w = world(&[("p.jpg", "image/jpeg")]).await;
    w.fixture.write_rows(|store| store.item_store().begin_delta_listing().map(|_| ()));
    let filler = w.filler();
    let (made, next) = filler.run_from(&CancellationToken::new(), 1, None).await;
    assert_eq!((made.written, next), (0, None), "the drain ends here");
    assert_eq!(asked_count(&w).await, 0);
}

/// Before a full listing ended (the store has no delta link) nothing is asked.
#[tokio::test]
async fn nothing_is_asked_before_the_first_listing_ended() {
    let w = world(&[("p.jpg", "image/jpeg")]).await;
    w.fixture.write_rows(|store| store.item_store().begin_full_listing().map(|_| ()));
    assert_eq!(w.filler().run_once(&CancellationToken::new(), 100).await.written, 0);
    assert_eq!(asked_count(&w).await, 0);
}

/// Thumbnails give way: none is asked while a file is being opened, while a transfer
/// waits for a slot of the pool, or during a throttle's wait.
#[tokio::test]
async fn nothing_is_asked_while_a_file_is_opened_a_transfer_waits_or_onedrive_throttles() {
    let w = world(&[("p.jpg", "image/jpeg")]).await;
    let filler = w.filler();
    let pool = Arc::clone(filler.drive.pool());

    let opened = pool.acquire(Class::Open).await;
    run_briefly(&filler).await;
    assert_eq!(asked_count(&w).await, 0, "a file is being opened");
    drop(opened);

    let mut downloads = Vec::new();
    for _ in 0..pool.size() {
        downloads.push(pool.acquire(Class::Download).await);
    }
    let waiting = {
        let pool = Arc::clone(&pool);
        tokio::spawn(async move { pool.acquire(Class::Upload).await })
    };
    while !pool.waiting() {
        tokio::task::yield_now().await;
    }
    run_briefly(&filler).await;
    assert_eq!(asked_count(&w).await, 0, "an upload waits for a slot");
    waiting.abort();
    let _ = waiting.await;
    run_briefly(&filler).await;
    assert_eq!(asked_count(&w).await, 1, "every slot busy and nothing waiting: a thumbnail needs no slot");
    drop(downloads);

    let w = world(&[("p.jpg", "image/jpeg")]).await;
    let filler = w.filler();
    filler.drive.pool().throttled(Some(Duration::from_secs(300)));
    run_briefly(&filler).await;
    assert_eq!(asked_count(&w).await, 0, "OneDrive asked to wait");
}
