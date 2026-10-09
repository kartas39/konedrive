//! The download of a whole file: into a file without a name, which gets the item's name once
//! it is whole and checked.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use konedrive_fs::placeholder::State;
use konedrive_graph::pool::{Class, Size};
use konedrive_tree::Row;
use tokio::sync::oneshot;

use crate::hydration::source::{download_staged, ContentSource, Split};
use crate::hydration::tracked::Tracked;
use crate::status::activity::{self, Kind as EventKind};

use super::announce::Announce;
use super::shared::{seconds, Shared};
use super::backing_directory::LandingOutcome;

/// How many downloads that failed keep what they have, to be continued from.
pub(super) const UNFINISHED: usize = 16;

/// A caller in the daemon that waits for a download to end.
pub(super) type Waiter = oneshot::Sender<Result<(), i32>>;

/// A download in one stream that is under way: how far its staging file is written in order,
/// and where that file is. What lies well below the mark is read from it.
#[derive(Clone)]
pub(super) struct Running {
    pub(super) mark: Arc<AtomicU64>,
    pub(super) staged: Arc<File>,
}

/// How far behind the mark the staging file is surely written: the download writes each piece
/// it has read before it reads the next, and a piece is no larger than this.
pub(super) const WRITTEN_BEHIND: u64 = 1 << 20;

/// The download's source, with the mark moved as its one stream is read. A download in parts
/// (`end` given) writes out of order and moves no mark.
struct Marked<'a> {
    inner: &'a dyn ContentSource,
    mark: Arc<AtomicU64>,
}

struct Marking {
    inner: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    mark: Arc<AtomicU64>,
}

impl tokio::io::AsyncRead for Marking {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &polled {
            self.mark.fetch_add((buf.filled().len() - before) as u64, Ordering::SeqCst);
        }
        polled
    }
}

#[async_trait::async_trait]
impl ContentSource for Marked<'_> {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<crate::hydration::source::Fetched, crate::hydration::source::SourceError> {
        let mut fetched = self.inner.fetch(item_id, from, end).await?;
        if end.is_none() {
            // Also when the download starts over: what is on disk counts from here again.
            self.mark.store(fetched.served_from, Ordering::SeqCst);
            fetched.stream = Box::new(Marking { inner: fetched.stream, mark: Arc::clone(&self.mark) });
        }
        Ok(fetched)
    }

    fn progress(&self, done: u64, size: u64) {
        self.inner.progress(done, size);
    }
}

