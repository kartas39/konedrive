//! Thumbnails from OneDrive: Graph's own thumbnail of
//! every image and video, written into the freedesktop thumbnail cache under
//! the name KIO looks for (`docs/kio-behavior.md`), so that Dolphin draws it
//! without opening — and so downloading — the file. In the background, one
//! request at a time with a pause between them.
//!
//! Only `normal`, `large` and `x-large` (up to 512 px) are filled: one Graph
//! request per image (`c512x512`), scaled down locally to the smaller sizes.
//! `xx-large` (1024 px) is not filled — see limitations log entry K15.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use md5::Digest;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::root::SyncRoot;
use crate::drive::DriveClient;
use crate::tree::{Row, Store};

/// The cache directories KIO consults, and the longest edge of each
/// (`docs/kio-behavior.md` §A). `xx-large` is deliberately absent — see
/// limitations log entry K15.
pub const SIZES: &[(&str, u32)] = &[("normal", 128), ("large", 256), ("x-large", 512)];
/// The one size asked of Graph; the smaller ones are scaled from it.
const GRAPH_SIZE: &str = "c512x512";
/// The decode limits a thumbnail's bytes are read under:
/// `DriveClient::thumbnail` already caps the body at 8 MiB, but a small body
/// can still decompress into a huge image (a decompression bomb), so the
/// decoder itself is capped too — comfortably above any real `c512x512`
/// thumbnail, far below what would hurt.
const MAX_DECODE_EDGE: u32 = 4096;
const MAX_DECODE_ALLOC: u64 = 64 * 1024 * 1024;

/// `file://` and the path, as KIO spells the URI it hashes (§A of the
/// measurements): every byte percent-encoded except the unreserved
/// characters, `/`, and the sub-delimiters a path segment may hold.
pub fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// What a thumbnail is made for: the version, the path (its cache name) and
/// the mtime (which KIO checks). Any of them changing needs a new one.
pub fn thumb_key(row: &Row, rel: &Path) -> String {
    format!("{}|{}|{}", row.ctag.as_deref().unwrap_or(""), rel.display(), row.mtime)
}

pub struct ThumbnailFiller {
    drive: DriveClient,
    store: Store,
    root: SyncRoot,
    cache: PathBuf,
    pause: Duration,
}

/// What one `run_once` did: `taken` is how many candidates it
/// looked at, `written` how many thumbnails it actually wrote. A batch of
/// exactly `limit` candidates may have more waiting behind it even when few
/// of them were writable (a 404, a body over the cap...), so a draining loop
/// must key off `taken`, not `written`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunOutcome {
    pub taken: usize,
    pub written: usize,
}

/// What making a thumbnail from the bytes Graph gave back can fail at, once
/// there are bytes to work with at all.
enum FillError {
    /// The bytes are not a usable image: corrupt, an unsupported format, or
    /// over the decode limits ([`MAX_DECODE_EDGE`]/[`MAX_DECODE_ALLOC`]).
    /// Recorded like a 404 — asking Graph again would only get the same
    /// bytes back.
    Undecodable(String),
    /// A local problem: disk, permissions, a vanished cache directory. Not
    /// recorded, so the next cycle tries again.
    Io(String),
}

impl ThumbnailFiller {
    pub fn new(drive: DriveClient, store: Store, root: SyncRoot, cache: PathBuf) -> Self {
        Self { drive, store, root, cache, pause: Duration::from_millis(500) }
    }

    /// The pause between two requests (tests: none).
    pub fn with_pause(mut self, pause: Duration) -> Self {
        self.pause = pause;
        self
    }

