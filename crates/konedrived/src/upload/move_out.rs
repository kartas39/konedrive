//! Moves out of the folder (`docs/design/writes.md` §8, §10, WR5): a `move-out` row's step, the
//! re-marking of what left, and the routing of its fills.
//!
//! **The order is WR5's.** An object that left the folder is reached by its file handle, through
//! the helper (`OpenByHandle`), never by a path the row remembers. Then:
//!
//! - **anywhere but the Trash**, a placeholder is marked again (`MarkFile`, so any open is
//!   intercepted, M4) and downloaded through its own descriptor, the ordinary fill; a directory is
//!   walked and every placeholder of its item downloaded. Only when every byte is local and the
//!   state says so (`hydrated`, the fill's commit point, verified against OneDrive's hash), and the
//!   object is still proved to be outside the folder, is the row marked [`CONTENT_LOCAL`], the
//!   attributes taken off (the item id first), the directories unmarked (`UnmarkDir`, never one
//!   beneath a registered folder), and the item deleted in OneDrive;
//! - **in the Trash** — the user's own (`$XDG_DATA_HOME/Trash`) or a mount's (`.Trash-<uid>`,
//!   `.Trash/<uid>` at the mount's top), holding the entry's `.trashinfo` — nothing is downloaded,
//!   as Windows does: downloaded content stays there as the user's own file, a placeholder is
//!   removed with its `.trashinfo` (proved gone: no link left), and the item goes to OneDrive's
//!   recycle bin. Anything that only looks like a Trash is "anywhere else";
//! - **back inside the folder**, nothing is decided: the examination takes it on, and a row it
//!   records behind this one supersedes it.
//!
//! **Doubt keeps the row.** `EPERM` is never "gone" (a nested subvolume, another owner, an object
//! without the attribute), and neither is a helper that does not answer, a download that stopped
//! part-way, or a place that cannot be proved. `ESTALE` is "gone" only when the handles the store
//! recorded belong to the filesystem the folder is on now ([`handles_current`]), and for the row's
//! own object only when it says so twice, some seconds apart. The row's own markers are the one
//! exception to "`EPERM` is never gone": once [`CONTENT_LOCAL`] or [`TRASHED`] is written, the
//! content was proved local (or the object was in the Trash), and the `EPERM` that follows our own
//! stripping of the attributes is the expected answer.
//!
//! **A crash at each step converges** (§5): before a marker, the row starts again from the object
//! (a fill resumes from its checkpoint); after it, the attributes are taken off what is still
//! reachable (what was stripped already answers `EPERM`, which the marker explains) and the item
//! deleted; a delete whose answer was lost finds `404`.
//!
//! **Descriptors from `OpenByHandle`** are used for what the object is (`fstat`, its attributes),
//! where it is (`/proc/self/fd`, proved by opening that path again), `MarkFile`/`MarkDir`/
//! `UnmarkDir`, and a file's own reopen for writing. A directory's is never an anchor for anything
//! below it (SECURITY.md, F90): the walk opens the directory again by its path, through the user's
//! own lookups, checks that it is the same inode, and goes down one name at a time from there,
//! never following a symlink.
//!
//! [`handles_current`]: crate::local::liveness::handles_current_async

mod cases;
mod place;
mod tidy;
mod trash;
mod walk;

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use konedrive_fs::placeholder::{self, State};

use super::engine::{Engine, Fail, Outcome};
use crate::folder::disk::Disk;
use crate::helper::linked::Helper;
use crate::helper::{reopen_for_writing, Clearance, HelperError};

use crate::local::liveness::{absent_at, handles_current_async};
use crate::local::RECHECK;
use crate::hydration::source::{self, ContentSource, FillError};
use crate::folder::locks::InodeKey;
use konedrive_tree::outbox::{place_name, OutboxRow, Reason, Snapshot};
use konedrive_tree::Table;

pub(crate) use tidy::{drop_rows, Tidy};
pub use trash::{home_trash, trash_of, TrashEntry};

use cases::{elsewhere_file, elsewhere_folder, finish, gone, kept, stands_in_another_folder, trashed_file, trashed_folder};
use place::{beneath_a_root, in_another_folder, place_of, root_path, verified_path, Place};
use walk::{item_id_of, open_met, reopen_dir, walk};

