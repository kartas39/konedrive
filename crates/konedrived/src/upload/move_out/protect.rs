//! The re-marking of what left the folder, and the routing of its fills (`docs/design/writes.md` §8.3,
//! §10): the worker's own task does it, before any row runs.

use std::collections::HashSet;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use super::place::verified_path;
use super::reach::{reach, Reach};
use super::row::{hydrated, remember_place};
use super::tidy::inside_of;
use super::walk::{open_met, reopen_dir, walk};
use super::{MoveOuts, CONTENT_LOCAL};
use crate::folder::disk::Disk;
use crate::helper::HelperError;
use crate::upload::engine::Engine;
use crate::upload::steps::{blocking, off};

/// The worker's own record of what it protected: the rows re-marked on this helper connection,
/// and the ids last handed to [`MoveOuts::route`].
#[derive(Default)]
pub(in crate::upload) struct Protection {
    marked: HashSet<i64>,
    routes: Option<HashSet<String>>,
}

impl Protection {
    /// The helper came back: its marks are gone.
    pub(in crate::upload) fn helper_back(&mut self) {
        self.marked.clear();
    }
}

impl Engine {
    /// Re-marks what the pending `move-out` rows name, and hands their ids to the router: before
    /// anything else runs and again whenever the worker is woken, whatever the rows' states (held,
    /// paused, offline, waiting for a folder), since a placeholder that left reads zeros while
    /// nothing marks it (Z3). Each row is marked once per helper connection
    /// ([`Protection::helper_back`]); an answer that may change (a lease, a handle of another
    /// filesystem, a helper that does not answer) leaves it for the next look.
    pub(in crate::upload) async fn protect(&self, disk: &Arc<Disk>) {
        let Some(mo) = self.move_outs() else { return };
        let rows = match self.store().call(|s| s.outbox_move_outs()).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!("what left the folder is not marked again yet: its rows cannot be read: {e}");
                return;
            }
        };
        let mut ids = HashSet::new();
        for row in &rows {
            let Some(id) = &row.item_id else { continue };
            match inside_of(self.store(), id).await {
                Ok(inside) => ids.extend(inside),
                Err(e) => {
                    // The object's own fills are routed; those of what it holds are not yet.
                    tracing::debug!("what the base has inside {} cannot be read: {e}", row.rel.display());
                    ids.insert(id.clone());
                }
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
        let root = match disk.dir(Path::new("")) {
            Ok(root) => root,
            Err(e) => {
                tracing::debug!("what left the folder is not marked again yet: the folder cannot be opened: {e}");
                return;
            }
        };
        for row in rows {
            if self.protection().marked.contains(&row.seq) || row.snapshot_is(CONTENT_LOCAL) {
                continue;
            }
            let Some(handle) = row.inode.as_ref().and_then(|i| i.handle.clone()) else { continue };
            let object = match reach(&*mo.helper, self.store(), &root, &handle).await {
                Reach::Open(object) => object,
                // Gone, not the user's to have, or no handle: nothing to mark. The row decides.
                Reach::Gone | Reach::Refused | Reach::BadHandle => {
                    self.protection().marked.insert(row.seq);
                    continue;
                }
                // Asked again at the next look: the examination takes the handles again.
                Reach::Stale => {
                    tracing::debug!("{} is not marked again yet: its handle is another filesystem's", row.rel.display());
                    continue;
                }
                Reach::Busy => {
                    tracing::debug!("{} is not marked again yet: it is leased", row.rel.display());
                    continue;
                }
                Reach::Errno(errno) => {
                    tracing::debug!("{} is not marked again yet: errno {errno}", row.rel.display());
                    continue;
                }
                Reach::NoHelper(e) => {
                    tracing::debug!("moved-out objects are not marked again yet: {e}");
                    return;
                }
            };
            if let Err(e) = remember_place(self.store(), &row, &object).await {
                tracing::debug!("where {} is now is not kept: {e:?}", row.rel.display());
            }
            let marked = if object.metadata().is_ok_and(|m| m.is_dir()) {
                mark_tree(mo, &object).await
            } else if hydrated(&object).await.unwrap_or(false) {
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
}

/// `MarkDir` for a moved-out directory and every directory below it, whatever it holds: the
/// marks it took along are gone once the helper restarts. The first failure is the answer,
/// and the row is marked again at the next look.
async fn mark_tree(mo: &MoveOuts, object: &Arc<File>) -> Result<(), HelperError> {
    mo.helper.mark_dir(object).await?;
    let unreadable = |what: &str| HelperError::Io(format!("a moved-out directory cannot be {what}"));
    let at = Arc::clone(object);
    let top = off(move || Ok(verified_path(&at).and_then(|path| reopen_dir(&path, &at).ok().flatten()))).await.ok().flatten();
    let top = Arc::new(top.ok_or_else(|| unreadable("found by its path"))?);
    let below = Arc::clone(&top);
    let met = blocking(move || walk(&below)).await.map_err(|_| unreadable("walked"))?;
    for m in met.into_iter().filter(|m| m.is_dir) {
        let below = Arc::clone(&top);
        let dir = off(move || open_met(&below, &m)).await.map_err(|e| HelperError::Io(e.to_string()))?;
        mo.helper.mark_dir(&dir).await?;
    }
    Ok(())
}