    /// Makes up to `limit` missing thumbnails.
    pub async fn run_once(&self, cancel: &CancellationToken, limit: usize) -> RunOutcome {
        let candidates = match self.store.run(move |s| s.thumbnail_candidates(limit, thumb_key)).await {
            Ok(candidates) => candidates,
            Err(e) => {
                tracing::warn!("cannot list the thumbnails to make: {e}");
                return RunOutcome::default();
            }
        };
        let taken = candidates.len();
        let mut written = 0;
        for (row, rel) in candidates {
            if cancel.is_cancelled() {
                break;
            }
            let key = thumb_key(&row, &rel);
            // Whether to record `key` for this item: true for anything that
            // settles the question of whether it has a usable thumbnail
            // (a real write, a 404, an oversized/refused body, bytes that
            // will not decode); false for a condition worth trying again
            // (a network hiccup, a local I/O problem).
            //
            // The request gives way to a stop: it
            // can wait out Graph's `Retry-After` — up to 300 s, four times —
            // and a Forget, or a switch to interception under the lifecycle
            // lock, waits for this task.
            let fetched = tokio::select! {
                () = cancel.cancelled() => break,
                fetched = self.drive.thumbnail(&row.id, GRAPH_SIZE) => fetched,
            };
            let settle = match fetched {
                Ok(Some(bytes)) => {
                    let (cache, file, mtime) = (self.cache.clone(), self.root.path.join(&rel), row.mtime);
                    match tokio::task::spawn_blocking(move || write_thumbnail(&cache, &file, mtime, &bytes)).await {
                        Ok(Ok(())) => {
                            written += 1;
                            true
                        }
                        Ok(Err(FillError::Undecodable(reason))) => {
                            tracing::warn!("no usable thumbnail for {}: {reason}", rel.display());
                            true
                        }
                        Ok(Err(FillError::Io(reason))) => {
                            tracing::warn!("cannot cache the thumbnail of {}: {reason}", rel.display());
                            false
                        }
                        Err(e) => {
                            tracing::warn!("the thumbnail task for {} failed: {e}", rel.display());
                            false
                        }
                    }
                }
                Ok(None) => true,
                Err(e) => {
                    tracing::info!("no thumbnail for {} this time: {e}", rel.display());
                    false
                }
            };
            if settle {
                let id = row.id.clone();
                if let Err(e) = self.store.run(move |s| s.set_thumb_key(&id, &key)).await {
                    tracing::warn!("cannot record the thumbnail of {}: {e}", rel.display());
                }
            }
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(self.pause) => {}
            }
        }
        RunOutcome { taken, written }
    }

    /// Calls `run_once` until a batch comes back with fewer than `limit`
    /// candidates — the sign nothing more is waiting right now — or
    /// cancellation.
    async fn drain(&self, cancel: &CancellationToken, limit: usize) -> RunOutcome {
        let mut total = RunOutcome::default();
        loop {
            let outcome = self.run_once(cancel, limit).await;
            total.taken += outcome.taken;
            total.written += outcome.written;
            if outcome.taken < limit || cancel.is_cancelled() {
                return total;
            }
        }
    }

    /// Runs in the background: after every cycle (`kick`), and every ten
    /// minutes in case a kick was missed, draining 200 thumbnails at a time
    /// per `run_once` until a batch is not full.
    pub fn spawn(self, kick: Arc<Notify>, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = kick.notified() => {}
                    () = tokio::time::sleep(Duration::from_secs(600)) => {}
                    () = cancel.cancelled() => return,
                }
                // Paused (`docs/design/writes.md` §11): no thumbnails either; the next kick
                // after the pause ends drains what waits.
                let store = self.store.clone();
                let paused = tokio::task::spawn_blocking(move || crate::sync::upload::paused(&store).is_some()).await.unwrap_or(false);
                if paused {
                    continue;
                }
                self.drain(&cancel, 200).await;
            }
        })
    }
}

/// A test-only rendezvous (deterministic test): lets a test
/// prove `write_thumbnail` runs off the async task without timing anything
/// on the passing path. Keyed by the exact `file` a call is made for, so
/// unrelated tests (different temp directories, so always a different path)
/// never see each other's hook.
#[cfg(test)]
type WriteHook = (PathBuf, std::sync::mpsc::Receiver<()>, tokio::sync::oneshot::Sender<()>);
#[cfg(test)]
static WRITE_HOOKS: std::sync::Mutex<Vec<WriteHook>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn register_write_hook(file: PathBuf, go: std::sync::mpsc::Receiver<()>, ready: tokio::sync::oneshot::Sender<()>) {
    WRITE_HOOKS.lock().unwrap().push((file, go, ready));
}