/// A `move-out` row's marker, kept in its `snapshot`: the content was proved local, so what
/// follows — the attributes taken off, the item deleted in OneDrive — may run. Written before the
/// first of those, so that a replay after a crash, which finds the attributes gone (`EPERM`),
/// knows that it took them off itself. A row that carries it needs no re-marking.
pub const CONTENT_LOCAL: Snapshot = Snapshot::ContentLocal;
/// The same for a placeholder in the Trash, removed without a download: the row may delete once
/// it is gone. Such a row is still re-marked while its placeholder is there.
pub const TRASHED: Snapshot = Snapshot::Trashed;

/// How long a first `ESTALE` for a row's own object waits before a second one is believed.
const GONE_AGAIN: Duration = Duration::from_secs(5);

/// Downloads a placeholder in place, through a writable descriptor: the ordinary fill.
#[async_trait]
pub trait Filler: Send + Sync {
    /// `shown` is where the file is now, for `Transfers` and the activity log.
    async fn fill(&self, file: File, shown: &Path, clearance: Option<&Clearance>) -> Result<(), FillError>;
}

/// A fill from `source`, recorded nowhere (tests and the VM suite).
pub struct SourceFill(pub Arc<dyn ContentSource>);

#[async_trait]
impl Filler for SourceFill {
    async fn fill(&self, file: File, _shown: &Path, clearance: Option<&Clearance>) -> Result<(), FillError> {
        source::hydrate_with(file.into(), &*self.0, clearance).await
    }
}

/// The folders registered in this daemon, every account's: nothing beneath one of them is ever
/// unmarked, and an object beneath this account's own is the examination's.
pub type Roots = Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>;

/// What the worker needs for `move-out` rows. Without it they wait, as before the move-out step.
#[derive(Clone)]
pub struct MoveOuts {
    pub helper: Arc<dyn Helper>,
    pub filler: Arc<dyn Filler>,
    /// Told the item ids whose fills belong to this account wherever the objects are now (write
    /// design §4.6, §8.5): each moved-out object's, and what the base has inside a moved-out folder.
    pub route: Option<Arc<dyn Fn(HashSet<String>) + Send + Sync>>,
    /// The user's own Trash (`$XDG_DATA_HOME/Trash`). A mount's `.Trash-<uid>` and `.Trash/<uid>`
    /// are recognised at the mount's top only.
    pub home_trash: Option<PathBuf>,
    pub roots: Roots,
}

/// The worker's own record of what it protected: the rows re-marked on this helper connection,
/// and the ids last handed to [`MoveOuts::route`].
#[derive(Default)]
pub(super) struct Protection {
    marked: HashSet<i64>,
    routes: Option<HashSet<String>>,
}

impl Protection {
    /// The helper came back: its marks are gone.
    pub(super) fn helper_back(&mut self) {
        self.marked.clear();
    }
}

/// The check before a marker is written: the row is still the item's newest (a row the
/// examination recorded behind it — the object came back, and went on — supersedes it, and it
/// goes), and the object is still proved to be outside this account's folder. `Some` is what the
/// row does instead.
async fn before_marker(e: &Engine, disk: &Disk, row: &OutboxRow, id: &str, object: &File) -> Result<Option<Outcome>, Fail> {
    if let Some(outcome) = superseded(e, row, id).await? {
        return Ok(Some(outcome));
    }
    let Some(path) = verified_path(object) else { return Ok(Some(Outcome::backoff(Reason::PlaceUnknown))) };
    if root_path(disk).is_some_and(|root| path.starts_with(root)) {
        return Ok(Some(Outcome::backoff(Reason::BackInside)));
    }
    Ok(None)
}

/// A newer row of the same item (the examination's, behind this running one) supersedes it: this
/// one goes, unless it has begun to take attributes off already.
async fn superseded(e: &Engine, row: &OutboxRow, id: &str) -> Result<Option<Outcome>, Fail> {
    if marker(row) {
        return Ok(None);
    }
    let item = id.to_owned();
    let newer = e.store().call(move |s| s.outbox_for_item(&item)).await?.into_iter().any(|r| r.seq > row.seq);
    if !newer {
        return Ok(None);
    }
    tracing::info!("{} came back before its move out was done: the newer change goes instead", row.rel.display());
    let seq = row.seq;
    e.store().call(move |s| s.outbox_drop(seq, None, None, None)).await?;
    Ok(Some(Outcome::Done))
}

fn marker(row: &OutboxRow) -> bool {
    row.snapshot().is_some_and(Snapshot::is_marker)
}

/// Where the row's object was last proved to be (kept in its `target_name`): what an `ESTALE` is
/// checked against.
fn last_place(row: &OutboxRow) -> Option<&Path> {
    row.last_place()
}

