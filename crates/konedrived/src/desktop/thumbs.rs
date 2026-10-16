//! Thumbnails from OneDrive: Graph's own thumbnail of every file of a type a thumbnail is
//! made for — images, videos and documents (the store's `HAS_THUMBNAIL_TYPE`) — written
//! into the freedesktop thumbnail cache under the name KIO looks for
//! (`docs/kio-behavior.md`), so that Dolphin draws it without opening — and so
//! downloading — the file. In the background, a few requests at once and none while
//! anything else has work for OneDrive ([`request_start`]): a thumbnail is a request to the
//! Graph API, and takes no slot of the account's transfer pool.
//!
//! Only `normal`, `large` and `x-large` (up to 512 px) are filled: one Graph
//! request per file (`c512x512`), scaled down locally to the smaller sizes.
//! `xx-large` (1024 px) is not filled (`docs/design/desktop.md` §8).
//!
//! Every answer that settles whether an item has a usable thumbnail is
//! recorded (the store's thumbnail marks), so the item is asked for again only once it
//! changes; only a passing trouble — no answer, `401`, `408`, `429`, 5xx — is
//! tried again at the next drain. A `406` at `c512x512` is asked
//! once more at Graph's named size `large` before it counts as a refusal. A thumbnail
//! that cannot be written into the cache records nothing and ends the drain: the next
//! one asks again.
//!
//! The cache itself is looked at in the first drain after the folder's sync started and
//! in the one the ten-minute timer starts ([`DrainCause`]): a written thumbnail of which
//! the cache lacks a size ([`decide_cached_thumbnail`]) has its mark forgotten, and the
//! same drain makes it again.
//!
//! How many thumbnails are being downloaded now is `Transfers.ThumbnailDownloads`
//! (`TransferStatus::thumbnail_downloads`), written here as each request starts and ends.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use md5::Digest;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub(crate) mod request_start;

use crate::folder::root::SyncRoot;
use crate::conditions::running::Running;
use crate::status::snapshot::SyncStateHandle;
use konedrive_graph::pool::Class;
use request_start::{decide_request_start, ListingProgress, RequestFacts, RequestStart, RequestWait, ASKED_AGAIN_AFTER};
use konedrive_graph::drive::{DriveClient, Thumbnail};
use konedrive_tree::{LocalItemId, Store, ThumbnailCandidate as Wanted};

/// How often the timer starts a drain, however often the nudges come: in case one was
/// missed, and to look into the cache.
const TIMER_PERIOD: Duration = Duration::from_secs(600);

/// The cache directories KIO consults, and the longest edge of each
/// (`docs/kio-behavior.md` §A). `xx-large` is deliberately absent: it would be a second
/// request per file (`docs/design/desktop.md` §8).
pub(crate) const SIZES: &[(&str, u32)] = &[("normal", 128), ("large", 256), ("x-large", 512)];
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
pub(crate) fn file_uri(path: &Path) -> String {
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

pub(crate) struct ThumbnailFiller {
    drive: DriveClient,
    store: Store,
    root: SyncRoot,
    cache: PathBuf,
    /// Whether thumbnails are asked for now: the account's setting, and its pause
    /// (`conditions::running`).
    running: Arc<Running>,
    /// Where the count of the thumbnails being downloaded is said.
    state: SyncStateHandle,
}

/// What one `run_once` did: `taken` is how many candidates it
/// looked at, `written` how many thumbnails it actually wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RunOutcome {
    pub(crate) taken: usize,
    pub(crate) written: usize,
}

/// What started a drain, which decides whether it looks into the cache before it reads
/// its candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainCause {
    /// The first drain since the folder's sync started, whatever woke it: the cache may
    /// have been emptied while the folder's sync did not run. Looks into the cache.
    SyncStarted,
    /// The ten-minute timer ([`TIMER_PERIOD`]). Looks into the cache.
    TimerFired,
    /// A listing ended or steps changed the folder (the nudge), or thumbnails were turned
    /// on again. These come often while files change: they go by the marks alone.
    Nudged,
}

