//! Thumbnails from OneDrive: Graph's own thumbnail of
//! every image and video, written into the freedesktop thumbnail cache under
//! the name KIO looks for (`docs/kio-behavior.md`), so that Dolphin draws it
//! without opening — and so downloading — the file. In the background, each
//! request in a slot of the account's transfer pool, like any background download.
//!
//! Only `normal`, `large` and `x-large` (up to 512 px) are filled: one Graph
//! request per image (`c512x512`), scaled down locally to the smaller sizes.
//! `xx-large` (1024 px) is not filled — see limitations log entry K15.
//!
//! Every answer that settles whether an item has a usable thumbnail is
//! recorded (its `thumb_key`), so the item is asked for again only once it
//! changes; only a passing trouble — no answer, `401`, `408`, `429`, 5xx — is
//! tried again at the next drain (issue #80). A `406` at `c512x512` is asked
//! once more at Graph's named size `large` before it counts as a refusal.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use md5::Digest;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::folder::root::SyncRoot;
use crate::conditions::running::Running;
use konedrive_graph::drive::{DriveClient, Thumbnail};
use konedrive_tree::{Row, Store};

/// The cache directories KIO consults, and the longest edge of each
/// (`docs/kio-behavior.md` §A). `xx-large` is deliberately absent — see
/// limitations log entry K15.
pub const SIZES: &[(&str, u32)] = &[("normal", 128), ("large", 256), ("x-large", 512)];
/// The one size asked of Graph; the smaller ones are scaled from it.
const GRAPH_SIZE: &str = "c512x512";
/// Asked instead when Graph answers [`GRAPH_SIZE`] with `406 Not Acceptable`,
/// as it does for some items: Graph's named size, up to 800 px, scaled down
/// the same way.
const FALLBACK_SIZE: &str = "large";
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
    /// Whether thumbnails are asked for now: the account's setting, and its pause
    /// (`conditions::running`).
    running: Arc<Running>,
}

/// What one `run_once` did: `taken` is how many candidates it
/// looked at, `written` how many thumbnails it actually wrote.
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
    /// A local problem: disk, permissions, a vanished cache directory.
    /// Recorded like the rest (issue #39): a batch of them ends the drain,
    /// and the item is asked for again once it changes.
    Io(String),
}

impl ThumbnailFiller {
    pub fn new(drive: DriveClient, store: Store, root: SyncRoot, cache: PathBuf, running: Arc<Running>) -> Self {
        Self { drive, store, root, cache, running }
    }

    /// Makes up to `limit` missing thumbnails, each request in a background slot of the
    /// account's transfer pool, as many at once as the pool gives.
    pub async fn run_once(&self, cancel: &CancellationToken, limit: usize) -> RunOutcome {
        self.run_from(cancel, limit, String::new()).await.0
    }

