//! The cases of a `move-out` row's step ([`MoveOut::step`](super::row::MoveOut::step) says
//! which): what happens to the object where it is now, and whether its item leaves OneDrive.

use konedrive_tree::ActivityKind;
use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_fs::placeholder::{self, State};
use konedrive_tree::outbox::Reason;
use konedrive_tree::{Kind, Placement, Table};

use super::place::Place;
use super::reach::Reach;
use super::row::{absent, marker, proved_path, Local, MoveOut};
use super::tidy::{fate, inside_of, remove, remove_all, strip_all, tidy_dirs, Emptied, Fate, Walked};
use super::trash::TrashEntry;
use super::walk::{open_met, Met};
use super::{CONTENT_LOCAL, TRASHED};
use crate::folder::locks::{InodeGuard, InodeKey};
use crate::local::RECHECK;
use crate::upload::engine::{Fail, Outcome};
use crate::upload::steps::{blocking, blocking_under};
use crate::upload::Fault;

/// What left a moved-out folder since ([`MoveOut::left_since`]).
enum Left {
    /// Each is local where it went, or gone: these are stripped with the folder's own.
    Local(Vec<Arc<File>>),
    /// One of them keeps the folder in OneDrive for now.
    Wait(Outcome),
}

impl MoveOut<'_> {
    /// A file moved anywhere but the Trash: downloaded, stripped, then its item deleted (WR5).
    pub(super) async fn elsewhere_file(&self, object: Arc<File>) -> Result<Outcome, Fail> {
        let shown = proved_path(&object).await?.unwrap_or_else(|| self.e.root().path.join(&self.row.rel));
        if let Local::No(outcome) = self.make_local(&object, &shown).await? {
            return Ok(outcome);
        }
        if let Some(outcome) = self.before_marker(&object).await? {
            return Ok(outcome);
        }
        self.set_marker(Some(CONTENT_LOCAL)).await?;
        blocking(move || placeholder::strip(&object)).await?;
        self.e.fault(Fault::AfterStrip)?;
        tracing::info!("{} left the folder: downloaded to {}, and removed from OneDrive", self.row.rel.display(), shown.display());
        self.finish().await
    }

    /// A folder moved anywhere but the Trash: every placeholder of its item downloaded where it
    /// is, the attributes taken off and every directory unmarked, then the folder deleted in
    /// OneDrive — as a folder delete is: one unguarded `DELETE` of the folder itself, whatever it
    /// holds there by then (F82 (10)).
    pub(super) async fn elsewhere_folder(&self, object: Arc<File>) -> Result<Outcome, Fail> {
        if let Err(err) = self.mo.helper.mark_dir(&object).await {
            tracing::debug!("{} is not marked again yet: {err}", self.row.rel.display());
        }
        let Some(walked) = self.walked(&object).await? else { return Ok(Outcome::backoff(Reason::PlaceUnknown)) };
        let mut ours: Vec<Met> = Vec::new();
        // Directories holding another item's placeholder (with a row of its own) stay marked.
        let mut keep_marked: HashSet<PathBuf> = HashSet::new();
        let mut found: HashSet<String> = HashSet::new();
        for m in walked.met.iter().filter(|m| !m.is_dir) {
            let Some(item) = &m.id else { continue };
            if walked.inside.contains(item) {
                let file = reopened(&walked.top, m).await?;
                if let Local::No(outcome) = self.make_local(&file, &walked.path.join(&m.rel)).await? {
                    return Ok(outcome);
                }
                found.insert(item.clone());
                ours.push(m.clone());
            } else {
                let (below, met) = (Arc::clone(&walked.top), m.clone());
                let whole = blocking(move || Ok(matches!(placeholder::read_state(&open_met(&below, &met)?), Ok(Some(State::Hydrated))))).await?;
                if !whole {
                    keep_marked.insert(m.dir().to_path_buf());
                }
            }
        }
        let extra = match self.left_since(&walked.inside, &found, Some(&walked.path)).await? {
            Left::Local(extra) => extra,
            Left::Wait(outcome) => return Ok(outcome),
        };
        if let Some(outcome) = self.before_marker(&object).await? {
            return Ok(outcome);
        }
        self.set_marker(Some(CONTENT_LOCAL)).await?;
        for (n, m) in ours.into_iter().enumerate() {
            let top = Arc::clone(&walked.top);
            blocking(move || placeholder::strip(&open_met(&top, &m)?)).await?;
            if n == 0 {
                self.e.fault(Fault::MidStrip)?;
            }
        }
        strip_each(extra).await?;
        tidy_dirs(self.mo, self.disk, &walked, &keep_marked, Emptied::Stays).await?;
        self.e.fault(Fault::AfterStrip)?;
        tracing::info!("{} left the folder: downloaded to {}, and removed from OneDrive", self.row.rel.display(), walked.path.display());
        self.finish().await
    }

    /// A file moved to the Trash: nothing is downloaded (Windows does the same). Downloaded
    /// content stays there as the user's own file; a placeholder, which holds nothing whole, is
    /// removed with its `.trashinfo`, and only once it has no link left does the item go to
    /// OneDrive's recycle bin.
    pub(super) async fn trashed_file(&self, object: Arc<File>, entry: &TrashEntry) -> Result<Outcome, Fail> {
        let Some(path) = proved_path(&object).await? else { return Ok(Outcome::backoff(Reason::PlaceUnknown)) };
        // Held until the file is stripped or gone: no fill or free-up starts meanwhile.
        let Some(inode) = self.e.locks().try_lock(InodeKey::of(&object)?) else { return Ok(Outcome::later(Reason::NotLocal, RECHECK)) };
        if let Some(outcome) = self.before_marker(&object).await? {
            return Ok(outcome);
        }
        match fate_of(&object).await? {
            Fate::Stays => {
                self.set_marker(Some(CONTENT_LOCAL)).await?;
                let stripped = Arc::clone(&object);
                blocking_under(inode.hold(), move || placeholder::strip(&stripped)).await?;
            }
            // The cloud keeps it, in its recycle bin.
            Fate::Goes => {
                self.set_marker(Some(TRASHED)).await?;
                let (entry, removed) = (entry.clone(), Arc::clone(&object));
                blocking_under(inode.hold(), move || remove(&removed, &path, Some(&entry))).await?;
                // Proved gone: no link left. Renamed meanwhile, or linked elsewhere, it is found
                // where it is at the next run.
                if object.metadata()?.nlink() != 0 {
                    self.set_marker(None).await?;
                    return Ok(Outcome::backoff(Reason::PlaceUnknown));
                }
            }
            Fate::Unsure => return Ok(Outcome::later(Reason::NotLocal, RECHECK)),
        }
        self.e.fault(Fault::AfterStrip)?;
        tracing::info!("{} was moved to the Trash: it is in OneDrive's recycle bin", self.row.rel.display());
        self.finish().await
    }

    /// A folder moved to the Trash: nothing is downloaded into it. Its downloaded files stay, as
    /// the user's own; its placeholders go (each proved gone), and so do its directories left
    /// empty, and the whole entry with its `.trashinfo` when nothing is left. A placeholder with
    /// another link is downloaded instead.
    pub(super) async fn trashed_folder(&self, object: Arc<File>, entry: &TrashEntry) -> Result<Outcome, Fail> {
        let Some(walked) = self.walked(&object).await? else { return Ok(Outcome::backoff(Reason::PlaceUnknown)) };
        let mut found: HashSet<String> = HashSet::new();
        // Held until the placeholders are gone: no fill starts meanwhile.
        let mut guards: Vec<InodeGuard> = Vec::new();
        // The item's files: what stays (downloaded) and what goes (a placeholder).
        let (mut stay, mut go) = (Vec::new(), Vec::new());
        for m in walked.files() {
            let file = reopened(&walked.top, m).await?;
            if file.metadata()?.nlink() > 1 {
                if let Local::No(outcome) = self.make_local(&file, &walked.path.join(&m.rel)).await? {
                    return Ok(outcome);
                }
            }
            let Some(guard) = self.e.locks().try_lock(InodeKey::of(&file)?) else { return Ok(Outcome::later(Reason::NotLocal, RECHECK)) };
            match fate_of(&file).await? {
                Fate::Stays => stay.push(m.clone()),
                Fate::Goes => go.push(m.clone()),
                Fate::Unsure => return Ok(Outcome::later(Reason::NotLocal, RECHECK)),
            }
            guards.push(guard);
            found.extend(m.id.clone());
        }
        let extra = match self.left_since(&walked.inside, &found, Some(&walked.path)).await? {
            Left::Local(extra) => extra,
            Left::Wait(outcome) => return Ok(outcome),
        };
        if let Some(outcome) = self.before_marker(&object).await? {
            return Ok(outcome);
        }
        self.set_marker(Some(TRASHED)).await?;
        // The placeholders go first, each proved gone; nothing is stripped until they all are, so
        // that the marker can be taken off again with nothing stripped.
        let removed_all = {
            let top = Arc::clone(&walked.top);
            let holds: Vec<_> = guards.iter().map(InodeGuard::hold).collect();
            blocking_under(holds, move || remove_all(&top, &go)).await?
        };
        drop(guards);
        if !removed_all {
            // A placeholder renamed or linked meanwhile: nothing goes until it is found again.
            self.set_marker(None).await?;
            return Ok(Outcome::backoff(Reason::PlaceUnknown));
        }
        strip_each(extra).await?;
        let top = Arc::clone(&walked.top);
        blocking(move || strip_all(&top, &stay)).await?;
        tidy_dirs(self.mo, self.disk, &walked, &HashSet::new(), Emptied::Goes { entry: Some(entry) }).await?;
        self.e.fault(Fault::AfterStrip)?;
        tracing::info!("{} was moved to the Trash: it is in OneDrive's recycle bin", self.row.rel.display());
        self.finish().await
    }

    /// The moved-out folder behind `object`, walked where it is now, with what the base has
    /// inside the row's item. `None` where its place is not proved.
    async fn walked(&self, object: &Arc<File>) -> Result<Option<Walked>, Fail> {
        let inside = inside_of(self.e.store(), self.id).await?;
        let by = Arc::clone(object);
        blocking(move || Walked::of(&by, inside)).await
    }

    /// What the base still has inside the row's folder that was placed here and is not among
    /// what the walk found (`found`): each is asked after by its own handle. Gone is fine
    /// (deleted by the user) when the handles are this filesystem's; so is a refusal once the
    /// row is marked (it took that file's attributes off itself); a file alive outside the
    /// folder left the moved-out folder since, and is made local where it is, like the folder's
    /// own (returned, to be stripped with them); anything else — alive in the folder,
    /// unreachable, unanswered, with no handle — keeps the folder in OneDrive for now.
    async fn left_since(&self, inside: &HashSet<String>, found: &HashSet<String>, top: Option<&std::path::Path>) -> Result<Left, Fail> {
        let store = self.e.store();
        let asked = self.id.to_owned();
        let folder = store.call(move |s| s.locate(Table::Items, &asked)).await?.map(|l| l.rel);
        let mut extra = Vec::new();
        for item in inside.iter().filter(|i| i.as_str() != self.id && !found.contains(*i)) {
            let asked = item.clone();
            let Some(base) = store.call(move |s| s.get(Table::Items, &asked)).await? else { continue };
            if base.kind != Kind::File || base.placement != Placement::Placed {
                continue;
            }
            let asked = item.clone();
            let Some(handle) = store.call(move |s| s.local_handle(&asked)).await? else {
                return Ok(Left::Wait(Outcome::backoff(Reason::Unreachable(None))));
            };
            let object = match self.reach(&handle).await {
                Reach::Open(object) => object,
                Reach::Stale => return Ok(Left::Wait(Outcome::backoff(Reason::StaleHandle))),
                // Gone with its evidence: nothing, or another object, at its place in the folder
                // where the folder is now.
                Reach::Gone => {
                    let asked = item.clone();
                    let at = store.call(move |s| s.locate(Table::Items, &asked)).await?.map(|l| l.rel);
                    let there = match (top, folder.as_deref(), at.as_deref()) {
                        (Some(top), Some(folder), Some(at)) => at.strip_prefix(folder).ok().map(|inside| top.join(inside)),
                        _ => None,
                    };
                    if absent(there.as_deref(), &handle).await? {
                        continue;
                    }
                    return Ok(Left::Wait(Outcome::backoff(Reason::GoneUnproved)));
                }
                Reach::Refused if marker(self.row) => continue,
                Reach::Refused | Reach::Busy | Reach::BadHandle | Reach::Errno(_) => return Ok(Left::Wait(Outcome::backoff(Reason::Unreachable(None)))),
                Reach::NoHelper(_) => return Ok(Left::Wait(Outcome::backoff(Reason::NoHelper))),
            };
            let shown = match self.place(&object, &handle).await? {
                Place::Elsewhere(Some(path)) => path,
                Place::Trash(entry) => entry.top,
                Place::Elsewhere(None) => return Ok(Left::Wait(Outcome::backoff(Reason::PlaceUnknown))),
                Place::Inside | Place::Unknown => return Ok(Left::Wait(Outcome::backoff(Reason::BackInside))),
            };
            if let Local::No(outcome) = self.make_local(&object, &shown).await? {
                return Ok(Left::Wait(outcome));
            }
            extra.push(object);
        }
        Ok(Left::Local(extra))
    }

    /// The item leaves OneDrive, as a delete does (§4.7): `If-Match`, `404` done — a folder,
    /// unguarded, whole, whatever changed inside it since.
    pub(super) async fn finish(&self) -> Result<Outcome, Fail> {
        crate::upload::steps::delete(self.e, self.row.clone()).await
    }

    /// The object is gone (`ESTALE`, twice, on this filesystem's handles, with nothing where it
    /// was): the user deleted it after it left (§5), and its item is deleted as any delete is —
    /// a folder only once what left it since is local where it went, or gone too.
    pub(super) async fn gone(&self) -> Result<Outcome, Fail> {
        let asked = self.id.to_owned();
        let folder = self.e.store().call(move |s| s.get(Table::Items, &asked)).await?.is_some_and(|item| item.kind == Kind::Folder);
        if folder {
            let inside = inside_of(self.e.store(), self.id).await?;
            let extra = match self.left_since(&inside, &HashSet::new(), self.row.last_place()).await? {
                Left::Local(extra) => extra,
                Left::Wait(outcome) => return Ok(outcome),
            };
            if let Some(outcome) = self.superseded().await? {
                return Ok(outcome);
            }
            self.set_marker(Some(CONTENT_LOCAL)).await?;
            strip_each(extra).await?;
        }
        self.finish().await
    }

    /// The object is gone, and was last proved to be inside another account's folder, or stands
    /// there taken for that account's own: that account may have removed it as none of its own,
    /// or the user deleted it there. Nothing is deleted in OneDrive: the row goes, the item and
    /// what is inside it forget their local objects, and the reconcile places them again in
    /// this folder (F124).
    pub(super) async fn kept(&self) -> Result<Outcome, Fail> {
        let e = self.e;
        let event = e.event(ActivityKind::Restored, &self.row.rel, "it was last in another account's folder, and stays in OneDrive");
        {
            let _tree = e.tree_lock().lock().await;
            let (seq, id, stored) = (self.row.seq, self.id.to_owned(), event.clone());
            e.store().call(move |s| s.outbox_drop(seq, Some(&id), Some(&stored))).await?;
        }
        tracing::info!("{} went from another account's folder: it stays in OneDrive, and comes back here", self.row.rel.display());
        e.host().activity(&event);
        e.host().full_cycle_wanted();
        Ok(Outcome::Done)
    }
}

/// What the walk met at `m` below `top`, opened again off the runtime ([`open_met`]).
async fn reopened(top: &Arc<File>, m: &Met) -> Result<Arc<File>, Fail> {
    let (top, m) = (Arc::clone(top), m.clone());
    Ok(Arc::new(blocking(move || open_met(&top, &m)).await?))
}

/// [`fate`], off the runtime.
async fn fate_of(file: &Arc<File>) -> Result<Fate, Fail> {
    let file = Arc::clone(file);
    blocking(move || Ok(fate(&file))).await
}

/// Takes konedrive's attributes off each of `files`, which are local where they are.
async fn strip_each(files: Vec<Arc<File>>) -> Result<(), Fail> {
    blocking(move || {
        for file in &files {
            placeholder::strip(file)?;
        }
        Ok(())
    })
    .await
}
