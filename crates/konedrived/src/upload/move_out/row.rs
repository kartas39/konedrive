//! One `move-out` row's step: where its object is now, and which case that is.

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, State};
use konedrive_tree::outbox::{place_name, OutboxRow, Reason, Snapshot};
use konedrive_tree::Store;

use super::place::{in_another_folder, place_of, reopen_parent, root_path, verified_path, Place};
use super::reach::{reach, Reach};
use super::walk::item_id_of;
use super::MoveOuts;
use crate::folder::disk::Disk;
use crate::folder::locks::InodeKey;
use crate::helper::reopen_for_writing;
use crate::local::liveness::absent_at;
use crate::local::RECHECK;
use crate::upload::engine::{Engine, Fail, Outcome};
use crate::upload::steps::{blocking, share};

/// How long a first `ESTALE` for a row's own object waits before a second one is believed.
const GONE_AGAIN: Duration = Duration::from_secs(5);

/// Whether a file's content is local.
pub(super) enum Local {
    Yes,
    /// Not yet, and why: the row is tried again later.
    No(Outcome),
}

/// One `move-out` row's step, and what every case of it works with: the worker, what it has
/// for moves out, the folder, the row, and the item and the object the row names.
pub(super) struct MoveOut<'a> {
    pub(super) e: &'a Arc<Engine>,
    pub(super) mo: &'a MoveOuts,
    pub(super) disk: &'a Arc<Disk>,
    pub(super) row: &'a OutboxRow,
    /// The row's item.
    pub(super) id: &'a str,
    /// The row's object, as the examination recorded it.
    pub(super) handle: &'a FileHandle,
    /// The folder's own directory: what the helper is asked beneath.
    pub(super) root: &'a File,
}