/// What came of asking for one candidate's thumbnail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateOutcome {
    /// The thumbnail was written into the cache, and recorded so.
    Written,
    /// OneDrive has no usable thumbnail of the content: none, a refusal, or bytes that are
    /// no image. Recorded, so that it is not asked again until the content or the path
    /// changes.
    NoThumbnail,
    /// Nothing settled: a passing trouble, or the stop came first. Nothing is recorded,
    /// and the next drain asks again.
    NotSettled,
    /// The cache cannot be written. Nothing is recorded, and the drain stops: no further
    /// request starts in it.
    CacheUnwritable,
}

/// Whether the cache holds a file's thumbnail whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CachedThumbnail {
    /// The PNG of every size the filler writes ([`SIZES`]) is there.
    Whole,
    /// The PNG of one size or more is not: the thumbnail is made again, since KIO makes a
    /// size it does not find by reading the file.
    Missing,
}

/// What making a thumbnail from the bytes Graph gave back can fail at, once
/// there are bytes to work with at all.
enum FillError {
    /// The bytes are not a usable image: corrupt, an unsupported format, or
    /// over the decode limits ([`MAX_DECODE_EDGE`]/[`MAX_DECODE_ALLOC`]).
    /// Recorded like a 404 — asking Graph again would only get the same
    /// bytes back.
    Undecodable(String),
    /// A local problem: disk, permissions, a vanished cache directory. Not recorded: the
    /// drain stops, and the next asks again ([`CandidateOutcome::CacheUnwritable`]).
    Io(String),
}

impl ThumbnailFiller {
    pub fn new(drive: DriveClient, store: Store, root: SyncRoot, cache: PathBuf, running: Arc<Running>, state: SyncStateHandle) -> Self {
        Self { drive, store, root, cache, running, state }
    }

    /// What [`decide_request_start`] is asked with, read now: the listing from the store,
    /// the throttle, the opens and what waits from the account's transfer pool. A store
    /// that cannot be read says the listing has not ended: no request starts.
    async fn read_request_facts(&self, requests_under_way: usize) -> RequestFacts {
        let read = self.store.call(|store| Ok((store.delta_link()?.is_some(), store.item_store().find_listing_under_way()?.is_some()))).await;
        let listing = match read {
            Ok((true, false)) => ListingProgress::Ended,
            Ok((true, true)) => ListingProgress::UnderWay,
            Ok((false, _)) => ListingProgress::FullNotEnded,
            Err(e) => {
                tracing::warn!("cannot read how far the listing is, no thumbnail is asked for: {e}");
                ListingProgress::FullNotEnded
            }
        };
        let pool = self.drive.pool();
        RequestFacts {
            listing,
            throttle_left: Duration::from_secs(u64::from(pool.throughput().retry_after)),
            is_file_being_opened: pool.held(Class::Open) > 0,
            is_transfer_waiting: pool.waiting(),
            requests_under_way,
        }
    }

    /// Makes up to `limit` missing thumbnails, a request starting whenever
    /// [`decide_request_start`] lets one.
    #[cfg(test)]
    pub(crate) async fn run_once(&self, cancel: &CancellationToken, limit: usize) -> RunOutcome {
        self.run_from(cancel, limit, None).await.0
    }