/// Called at the very top of the blocking work: if a test registered a hook
/// for this exact `file`, says "ready" (a task on the same runtime is
/// `.await`ing that, and only then sends "go") and waits up to 2 s for it.
/// If `write_thumbnail` runs inline on a single-threaded runtime, that task
/// can never be polled while this call blocks, so the wait times out and
/// this panics — the test fails fast at 2 s rather than hanging. On the
/// passing path (a separate `spawn_blocking` thread), the runtime's one
/// async thread is free to run that task immediately, so this returns in
/// well under a millisecond.
#[cfg(test)]
fn wait_for_test_hook(file: &Path) {
    let hook = {
        let mut hooks = WRITE_HOOKS.lock().unwrap();
        hooks.iter().position(|(f, _, _)| f == file).map(|i| hooks.remove(i))
    };
    if let Some((_, go, ready)) = hook {
        let _ = ready.send(());
        go.recv_timeout(Duration::from_secs(2)).expect(
            "nothing else on this runtime got to run while write_thumbnail was blocking it: it must not run inline on the async task",
        );
    }
}

/// Decodes `jpeg` (any format `image` recognises, under the decode limits),
/// scales it to each of `SIZES` and writes it into the freedesktop cache
/// atomically (a temp file, then a rename) — all synchronous, so callers run
/// it with `spawn_blocking` rather than on the async task.
fn write_thumbnail(cache: &Path, file: &Path, mtime: i64, jpeg: &[u8]) -> Result<(), FillError> {
    #[cfg(test)]
    wait_for_test_hook(file);

    let uri = file_uri(file);
    let name = format!("{:x}.png", md5::Md5::digest(uri.as_bytes()));

    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_EDGE);
    limits.max_image_height = Some(MAX_DECODE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(jpeg))
        .with_guessed_format()
        .map_err(|e| FillError::Undecodable(e.to_string()))?;
    reader.limits(limits);
    let image = reader.decode().map_err(|e| FillError::Undecodable(e.to_string()))?;

    write_sizes(cache, &name, &uri, mtime, &image).map_err(|e| FillError::Io(e.to_string()))
}

/// The filesystem half of [`write_thumbnail`]: scale, PNG-encode and cache
/// `image` at every size in [`SIZES`].
fn write_sizes(cache: &Path, name: &str, uri: &str, mtime: i64, image: &image::DynamicImage) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    for (dir, edge) in SIZES {
        let scaled = if image.width().max(image.height()) > *edge { image.thumbnail(*edge, *edge) } else { image.clone() };
        let rgba = scaled.to_rgba8();
        let dir = cache.join(dir);
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let tmp = dir.join(format!(".{name}.konedrive"));
        {
            let file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), rgba.width(), rgba.height());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.add_text_chunk("Thumb::URI".into(), uri.to_owned())?;
            encoder.add_text_chunk("Thumb::MTime".into(), mtime.to_string())?;
            encoder.add_text_chunk("Software".into(), "konedrive".into())?;
            let mut writer = encoder.write_header()?;
            writer.write_image_data(rgba.as_raw())?;
            writer.finish()?;
        }
        std::fs::rename(&tmp, dir.join(name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
        store.run(move |s| { s.begin_staging(false)?; s.stage(&changes)?; s.commit_staging("L") }).await.unwrap();
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
            let drive = crate::drive::DriveClient::new(Url::parse(&format!("{}/", self.server.uri())).unwrap(), Arc::new(StaticToken::new("T"))).unwrap();
            let root = SyncRoot { path: self.folder.path().canonicalize().unwrap(), root_id: "r".into() };
            ThumbnailFiller::new(drive, self.store.clone(), root, self.cache.path().to_path_buf()).with_pause(Duration::ZERO)
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
        w.store.run(|s| { s.begin_staging(true)?; s.stage(&[photo("P", "q.jpg", "image/jpeg")])?; s.commit_staging("L2") }).await.unwrap();
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
        w.store.run(move |s| { s.begin_staging(false)?; s.stage(&[root, photo("P", "p.jpg", "image/jpeg")])?; s.commit_staging("L2") }).await.unwrap();
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
}