impl MoveOut<'_> {
    /// The step: the object is asked for by its handle, and what the helper answers, then what
    /// the object is and where, say which case this is.
    pub(super) async fn step(&self) -> Result<Outcome, Fail> {
        let object = match self.reach(self.handle).await {
            Reach::Open(object) => object,
            Reach::Stale => return Ok(Outcome::backoff(Reason::StaleHandle)),
            Reach::Gone => return self.vanished().await,
            Reach::Refused => return self.refused().await,
            Reach::Busy => return Ok(Outcome::later(Reason::NotLocal, RECHECK)),
            Reach::BadHandle => return Ok(Outcome::blocked(Reason::BadHandle)),
            Reach::Errno(errno) => return Ok(Outcome::backoff(Reason::Unreachable(Some(format!("errno {errno}"))))),
            Reach::NoHelper(err) => {
                tracing::debug!("{}: {err}", self.row.rel.display());
                return Ok(Outcome::backoff(Reason::NoHelper));
            }
        };
        let carried = {
            let object = Arc::clone(&object);
            blocking(move || Ok(item_id_of(&object))).await?
        };
        if carried.as_deref() != Some(self.id) {
            // The helper hands over only an object carrying an item id: another one's is no answer.
            return Ok(Outcome::blocked(Reason::AnotherItem));
        }
        remember_place(self.e.store(), self.row, &object).await?;
        let is_dir = object.metadata()?.is_dir();
        match self.place(&object, self.handle).await? {
            // A marker stays: what it took off may be taken off already.
            Place::Inside => {
                if let Some(outcome) = self.superseded().await? {
                    return Ok(outcome);
                }
                Ok(Outcome::backoff(Reason::BackInside))
            }
            Place::Unknown => Ok(Outcome::backoff(Reason::PlaceUnknown)),
            Place::Trash(entry) if is_dir => self.trashed_folder(object, &entry).await,
            // A hard-linked placeholder is not only in the Trash: it is downloaded, as anywhere else.
            Place::Trash(entry) if object.metadata()?.nlink() == 1 => self.trashed_file(object, &entry).await,
            Place::Trash(_) | Place::Elsewhere(_) if is_dir => self.elsewhere_folder(object).await,
            Place::Trash(_) | Place::Elsewhere(_) => self.elsewhere_file(object).await,
        }
    }

    /// The object is gone by the helper's answer. Believed as the user's delete only for what
    /// this row did not remove itself, when said twice, some seconds apart, and with its
    /// evidence: nothing, or another object, where it was last proved to be (an inode that
    /// cannot be read says `ESTALE` every time, and stands there).
    async fn vanished(&self) -> Result<Outcome, Fail> {
        // What this row removed or stripped itself.
        if marker(self.row) {
            return self.finish().await;
        }
        if self.row.reason != Some(Reason::GoneOnce) {
            return Ok(Outcome::later(Reason::GoneOnce, GONE_AGAIN));
        }
        if !absent(self.row.last_place(), self.handle).await? {
            return Ok(Outcome::backoff(Reason::GoneUnproved));
        }
        // Last proved inside another account's folder: that account may have taken it for none
        // of its own. Nothing is deleted in OneDrive.
        if self.last_in_another_folder().await? {
            return self.kept().await;
        }
        self.gone().await
    }

    /// The helper does not hand the object over. Never "gone" (F90): the row is kept, and asked
    /// again now and then — but for the two things that explain the answer.
    async fn refused(&self) -> Result<Outcome, Fail> {
        // The attributes this very row took off: its content was proved local first.
        if marker(self.row) {
            return self.finish().await;
        }
        // The same object, where it was last proved to be inside another account's folder, with
        // no item id: that account's examination took it for its own, and uploads it. Still not
        // "gone": nothing is deleted in OneDrive, and the item comes back here.
        if self.stands_in_another_folder().await? {
            return self.kept().await;
        }
        Ok(Outcome::backoff(Reason::Unreachable(None)))
    }

    /// The helper's answer for `handle`: the row's own object, or one the base has inside it.
    pub(super) async fn reach(&self, handle: &FileHandle) -> Reach {
        reach(&*self.mo.helper, self.e.store(), self.root, handle).await
    }

    /// [`place_of`], off the runtime.
    pub(super) async fn place(&self, object: &Arc<File>, handle: &FileHandle) -> Result<Place, Fail> {
        let (mo, disk, object, handle) = (self.mo.clone(), Arc::clone(self.disk), Arc::clone(object), handle.clone());
        blocking(move || Ok(place_of(&mo, &disk, &object, &handle))).await
    }

    /// Whether the row's object was last proved to be inside another account's folder.
    async fn last_in_another_folder(&self) -> Result<bool, Fail> {
        let Some(place) = self.row.last_place().map(Path::to_owned) else { return Ok(false) };
        let (mo, disk) = (self.mo.clone(), Arc::clone(self.disk));
        blocking(move || Ok(in_another_folder(&mo, &disk, &place))).await
    }

    /// Whether the row's object still stands where it was last proved to be, inside another
    /// account's folder: the name there has the row's handle.
    async fn stands_in_another_folder(&self) -> Result<bool, Fail> {
        let Some(place) = self.row.last_place().map(Path::to_owned) else { return Ok(false) };
        let (mo, disk, handle) = (self.mo.clone(), Arc::clone(self.disk), self.handle.clone());
        blocking(move || {
            if !in_another_folder(&mo, &disk, &place) {
                return Ok(false);
            }
            let (Some(parent), Some(name)) = (place.parent(), place.file_name()) else { return Ok(false) };
            Ok(reopen_parent(parent).ok().and_then(|dir| FileHandle::at(&dir, name).ok()).as_ref() == Some(&handle))
        })
        .await
    }

    /// A newer row of the same item (the examination's, behind this running one) supersedes it:
    /// this one goes, unless it has begun to take attributes off already.
    pub(super) async fn superseded(&self) -> Result<Option<Outcome>, Fail> {
        if marker(self.row) {
            return Ok(None);
        }
        let (item, seq) = (self.id.to_owned(), self.row.seq);
        let newer = self.e.store().call(move |s| s.outbox_for_item(&item)).await?.into_iter().any(|r| r.seq > seq);
        if !newer {
            return Ok(None);
        }
        tracing::info!("{} came back before its move out was done: the newer change goes instead", self.row.rel.display());
        self.e.store().call(move |s| s.outbox_drop(seq, None, None, None)).await?;
        Ok(Some(Outcome::Done))
    }

    /// The check before a marker is written: the row is still the item's newest (a row the
    /// examination recorded behind it — the object came back, and went on — supersedes it, and
    /// it goes), and the object is still proved to be outside this account's folder. `Some` is
    /// what the row does instead.
    pub(super) async fn before_marker(&self, object: &Arc<File>) -> Result<Option<Outcome>, Fail> {
        if let Some(outcome) = self.superseded().await? {
            return Ok(Some(outcome));
        }
        let (on, at) = (Arc::clone(self.disk), Arc::clone(object));
        // Whether it is outside the folder; `None` where its place is not proved.
        let outside = blocking(move || Ok(verified_path(&at).map(|path| !root_path(&on).is_some_and(|root| path.starts_with(root))))).await?;
        Ok(match outside {
            None => Some(Outcome::backoff(Reason::PlaceUnknown)),
            Some(false) => Some(Outcome::backoff(Reason::BackInside)),
            Some(true) => None,
        })
    }

    /// Writes the row's marker (or takes it off), before anything is taken off or removed.
    pub(super) async fn set_marker(&self, marker: Option<Snapshot>) -> Result<(), Fail> {
        let seq = self.row.seq;
        Ok(self.e.store().call(move |s| s.outbox_set_snapshot(seq, marker)).await?)
    }

    /// Makes the file behind `object` (read-only, as `OpenByHandle` gives it) local where it is:
    /// marked again first, probed for a writer (one from before the mark could be writing
    /// into it), then filled through a descriptor of its own for writing, under the per-inode
    /// lock every fill takes. `Yes` only for a file that reads `hydrated` afterwards — the fill's
    /// commit point, after the whole content and its hash.
    pub(super) async fn make_local(&self, object: &Arc<File>, shown: &Path) -> Result<Local, Fail> {
        if hydrated(object).await? {
            return Ok(Local::Yes);
        }
        if let Err(e) = self.mo.helper.mark_file(object).await {
            // Without a helper nothing intercepts anything; the fill below needs none.
            tracing::debug!("{} is not marked again yet: {e}", shown.display());
        }
        match konedrive_fs::lease::open_for_writing(object) {
            Ok(false) => {}
            Ok(true) => return Ok(Local::No(Outcome::later(Reason::OpenForWriting, RECHECK))),
            // Leases off (`fs.leases-enable=0`) or not supported: nothing can tell a writer,
            // so nothing is filled.
            Err(err) => return Ok(Local::No(Outcome::backoff(Reason::NoLease(Some(err.to_string()))))),
        }
        // Reopened before the lock: the reopen is an open like any other, and is let through at
        // once only as this daemon's own (F91). A fill it could wait for takes the same lock.
        let reopened = {
            let object = Arc::clone(object);
            blocking(move || {
                let fd: OwnedFd = object.try_clone()?.into();
                placeholder::with_owner_write(&object, || reopen_for_writing(&fd))
            })
            .await
        };
        let inode = self.e.locks().lock(InodeKey::of(object)?).await;
        let state = state_of(object).await?.map_err(|e| Fail::Io(io::Error::other(e.to_string())))?;
        let clearance = match state {
            Some(State::Hydrated) => return Ok(Local::Yes),
            Some(State::OnlineOnly) => None,
            // A fill that stopped part-way (a crash) left it: continued, from its checkpoint.
            Some(State::Hydrating) => match self.mo.helper.clearance() {
                Some(clearance) => Some(clearance),
                None => return Ok(Local::No(Outcome::backoff(Reason::NoHelper))),
            },
            Some(State::Dehydrating) => return Ok(Local::No(Outcome::later(Reason::NotLocal, RECHECK))),
            // An item id with no state: nothing konedrive can fill, and nothing to be sure of.
            None => return Ok(Local::No(Outcome::backoff(Reason::NotLocal))),
        };
        let writable = match reopened {
            Ok(writable) => writable,
            // Leased (`EAGAIN`, F91), or not writable by its owner: tried again later.
            Err(Fail::Io(err)) => return Ok(Local::No(Outcome::backoff(Reason::NotOpened(Some(err.to_string()))))),
            Err(other) => return Err(other),
        };
        // Under the lock: a section of the fill that outlives it keeps the inode locked, and
        // the worker's stop waits for it.
        let counted = share().await;
        match crate::folder::locks::holding_with(&inode, counted, self.mo.filler.fill(writable, shown, clearance.as_ref())).await {
            Ok(()) if hydrated(object).await? => Ok(Local::Yes),
            Ok(()) => Ok(Local::No(Outcome::backoff(Reason::NotLocal))),
            Err(e) => {
                tracing::info!("{} could not be downloaded before its item leaves OneDrive: errno {}", shown.display(), e.errno());
                Ok(Local::No(Outcome::backoff(Reason::Download(Some(format!("errno {}", e.errno()))))))
            }
        }
    }
}