    /// `run_once` over the candidates after the item `after`
    /// (`ItemReader::list_thumbnail_candidates`, by local item id);
    /// with the item the next batch goes on from, `None` once every candidate
    /// has been looked at, and once the cache could not be written: what was started then
    /// ends, and no further request starts.
    async fn run_from(&self, cancel: &CancellationToken, limit: usize, after: Option<LocalItemId>) -> (RunOutcome, Option<LocalItemId>) {
        // Asked before the candidates are read: while a listing goes on the plan's task
        // nudges the filler after every round, and each drain would read them for nothing.
        if matches!(decide_request_start(&self.read_request_facts(0).await), RequestStart::Wait(RequestWait::FullListingNotEnded | RequestWait::ListingUnderWay)) {
            return (RunOutcome::default(), None);
        }
        let candidates = match self.store.read_items(move |reader| reader.list_thumbnail_candidates(after, limit)).await {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!("cannot list the thumbnails to make: {e}");
                return (RunOutcome::default(), None);
            }
        };
        // A full batch may have more after it; a short one was the last.
        let mut next = candidates.last().map(|last| last.item.local_item_id).filter(|_| limit > 0 && candidates.len() >= limit);
        let taken = candidates.len();
        let mut running = tokio::task::JoinSet::new();
        let (mut written, mut is_cache_unwritable) = (0, false);
        let mut count = |done: Result<CandidateOutcome, tokio::task::JoinError>| {
            let outcome = done.unwrap_or(CandidateOutcome::NotSettled);
            written += usize::from(outcome == CandidateOutcome::Written);
            is_cache_unwritable |= outcome == CandidateOutcome::CacheUnwritable;
            is_cache_unwritable
        };
        for wanted in candidates {
            // Turned off or paused meanwhile: no more requests; what was not asked waits
            // for the next drain.
            if !self.running.thumbnails_go() {
                break;
            }
            // The wait for its turn, and the request — which can wait out Graph's
            // `Retry-After`, up to 300 s, four times — give way to a stop: the stop of the
            // folder's sync waits for this task.
            let is_started = loop {
                // What ended meanwhile is counted first: once the cache could not be
                // written, no further request starts in this drain.
                let mut is_stopped_by_cache = false;
                while let Some(done) = running.try_join_next() {
                    is_stopped_by_cache = count(done);
                    self.state.set_thumbnail_downloads(running.len());
                }
                if is_stopped_by_cache {
                    break false;
                }
                let wait = match decide_request_start(&self.read_request_facts(running.len()).await) {
                    RequestStart::Start => break true,
                    // The listing's end nudges the filler: the drain ends here, and what
                    // was not asked waits for the one the nudge starts.
                    RequestStart::Wait(RequestWait::FullListingNotEnded | RequestWait::ListingUnderWay) => break false,
                    RequestStart::Wait(RequestWait::Throttled { left }) => left,
                    RequestStart::Wait(RequestWait::FileBeingOpened | RequestWait::TransferWaiting | RequestWait::RequestsAtLimit) => ASKED_AGAIN_AFTER,
                };
                tokio::select! {
                    () = cancel.cancelled() => break false,
                    () = tokio::time::sleep(wait) => {}
                    Some(done) = running.join_next(), if !running.is_empty() => {
                        let is_stopped_by_cache = count(done);
                        self.state.set_thumbnail_downloads(running.len());
                        if is_stopped_by_cache {
                            break false;
                        }
                    }
                }
            };
            if !is_started {
                next = None;
                break;
            }
            let one = One { drive: self.drive.clone(), store: self.store.clone(), cache: self.cache.clone(), folder: self.root.path.clone() };
            let cancel = cancel.clone();
            running.spawn(async move {
                tokio::select! {
                    () = cancel.cancelled() => CandidateOutcome::NotSettled,
                    outcome = one.make(wanted) => outcome,
                }
            });
            self.state.set_thumbnail_downloads(running.len());
        }
        while let Some(done) = running.join_next().await {
            count(done);
            self.state.set_thumbnail_downloads(running.len());
        }
        if is_cache_unwritable {
            next = None;
        }
        (RunOutcome { taken, written }, next)
    }

    /// Batches of up to `limit`, each going on where the last stopped,
    /// until every candidate has been looked at once — a
    /// candidate that failed this time waits for the next drain — or
    /// cancellation, or the cache could not be written. Started by `cause`: the first drain
    /// after the folder's sync started and the timer's look into the cache first
    /// ([`forget_marks_of_missing_thumbnails`](Self::forget_marks_of_missing_thumbnails)),
    /// so that the thumbnails it lacks are among the candidates.
    async fn drain(&self, cancel: &CancellationToken, limit: usize, cause: DrainCause) -> RunOutcome {
        if matches!(cause, DrainCause::SyncStarted | DrainCause::TimerFired) {
            self.forget_marks_of_missing_thumbnails(cancel, limit).await;
        }
        let mut total = RunOutcome::default();
        let mut after = None;
        loop {
            let (outcome, next) = self.run_from(cancel, limit, after).await;
            total.taken += outcome.taken;
            total.written += outcome.written;
            if next.is_none() || cancel.is_cancelled() || !self.running.thumbnails_go() {
                break;
            }
            after = next;
        }
        total
    }

    /// Forgets the marks of the thumbnails the cache does not hold whole, so that the drain
    /// makes them again: the marks that stand of a thumbnail written
    /// (`ItemReader::list_thumbnail_marks`), `limit` at a time, each looked up in the cache
    /// ([`find_cached_sizes`], [`decide_cached_thumbnail`]), and those `Missing` forgotten
    /// in one store write per batch (`ItemStore::forget_thumbnail_marks`). A cache that
    /// cannot be looked into tells nothing: the look ends there. Three `stat`s per mark.
    async fn forget_marks_of_missing_thumbnails(&self, cancel: &CancellationToken, limit: usize) {
        let mut forgotten = 0;
        let mut after = None;
        while !cancel.is_cancelled() {
            let marks = match self.store.read_items(move |reader| reader.list_thumbnail_marks(after, limit)).await {
                Ok(marks) => marks,
                Err(e) => {
                    tracing::warn!("cannot list the thumbnails made, to look for them in the cache: {e}");
                    break;
                }
            };
            // A full batch may have more after it; a short one was the last.
            let next = marks.last().map(|last| last.item).filter(|_| limit > 0 && marks.len() >= limit);
            let (cache, folder) = (self.cache.clone(), self.root.path.clone());
            let looked = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<LocalItemId>> {
                let mut missing = Vec::new();
                for mark in marks {
                    if decide_cached_thumbnail(&find_cached_sizes(&cache, &folder.join(&mark.path))?) == CachedThumbnail::Missing {
                        missing.push(mark.item);
                    }
                }
                Ok(missing)
            });
            let missing = match looked.await {
                Ok(Ok(missing)) => missing,
                Ok(Err(e)) => {
                    tracing::warn!("cannot look into the thumbnail cache: {e}");
                    break;
                }
                Err(e) => {
                    tracing::warn!("the look into the thumbnail cache failed: {e}");
                    break;
                }
            };
            if !missing.is_empty() {
                let count = missing.len();
                if let Err(e) = self.store.call(move |store| store.item_store().forget_thumbnail_marks(&missing)).await {
                    tracing::warn!("cannot forget the marks of the thumbnails missing from the cache: {e}");
                    break;
                }
                forgotten += count;
            }
            if next.is_none() {
                break;
            }
            after = next;
        }
        if forgotten > 0 {
            tracing::info!("the cache lacks {forgotten} thumbnails made before; they are made again");
        }
    }

    /// Runs in the background: after every cycle (`kick`), when thumbnails are
    /// turned on again, and every ten minutes ([`TIMER_PERIOD`]) in case a kick was missed,
    /// draining 200 thumbnails at a time until every candidate has been looked at. The
    /// first drain, and the timer's, look into the cache first ([`DrainCause`]).
    pub(crate) fn spawn(self, kick: Arc<Notify>, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // A timer of its own, which a kick does not start over: kicks come every
            // minute while the folder is listed.
            let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + TIMER_PERIOD, TIMER_PERIOD);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut first = Some(DrainCause::SyncStarted);
            loop {
                let woke = tokio::select! {
                    () = kick.notified() => DrainCause::Nudged,
                    () = self.running.thumbnails_turned_on() => DrainCause::Nudged,
                    _ = timer.tick() => DrainCause::TimerFired,
                    () = cancel.cancelled() => return,
                };
                // Turned off, or paused (`docs/design/writes.md` §6.3): no request; the
                // next kick after the pause ends, or turning them on, drains what waits.
                if !self.running.thumbnails_go() {
                    continue;
                }
                self.drain(&cancel, 200, first.take().unwrap_or(woke)).await;
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
    /// Asks Graph for the thumbnail of `wanted`, caches it and records what settled.
    async fn make(self, wanted: Wanted) -> CandidateOutcome {
        // A candidate is a file OneDrive has: its binding is what Graph is asked by, and the content OneDrive has
        // is what the thumbnail is of.
        let (Some(reference), Some(content)) = (wanted.item.onedrive_item.clone(), wanted.item.onedrive.as_ref().and_then(|version| version.content.clone())) else {
            return CandidateOutcome::NotSettled;
        };
        let (id, rel) = (reference.item_id.as_str(), &wanted.path);
        let mut fetched = self.drive.thumbnail(id, GRAPH_SIZE).await;
        if matches!(fetched, Ok(Thumbnail::Refused(konedrive_graph::drive::Status::NOT_ACCEPTABLE))) {
            fetched = self.drive.thumbnail(id, FALLBACK_SIZE).await;
        }
        // What settles the question of whether the item has a usable thumbnail is
        // recorded: a write, a 404, a refusal, an oversized body, bytes that will not
        // decode. A passing trouble (`DriveClient::thumbnail`'s `Err`) and a cache that
        // cannot be written are not, and are asked again at the next drain.
        let outcome = match fetched {
            Ok(Thumbnail::Image(bytes)) => {
                let (cache, file, mtime) = (self.cache.clone(), self.folder.join(rel), content.mtime().0);
                match tokio::task::spawn_blocking(move || write_thumbnail(&cache, &file, mtime, &bytes)).await {
                    Ok(Ok(())) => CandidateOutcome::Written,
                    Ok(Err(FillError::Undecodable(reason))) => {
                        tracing::warn!("no usable thumbnail for {}: {reason}", rel.display());
                        CandidateOutcome::NoThumbnail
                    }
                    Ok(Err(FillError::Io(reason))) => {
                        tracing::warn!("cannot cache the thumbnail of {}, no more are asked for until the next drain: {reason}", rel.display());
                        CandidateOutcome::CacheUnwritable
                    }
                    Err(e) => {
                        tracing::warn!("the thumbnail task for {} failed: {e}", rel.display());
                        CandidateOutcome::NotSettled
                    }
                }
            }
            Ok(Thumbnail::None) => CandidateOutcome::NoThumbnail,
            Ok(Thumbnail::Refused(status)) => {
                // Once per version of the item: recorded, so not asked again
                // until it changes.
                tracing::info!("OneDrive refuses a thumbnail of {}: {status}", rel.display());
                CandidateOutcome::NoThumbnail
            }
            Err(e) => {
                tracing::info!("no thumbnail for {} this time: {e}", rel.display());
                CandidateOutcome::NotSettled
            }
        };
        let is_written = match outcome {
            CandidateOutcome::Written => true,
            CandidateOutcome::NoThumbnail => false,
            CandidateOutcome::NotSettled | CandidateOutcome::CacheUnwritable => return outcome,
        };
        let (item, path) = (wanted.item.local_item_id, wanted.path.clone());
        if let Err(e) = self.store.call(move |store| store.item_store().set_thumbnail_made(item, &content, &path, is_written)).await {
            tracing::warn!("cannot record the thumbnail of {}: {e}", wanted.path.display());
        }
        outcome
    }
}