/// Keeps where `object` is now, proved, as the row's last place. A name that is not UTF-8 is not
/// kept: its `ESTALE` stays unproved.
async fn remember_place(e: &Engine, row: &OutboxRow, object: &File) -> Result<(), Fail> {
    let Some(path) = verified_path(object) else { return Ok(()) };
    let Some(text) = place_name(&path).filter(|t| row.target_name.as_deref() != Some(*t)) else { return Ok(()) };
    let (seq, text) = (row.seq, text.to_owned());
    Ok(e.store().call(move |s| s.outbox_set_target(seq, None, Some(&text))).await?)
}

/// Whether a file's content is local.
enum Local {
    Yes,
    /// Not yet, and why: the row is tried again later.
    No(Outcome),
}

impl Engine {
    fn moved_out(&self) -> &MoveOuts {
        self.cfg.moved_out.as_ref().expect("move-out rows run only with MoveOuts")
    }

    /// Makes the file behind `object` (read-only, as `OpenByHandle` gives it) local where it is:
    /// marked again first, probed for a writer (one from before the mark could be writing
    /// into it), then filled through a descriptor of its own for writing, under the per-inode
    /// lock every fill takes. `Yes` only for a file that reads `hydrated` afterwards — the fill's
    /// commit point, after the whole content and its hash.
    async fn make_local(&self, object: &File, shown: &Path) -> Result<Local, Fail> {
        let mo = self.moved_out();
        if matches!(placeholder::read_state(object), Ok(Some(State::Hydrated))) {
            return Ok(Local::Yes);
        }
        if let Err(e) = mo.helper.mark_file(object).await {
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
            let object = object.try_clone()?;
            super::steps::blocking(move || {
                let fd: OwnedFd = object.try_clone()?.into();
                placeholder::with_owner_write(&object, || reopen_for_writing(&fd))
            })
            .await
        };
        let inode = self.cfg.locks.lock(InodeKey::of(object)?).await;
        let state = placeholder::read_state(object).map_err(|e| Fail::Io(io::Error::other(e.to_string())))?;
        let clearance = match state {
            Some(State::Hydrated) => return Ok(Local::Yes),
            Some(State::OnlineOnly) => None,
            // A fill that stopped part-way (a crash) left it: continued, from its checkpoint.
            Some(State::Hydrating) => match mo.helper.clearance() {
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
        // Under the lock: a section of the fill that outlives it keeps the inode locked.
        match crate::folder::locks::holding(&inode, mo.filler.fill(writable, shown, clearance.as_ref())).await {
            Ok(()) if matches!(placeholder::read_state(object), Ok(Some(State::Hydrated))) => Ok(Local::Yes),
            Ok(()) => Ok(Local::No(Outcome::backoff(Reason::NotLocal))),
            Err(e) => {
                tracing::info!("{} could not be downloaded before its item leaves OneDrive: errno {}", shown.display(), e.errno());
                Ok(Local::No(Outcome::backoff(Reason::Download(Some(format!("errno {}", e.errno()))))))
            }
        }
    }

    /// Writes a row's marker (or takes it off), before anything is taken off or removed.
    async fn set_marker(&self, row: &OutboxRow, marker: Option<Snapshot>) -> Result<(), Fail> {
        let seq = row.seq;
        Ok(self.store().call(move |s| s.outbox_set_snapshot(seq, marker)).await?)
    }

    /// Re-marks what the pending `move-out` rows name, and hands their ids to the router: before
    /// anything else runs and again whenever the worker is woken, whatever the rows' states (held,
    /// paused, offline, waiting for a folder), since a placeholder that left reads zeros while
    /// nothing marks it (Z3). Each row is marked once per helper connection
    /// ([`Protection::helper_back`]); an answer that may change (`EAGAIN`, a helper that does not
    /// answer) leaves it for the next look.
    pub(super) async fn protect(&self, disk: &Disk) {
        let Some(mo) = self.cfg.moved_out.as_ref() else { return };
        let Ok(rows) = self.store().call(|s| s.outbox_move_outs()).await else { return };
        let mut ids = HashSet::new();
        for row in &rows {
            let Some(id) = &row.item_id else { continue };
            ids.insert(id.clone());
            let folder = id.clone();
            if let Ok(inside) = self.store().call(move |s| s.descendants(Table::Items, &folder)).await {
                ids.extend(inside);
            }
        }
        if let Some(route) = &mo.route {
            let mut protection = self.protection();
            if protection.routes.as_ref() != Some(&ids) {
                protection.routes = Some(ids.clone());
                drop(protection);
                route(ids);
            }
        }
        let Ok(root) = disk.dir(Path::new("")) else { return };
        for row in rows {
            if self.protection().marked.contains(&row.seq) || row.snapshot_is(CONTENT_LOCAL) {
                continue;
            }
            let Some(handle) = row.inode.as_ref().and_then(|i| i.handle.clone()) else { continue };
            let object = match mo.helper.open_by_handle(&root, &handle).await {
                Ok(object) => File::from(object),
                // Gone, not the user's to have, or no handle: nothing to mark. The row decides.
                Err(HelperError::Refused(libc::ESTALE | libc::EPERM | libc::EINVAL)) => {
                    self.protection().marked.insert(row.seq);
                    continue;
                }
                // Leased, or anything else: asked again at the next look.
                Err(HelperError::Refused(errno)) => {
                    tracing::debug!("{} is not marked again yet: errno {errno}", row.rel.display());
                    continue;
                }
                Err(e) => {
                    tracing::debug!("moved-out objects are not marked again yet: {e}");
                    return;
                }
            };
            if let Err(e) = remember_place(self, &row, &object).await {
                tracing::debug!("where {} is now is not kept: {e:?}", row.rel.display());
            }
            let marked = if object.metadata().is_ok_and(|m| m.is_dir()) {
                self.mark_tree(&object).await
            } else if matches!(placeholder::read_state(&object), Ok(Some(State::Hydrated))) {
                Ok(())
            } else {
                mo.helper.mark_file(&object).await
            };
            match marked {
                Ok(()) => {
                    self.protection().marked.insert(row.seq);
                }
                // Asked again at every look, so quietly.
                Err(e) => tracing::debug!("{} is not marked again yet: {e}", row.rel.display()),
            }
        }
    }

    /// `MarkDir` for a moved-out directory and every directory below it, whatever it holds: the
    /// marks it took along are gone once the helper restarts. The first failure is the answer,
    /// and the row is marked again at the next look.
    async fn mark_tree(&self, object: &File) -> Result<(), HelperError> {
        let mo = self.moved_out();
        mo.helper.mark_dir(object).await?;
        let unreadable = |what: &str| HelperError::Io(format!("a moved-out directory cannot be {what}"));
        let top = verified_path(object).and_then(|path| reopen_dir(&path, object).ok().flatten()).ok_or_else(|| unreadable("found by its path"))?;
        let top2 = top.try_clone().map_err(|e| HelperError::Io(e.to_string()))?;
        let met = super::steps::blocking(move || walk(&top2)).await.map_err(|_| unreadable("walked"))?;
        for m in met.iter().filter(|m| m.is_dir) {
            let dir = open_met(&top, m).map_err(|e| HelperError::Io(e.to_string()))?;
            mo.helper.mark_dir(&dir).await?;
        }
        Ok(())
    }

    async fn unmark(&self, disk: &Disk, dir: &File) {
        unmark(self.moved_out(), disk, dir).await;
    }
}

/// `UnmarkDir`, but never for a directory beneath a registered folder, wherever it went
/// meanwhile. A mark left behind costs a round trip per open, which the helper lets through (the
/// files are ordinary now); it goes with the helper's next start.
async fn unmark(mo: &MoveOuts, disk: &Disk, dir: &File) {
    match verified_path(dir) {
        Some(path) if !beneath_a_root(mo, disk, &path) => {
            if let Err(err) = mo.helper.unmark_dir(dir).await {
                tracing::debug!("a moved-out directory keeps its mark: {err}");
            }
        }
        _ => tracing::info!("a directory that left the folder is in a folder again, or cannot be placed: it stays marked"),
    }
}

/// A `move-out` row's step.
pub(super) async fn run(e: &Arc<Engine>, disk: &Disk, row: OutboxRow) -> Result<Outcome, Fail> {
    let Some(mo) = e.cfg.moved_out.as_ref() else {
        return Ok(Outcome::later(Reason::MoveOut, Duration::from_secs(3600)));
    };
    let (Some(id), Some(handle)) = (row.item_id.clone(), row.inode.as_ref().and_then(|i| i.handle.clone())) else {
        return Ok(Outcome::blocked(Reason::NoHandle));
    };
    let root = disk.dir(Path::new(""))?;
    let object = match mo.helper.open_by_handle(&root, &handle).await {
        Ok(object) => File::from(object),
        // Every decode failure is `ESTALE`: believed only for handles taken on the filesystem the
        // folder is on now.
        Err(HelperError::Refused(libc::ESTALE)) if !handles_current_async(e.store(), &root).await => {
            return Ok(Outcome::backoff(Reason::StaleHandle));
        }
        // What this row removed or stripped itself.
        Err(HelperError::Refused(libc::ESTALE)) if marker(&row) => return finish(e, &row).await,
        // Gone: the user deleted it, wherever it was (§5) — said twice, some seconds apart...
        Err(HelperError::Refused(libc::ESTALE)) if row.reason != Some(Reason::GoneOnce) => {
            return Ok(Outcome::later(Reason::GoneOnce, GONE_AGAIN));
        }
        // ...and with its evidence: nothing, or another object, where it was last proved to be. An
        // inode that cannot be read says `ESTALE` every time, and stands there.
        Err(HelperError::Refused(libc::ESTALE)) if !last_place(&row).is_some_and(|p| absent_at(p, &handle)) => {
            return Ok(Outcome::backoff(Reason::GoneUnproved));
        }
        // Last proved inside another account's folder: that account may have taken it for none
        // of its own. Nothing is deleted in OneDrive.
        Err(HelperError::Refused(libc::ESTALE)) if last_place(&row).is_some_and(|p| in_another_folder(mo, disk, p)) => {
            return kept(e, &row, &id).await;
        }
        Err(HelperError::Refused(libc::ESTALE)) => return gone(e, disk, &row, &id).await,
        // The attributes this very row took off: its content was proved local first.
        Err(HelperError::Refused(libc::EPERM)) if marker(&row) => return finish(e, &row).await,
        // The same object, where it was last proved to be inside another account's folder, with
        // no item id: that account's examination took it for its own, and uploads it (review
        // m5). Still not "gone": nothing is deleted in OneDrive, and the item comes back here.
        Err(HelperError::Refused(libc::EPERM)) if stands_in_another_folder(mo, disk, &row, &handle) => {
            return kept(e, &row, &id).await;
        }
        // Never "gone" (F90): kept, and asked again now and then.
        Err(HelperError::Refused(libc::EPERM)) => return Ok(Outcome::backoff(Reason::Unreachable(None))),
        Err(HelperError::Refused(libc::EAGAIN)) => return Ok(Outcome::later(Reason::NotLocal, RECHECK)),
        Err(HelperError::Refused(libc::EINVAL)) => return Ok(Outcome::blocked(Reason::BadHandle)),
        Err(HelperError::Refused(errno)) => return Ok(Outcome::backoff(Reason::Unreachable(Some(format!("errno {errno}"))))),
        Err(other) => {
            tracing::debug!("{}: {other}", row.rel.display());
            return Ok(Outcome::backoff(Reason::NoHelper));
        }
    };
    if item_id_of(&object).as_deref() != Some(id.as_str()) {
        // The helper hands over only an object carrying an item id: another one's is no answer.
        return Ok(Outcome::blocked(Reason::AnotherItem));
    }
    remember_place(e, &row, &object).await?;
    let is_dir = object.metadata()?.is_dir();
    match place_of(e, disk, &object, &handle) {
        // A marker stays: what it took off may be taken off already.
        Place::Inside => {
            if let Some(outcome) = superseded(e, &row, &id).await? {
                return Ok(outcome);
            }
            Ok(Outcome::backoff(Reason::BackInside))
        }
        Place::Unknown => Ok(Outcome::backoff(Reason::PlaceUnknown)),
        // A hard-linked placeholder is not only in the Trash: it is downloaded, as anywhere else.
        Place::Trash(entry) if is_dir || object.metadata()?.nlink() == 1 => {
            if is_dir {
                trashed_folder(e, disk, &row, &id, object, &entry).await
            } else {
                trashed_file(e, disk, &row, &id, object, &entry).await
            }
        }
        Place::Trash(_) | Place::Elsewhere(_) => {
            let shown = verified_path(&object).unwrap_or_else(|| e.cfg.root.path.join(&row.rel));
            if is_dir {
                elsewhere_folder(e, disk, &row, &id, object, &shown).await
            } else {
                elsewhere_file(e, disk, &row, &id, object, &shown).await
            }
        }
    }
}

/// `f` on a blocking thread.
async fn off<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(io::Error::other)?
}