/// Whether the row carries a marker: it has begun to take attributes off, or to remove.
pub(super) fn marker(row: &OutboxRow) -> bool {
    row.snapshot().is_some_and(Snapshot::is_marker)
}

/// [`verified_path`], off the runtime.
pub(super) async fn proved_path(object: &Arc<File>) -> Result<Option<PathBuf>, Fail> {
    let object = Arc::clone(object);
    blocking(move || Ok(verified_path(&object))).await
}

/// Whether nothing, or another object, stands at `place` ([`absent_at`]): where an object was
/// last proved to be. A place that was not kept proves nothing.
pub(super) async fn absent(place: Option<&Path>, handle: &FileHandle) -> Result<bool, Fail> {
    let Some(place) = place.map(Path::to_owned) else { return Ok(false) };
    let handle = handle.clone();
    blocking(move || Ok(absent_at(&place, &handle))).await
}

/// A file's state, read off the runtime.
async fn state_of(file: &Arc<File>) -> Result<Result<Option<State>, placeholder::StateError>, Fail> {
    let file = Arc::clone(file);
    blocking(move || Ok(placeholder::read_state(&file))).await
}

/// Whether a file reads `hydrated`.
pub(super) async fn hydrated(file: &Arc<File>) -> Result<bool, Fail> {
    Ok(matches!(state_of(file).await?, Ok(Some(State::Hydrated))))
}

/// Keeps where `object` is now, proved, as the row's last place (its `target_name`): what an
/// `ESTALE` is checked against. A name that is not UTF-8 is not kept: its `ESTALE` stays
/// unproved.
pub(super) async fn remember_place(store: &Store, row: &OutboxRow, object: &Arc<File>) -> Result<(), Fail> {
    let Some(path) = proved_path(object).await? else { return Ok(()) };
    let Some(text) = place_name(&path).filter(|t| row.target_name.as_deref() != Some(*t)) else { return Ok(()) };
    let (seq, text) = (row.seq, text.to_owned());
    Ok(store.call(move |s| s.outbox_set_target(seq, None, Some(&text))).await?)
}