impl Shared {
    /// Downloads `row` under a staging name, as every download of the daemon is made (breaks,
    /// the version it belongs to, the hash), and renames it into place. Its size, or the errno
    /// it failed with.
    pub(super) async fn fill(&self, row: &Row, rel: &Path, class: Class) -> Result<u64, i32> {
        let io_errno = |e: io::Error| e.raw_os_error().unwrap_or(libc::EIO);
        let mut slot = self.pool.acquire_sized(class, Size::of(row.size)).await;
        // The file a download is built in has no name: it is in the folder's filesystem and in
        // no directory, so nothing ever lists a file that is not whole, and one the daemon
        // leaves behind goes by itself. What an earlier download of the item left is
        // continued from its checkpoint when it is of the same version.
        let earlier = {
            let mut unfinished = crate::panic::lock(&self.unfinished);
            unfinished.iter().position(|(id, _)| *id == row.id).map(|at| unfinished.remove(at).1)
        };
        let file = match earlier {
            Some(file) => file,
            None => Arc::new(self.backing_directory.unnamed().map_err(io_errno)?),
        };
        let shown = self.shown(rel);
        let tracked = match class {
            Class::Open => Tracked::opening(Arc::clone(&self.source), self.report.transfers.clone(), shown),
            _ => Tracked::new(Arc::clone(&self.source), self.report.transfers.clone(), shown),
        };
        // What of the download is on disk in order, for the reads that come meanwhile.
        let mark = Arc::new(AtomicU64::new(0));
        crate::panic::lock(&self.running).insert(row.id.clone(), Running { mark: Arc::clone(&mark), staged: Arc::clone(&file) });
        let marked = Marked { inner: &tracked, mark };
        let done = async {
            // A large file that nothing waits to open (a pinned one) goes in parallel parts, the
            // slot held here being its first stream's.
            let split = (class != Class::Open && Size::of(row.size) == Size::Large).then(|| Split::new(Arc::clone(&self.pool), Arc::clone(&self.parts)));
            download_staged(&file, &row.id, row.size, &marked, split.as_ref()).await?;
            let size = tracked.fetched().unwrap_or(row.size);
            // The directories down to its place, each marked as its item's.
            self.directory_contents.make_marked_directories(rel.parent().unwrap_or(Path::new(""))).map_err(io_errno)?;
            // Moved or removed here while it was downloaded: the download is of no place now,
            // and the next read starts one where the item stands.
            if self.directory_contents.find_path(&row.id).as_deref() != Some(rel) {
                return Err(libc::ESTALE);
            }
            let (id, modified, at, file, backing_directory) = (row.id.clone(), seconds(row.mtime), rel.to_owned(), Arc::clone(&file), Arc::clone(&self.backing_directory));
            // The file's own steps wait for the disk: not on the runtime's threads.
            let landed = tokio::task::spawn_blocking(move || backing_directory.land(&file, &at, &id, modified)).await.map_err(|_| libc::EIO)?.map_err(io_errno)?;
            if landed == LandingOutcome::Refused {
                return Err(libc::EEXIST);
            }
            Ok(size)
        }
        .await;
        drop(tracked);
        crate::panic::lock(&self.running).remove(&row.id);
        match done {
            Ok(_) => {
                slot.succeeded();
                crate::panic::lock(&self.asked).remove(&row.id);
            }
            // A download that failed keeps what it has for as long as the daemon runs: the
            // next one continues from its checkpoint. Only the latest few are kept.
            Err(_) => {
                let mut unfinished = crate::panic::lock(&self.unfinished);
                unfinished.push((row.id.clone(), file));
                if unfinished.len() > UNFINISHED {
                    unfinished.remove(0);
                }
            }
        }
        done
    }

    /// What the folder has of the file `row` at `rel`: being downloaded, downloaded, or neither.
    pub(super) fn state_of(&self, row: &Row, rel: &Path) -> State {
        if crate::panic::lock(&self.fills).contains_key(&row.id) {
            State::Hydrating
        } else if self.backing_directory.has_downloaded_copy(rel, &row.id) {
            State::Hydrated
        } else {
            State::OnlineOnly
        }
    }

    /// Downloads `row` to `rel` unless a download of it runs already; `waiter` is answered when
    /// the download ends, however it ends.
    pub(super) fn ensure(self: &Arc<Self>, row: Row, rel: PathBuf, class: Class, waiter: Option<Waiter>) {
        {
            let mut fills = crate::panic::lock(&self.fills);
            if let Some(waiting) = fills.get_mut(&row.id) {
                return waiting.extend(waiter);
            }
            fills.insert(row.id.clone(), waiter.into_iter().collect());
        }
        // `hydrating`, for whoever shows the state.
        self.announce(vec![Announce::Changed(rel.clone(), row.id.clone())]);
        let s = Arc::clone(self);
        self.runtime.spawn(async move {
            // A download that panics still leaves no open waiting for it for ever.
            struct Ended(Arc<Shared>, String);
            impl Drop for Ended {
                fn drop(&mut self) {
                    crate::panic::lock(&self.0.running).remove(&self.1);
                    for waiter in crate::panic::lock(&self.0.fills).remove(&self.1).unwrap_or_default() {
                        let _ = waiter.send(Err(libc::EIO));
                    }
                }
            }
            let _ended = Ended(Arc::clone(&s), row.id.clone());
            let filled = s.fill(&row, &rel, class).await;
            let shown = s.shown(&rel);
            let event = match &filled {
                Ok(size) => activity::event(EventKind::Downloaded, shown, activity::human_size(*size)),
                Err(errno) => activity::event(EventKind::Failed, shown, activity::failure_reason(*errno)),
            };
            s.report.activity.record(vec![event]).await;
            s.report.space.kick();
            let waiting = crate::panic::lock(&s.fills).remove(&row.id).unwrap_or_default();
            for waiter in waiting {
                let _ = waiter.send(filled.map(|_| ()));
            }
            s.announce(vec![Announce::Changed(rel, row.id)]);
        });
    }
}
