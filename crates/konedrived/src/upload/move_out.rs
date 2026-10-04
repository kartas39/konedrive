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
//! [`handles_current`]: crate::local::handles::current_async
//!
//! **Where it is written.** [`run`] makes the row's [`MoveOut`](row::MoveOut), whose step
//! (`row`) asks the helper (`reach`), finds where the object is (`place`, `trash`) and takes
//! the case that follows (`cases`). What the Trash case and the tidying after dropped rows
//! (`dropped`) do to the object is one rule, in `tidy`. The worker's own re-marking is in
//! `protect`.

mod cases;
mod dropped;
mod place;
mod protect;
mod reach;
mod row;
mod tidy;
mod trash;
mod walk;

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use konedrive_tree::outbox::{OutboxRow, Reason, Snapshot};

use super::engine::{Engine, Fail, Outcome};
use super::steps::off;
use crate::folder::disk::Disk;
use crate::helper::linked::Helper;
use crate::helper::Clearance;
use crate::hydration::source::{self, ContentSource, FillError};

pub(crate) use dropped::{drop_rows, Tidy};
pub(super) use protect::Protection;
pub use trash::{home_trash, trash_of, TrashEntry};

use place::{beneath_a_root, verified_path};

/// A `move-out` row's marker, kept in its `snapshot`: the content was proved local, so what
/// follows — the attributes taken off, the item deleted in OneDrive — may run. Written before the
/// first of those, so that a replay after a crash, which finds the attributes gone (`EPERM`),
/// knows that it took them off itself. A row that carries it needs no re-marking.
pub const CONTENT_LOCAL: Snapshot = Snapshot::ContentLocal;
/// The same for a placeholder in the Trash, removed without a download: the row may delete once
/// it is gone. Such a row is still re-marked while its placeholder is there.
pub const TRASHED: Snapshot = Snapshot::Trashed;

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

/// `UnmarkDir`, but never for a directory beneath a registered folder, wherever it went
/// meanwhile. A mark left behind costs a round trip per open, which the helper lets through (the
/// files are ordinary now); it goes with the helper's next start.
async fn unmark(mo: &MoveOuts, disk: &Arc<Disk>, dir: &Arc<File>) {
    let (registered, on, at) = (mo.clone(), Arc::clone(disk), Arc::clone(dir));
    let outside = off(move || Ok(verified_path(&at).is_some_and(|path| !beneath_a_root(&registered, &on, &path)))).await;
    match outside {
        Ok(true) => {
            if let Err(err) = mo.helper.unmark_dir(dir).await {
                tracing::debug!("a moved-out directory keeps its mark: {err}");
            }
        }
        Ok(false) => tracing::info!("a directory that left the folder is in a folder again, or cannot be placed: it stays marked"),
        Err(err) => tracing::debug!("a moved-out directory keeps its mark: where it is cannot be read: {err}"),
    }
}

/// A `move-out` row's step. Without what move-outs need ([`MoveOuts`]) the row waits; one
/// that names no item, or no object, needs the user.
pub(super) async fn run(e: &Arc<Engine>, disk: &Arc<Disk>, row: OutboxRow) -> Result<Outcome, Fail> {
    let Some(mo) = e.move_outs() else {
        return Ok(Outcome::later(Reason::MoveOut, Duration::from_secs(3600)));
    };
    let (Some(id), Some(handle)) = (row.item_id.as_deref(), row.inode.as_ref().and_then(|i| i.handle.as_ref())) else {
        return Ok(Outcome::blocked(Reason::NoHandle));
    };
    let root = disk.dir(Path::new(""))?;
    row::MoveOut { e, mo, disk, row: &row, id, handle, root: &root }.step().await
}