/// Whether the cache holds a file's thumbnail whole, given the sizes whose PNG a look
/// found there (`found`, by the name of their directory). Pure: the `stat`s are
/// [`find_cached_sizes`].
fn decide_cached_thumbnail(found: &[&str]) -> CachedThumbnail {
    if SIZES.iter().all(|(size, _)| found.contains(size)) {
        CachedThumbnail::Whole
    } else {
        CachedThumbnail::Missing
    }
}

/// The sizes of [`SIZES`] whose PNG the cache holds for the file at `file`, under the
/// name [`write_thumbnail`] gives it: a `stat` of each. An error other than the PNG's
/// absence is one of a cache that cannot be looked into. Blocking.
fn find_cached_sizes(cache: &Path, file: &Path) -> std::io::Result<Vec<&'static str>> {
    let name = thumbnail_name(file);
    let mut found = Vec::new();
    for (size, _) in SIZES {
        match std::fs::metadata(cache.join(size).join(&name)) {
            Ok(metadata) if metadata.is_file() => found.push(*size),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(found)
}

/// The name of the file at `file`'s thumbnail in the directory of each size: the MD5 of
/// its URI ([`file_uri`]), as KIO names it (`docs/kio-behavior.md` §A).
fn thumbnail_name(file: &Path) -> String {
    format!("{:x}.png", md5::Md5::digest(file_uri(file).as_bytes()))
}

/// Decodes `jpeg` (any format `image` recognises, under the decode limits),
/// scales it to each of `SIZES` and writes it into the freedesktop cache
/// atomically (a temp file, then a rename) — all synchronous, so callers run
/// it with `spawn_blocking` rather than on the async task.
fn write_thumbnail(cache: &Path, file: &Path, mtime: i64, jpeg: &[u8]) -> Result<(), FillError> {
    let uri = file_uri(file);
    let name = thumbnail_name(file);

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