    /// [`run_once`](Self::run_once) over the candidates after id `after`;
    /// with the id the next batch goes on from, `None` once every candidate
    /// has been looked at.
    async fn run_from(&self, cancel: &CancellationToken, limit: usize, after: String) -> (RunOutcome, Option<String>) {
        let (candidates, next) = match self.store.call(move |s| s.thumbnail_candidates(&after, limit)).await {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!("cannot list the thumbnails to make: {e}");
                return (RunOutcome::default(), None);
            }
        };
        let taken = candidates.len();
        let mut running = tokio::task::JoinSet::new();
        let mut written = 0;
        for (row, rel) in candidates {
            // Turned off or paused meanwhile: no more requests; what was not asked waits
            // for the next drain.
            if !self.running.thumbnails_go(&self.store) {
                break;
            }
            // The wait for a slot, and the request — which can wait out Graph's
            // `Retry-After`, up to 300 s, four times — give way to a stop: a
            // Forget, or a switch to interception under the lifecycle lock,
            // waits for this task.
            let slot = loop {
                tokio::select! {
                    () = cancel.cancelled() => break None,
                    slot = self.drive.pool().acquire(konedrive_graph::pool::Class::Download) => break Some(slot),
                    Some(done) = running.join_next(), if !running.is_empty() => written += usize::from(done.unwrap_or(false)),
                }
            };
            let Some(slot) = slot else { break };
            let one = One { drive: self.drive.clone(), store: self.store.clone(), cache: self.cache.clone(), folder: self.root.path.clone() };
            let cancel = cancel.clone();
            running.spawn(async move {
                tokio::select! {
                    () = cancel.cancelled() => false,
                    written = one.make(row, rel, slot) => written,
                }
            });
        }
        while let Some(done) = running.join_next().await {
            written += usize::from(done.unwrap_or(false));
        }
        (RunOutcome { taken, written }, next)
    }

    /// Batches of up to `limit`, each going on where the last stopped
    /// (issue #39), until every candidate has been looked at once — a
    /// candidate that failed this time waits for the next drain — or
    /// cancellation.
    async fn drain(&self, cancel: &CancellationToken, limit: usize) -> RunOutcome {
        let mut total = RunOutcome::default();
        let mut after = Some(String::new());
        while let Some(from) = after {
            let (outcome, next) = self.run_from(cancel, limit, from).await;
            total.taken += outcome.taken;
            total.written += outcome.written;
            if cancel.is_cancelled() || !self.running.thumbnails_go(&self.store) {
                break;
            }
            after = next;
        }
        total
    }

    /// Runs in the background: after every cycle (`kick`), when thumbnails are
    /// turned on again, and every ten minutes in case a kick was missed, draining
    /// 200 thumbnails at a time until every candidate has been looked at.
    pub fn spawn(self, kick: Arc<Notify>, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = kick.notified() => {}
                    () = self.running.thumbnails_turned_on() => {}
                    () = tokio::time::sleep(Duration::from_secs(600)) => {}
                    () = cancel.cancelled() => return,
                }
                // Turned off, or paused (`docs/design/writes.md` §11): no request; the
                // next kick after the pause ends, or turning them on, drains what waits.
                if !self.running.thumbnails_go(&self.store) {
                    continue;
                }
                self.drain(&cancel, 200).await;
            }
        })
    }
}

/// What one thumbnail needs, for a task of its own.
struct One {
    drive: DriveClient,
    store: Store,
    cache: PathBuf,
    folder: PathBuf,
}

impl One {
    /// Asks Graph for the thumbnail of `row` and caches it; whether one was written.
    async fn make(self, row: Row, rel: PathBuf, mut slot: konedrive_graph::pool::Slot) -> bool {
        let key = thumb_key(&row, &rel);
        let mut fetched = self.drive.thumbnail(&row.id, GRAPH_SIZE).await;
        if matches!(fetched, Ok(Thumbnail::Refused(konedrive_graph::drive::Status::NOT_ACCEPTABLE))) {
            fetched = self.drive.thumbnail(&row.id, FALLBACK_SIZE).await;
        }
        if fetched.is_ok() {
            slot.succeeded();
        }
        drop(slot);
        let mut written = false;
        // Whether to record `key` for this item: true for anything that
        // settles the question of whether it has a usable thumbnail
        // (a real write, a 404, a refusal, an oversized body, bytes that
        // will not decode) and for a local I/O problem, which would fail
        // the same way at once (issue #39); false for a passing trouble
        // (`DriveClient::thumbnail`'s `Err`), tried again at the next drain.
        let settle = match fetched {
            Ok(Thumbnail::Image(bytes)) => {
                let (cache, file, mtime) = (self.cache.clone(), self.folder.join(&rel), row.mtime);
                match tokio::task::spawn_blocking(move || write_thumbnail(&cache, &file, mtime, &bytes)).await {
                    Ok(Ok(())) => {
                        written = true;
                        true
                    }
                    Ok(Err(FillError::Undecodable(reason))) => {
                        tracing::warn!("no usable thumbnail for {}: {reason}", rel.display());
                        true
                    }
                    Ok(Err(FillError::Io(reason))) => {
                        tracing::warn!("cannot cache the thumbnail of {}: {reason}", rel.display());
                        true
                    }
                    Err(e) => {
                        tracing::warn!("the thumbnail task for {} failed: {e}", rel.display());
                        false
                    }
                }
            }
            Ok(Thumbnail::None) => true,
            Ok(Thumbnail::Refused(status)) => {
                // Once per version of the item: recorded, so not asked again
                // until it changes.
                tracing::info!("OneDrive refuses a thumbnail of {}: {status}", rel.display());
                true
            }
            Err(e) => {
                tracing::info!("no thumbnail for {} this time: {e}", rel.display());
                false
            }
        };
        if settle {
            let id = row.id.clone();
            if let Err(e) = self.store.call(move |s| s.set_thumb_key(&id, &key)).await {
                tracing::warn!("cannot record the thumbnail of {}: {e}", rel.display());
            }
        }
        written
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
mod tests;
