use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder;
use konedrive_tree::outbox::Inode;

use crate::sync::SyncService;
use crate::status::activity::Kind;
use crate::helper::Clearance;
use crate::folder::locks::InodeKey;
use crate::sync::pins::{PinTarget, kept_by_folder};
use crate::sync::SyncError;
use crate::sync::hydrate::open_shown;
use crate::hydration::pin;
use crate::status::activity;

impl SyncService {
    /// Frees a hydrated file's space back to a placeholder.
    ///
    /// The file is opened here, through the same `SyncRoot::open_inside`
    /// gate, so that the per-inode lock can be taken on the inode that is
    /// about to be emptied — `(st_dev, st_ino)` from that very descriptor,
    /// never a name — and so that the descriptor the lock was
    /// taken on is the one `root::dehydrate_opened` marks, clears and
    /// punches (one open per dehydration).
    ///
    /// Recorded as a `freed` event with what it freed.
    ///
    /// Refused `NotAllowed` for a file a pin keeps on this device — its own,
    /// or a folder's above it: `FreeUp` takes a pin off.
    pub async fn dehydrate(&self, path: &Path) -> Result<(), SyncError> {
        let reg = self.require_record()?;
        let (root, target) = (reg.root.clone(), path.to_path_buf());
        let pinned = tokio::task::spawn_blocking(move || {
            let full = root.path.join(root.relative(&target).ok()?);
            pin::pinned_by(&root.path, &full).map(|by| (full, by))
        })
        .await
        .map_err(|e| SyncError::Io(format!("the dehydration task failed: {e}")))?;
        if let Some((full, by)) = pinned {
            return Err(SyncError::NotAllowed(pin::refusal(&full, &by)));
        }
        let (freed, shown) = self.free_one(path, Wait::Yes).await?;
        let event = activity::event(Kind::Freed, shown, activity::human_size(freed));
        self.report.activity.record(vec![event]).await;
        self.report.space.kick();
        Ok(())
    }

    /// `FreeUpSpace()`: every downloaded file under the folder
    /// freed up through the same per-file path `Dehydrate` takes — the
    /// helper's `ClearIgnore`, the write lease, the per-inode lock — so every
    /// rule that holds for one file holds here.
    ///
    /// A file that is open (its lease is refused) or that a fill or another
    /// free-up is busy with (its per-inode lock is taken) is left as it is
    /// and counted as busy, never waited for: a download can take any time.
    /// A file that is not a clean download — changed here, or not ours — is
    /// left alone as `Dehydrate` would refuse it, and counted in neither.
    /// The walk only reads names and attributes (`lstat`, `lgetxattr`); only
    /// a file that reads `hydrated` is opened, to be freed.
    ///
    /// Refused as `Dehydrate` is where no file could be freed (no root; an
    /// intercepted root with no helper), and stopped with that refusal if it
    /// becomes true part-way. What was freed is one `freed` event for the
    /// folder, not one per file.
    ///
    /// A file a pin keeps on this device is left as it is, and counted in
    /// [`FreedUp::pinned`]; the event says how many.
    pub async fn free_up_space(&self) -> Result<FreedUp, SyncError> {
        let reg = self.require_record()?;
        if reg.intercepted() {
            self.require_link()?;
        }
        let root = reg.root.path.clone();
        let (candidates, pinned) = tokio::task::spawn_blocking(move || pin::downloaded_under(&root, false))
            .await
            .map_err(|e| SyncError::Io(format!("the walk of the folder failed: {e}")))?;
        let (mut freed, stopped) = self.free_each(candidates).await;
        freed.pinned = pinned;
        if freed.files > 0 {
            let mut detail = freed_detail(freed.files, freed.bytes);
            if pinned > 0 {
                detail.push_str(&format!("; {pinned} kept on this device"));
            }
            let event = activity::event(Kind::Freed, reg.root.path.display().to_string(), detail);
            self.report.activity.record(vec![event]).await;
        }
        if pinned > 0 {
            tracing::info!("free up space kept {pinned} file(s) that are always kept on this device");
        }
        self.report.space.kick();
        match stopped {
            // Forgotten part-way: what was freed
            // is freed, and is what the caller hears — not a refusal that
            // drops the counts.
            Some(SyncError::NoRoot) => Ok(freed),
            Some(e) => Err(e),
            None => Ok(freed),
        }
    }

    /// Frees each of `files` up without waiting for any ([`free_one`](Self::free_one)
    /// with `Wait::No`): what was freed, and the refusal — no helper, no
    /// root — that stopped it part-way, if one did. A file in use is counted
    /// busy, one changed here [`FreedUp::modified`]; either is left as it is.
    async fn free_each(&self, files: Vec<PathBuf>) -> (FreedUp, Option<SyncError>) {
        let mut freed = FreedUp::default();
        for path in files {
            match self.free_one(&path, Wait::No).await {
                Ok((bytes, _)) => {
                    freed.files += 1;
                    freed.bytes += bytes;
                }
                Err(SyncError::InUse) => freed.busy += 1,
                Err(SyncError::ModifiedLocally) => freed.modified += 1,
                // Counted as busy, as `FreeUpSpace` says: it waits to go up.
                Err(SyncError::NotUploaded(_)) => freed.busy += 1,
                Err(e @ (SyncError::NoHelper | SyncError::NoRoot)) => return (freed, Some(e)),
                Err(e) => tracing::info!("{} is not freed up: {e}", path.display()),
            }
        }
        (freed, None)
    }

    /// What `FreeUp` checks before it frees anything, on its own:
    /// [`check_unpinnable`](Self::check_unpinnable)'s rules, and a folder
    /// with interception has its helper (`NoHelper`).
    pub async fn check_free_up(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        let reg = self.require_record()?;
        if reg.intercepted() {
            self.require_link()?;
        }
        self.check_unpinnable(paths).await?;
        // A file named itself with a change waiting to go up refuses the whole
        // call, before any account frees anything (the outbox on the bus).
        let targets = self.pin_targets(&reg.root, paths).await?;
        for target in targets.iter().filter(|t| !t.is_dir) {
            self.refuse_unuploaded(&target.item, &target.shown.display().to_string()).await?;
        }
        Ok(())
    }

    /// `FreeUp(paths)`, the menu's "Free up space":
    ///
    /// - a path with a pin of its own loses it, and then everything under it
    ///   is freed up;
    /// - a path a folder above it pins is refused `NotAllowed`, naming that
    ///   folder — unless that folder is one of `paths`, whose pin this call
    ///   takes off ([`kept_by_folder`]); checked for every path before
    ///   anything changes;
    /// - any other path is freed up.
    ///
    /// The pins come off first. One that cannot stops the call with that
    /// failure, before anything is freed; the pins already off stay off, and
    /// `PinnedCount` says so.
    ///
    /// Each file goes through `FreeUpSpace`'s per-file path: one in use or
    /// changed here is left and counted (`busy` and [`FreedUp::modified`]),
    /// and one a pin of its own — or of a folder between it and the path, or
    /// above the path by the time it is walked — keeps is left and counted in
    /// [`FreedUp::pinned`]. One `freed` event per path that freed anything.
    pub async fn free_up(&self, paths: &[PathBuf]) -> Result<FreedUp, SyncError> {
        let reg = self.require_record()?;
        if reg.intercepted() {
            self.require_link()?;
        }
        let targets = self.pin_targets(&reg.root, paths).await?;
        if let Some(refusal) = kept_by_folder(&targets) {
            return Err(refusal);
        }
        // A file named itself, whose change waits to be uploaded, is refused as
        // a whole (`docs/design/writes.md` §11); inside a folder it is left and counted.
        for target in targets.iter().filter(|t| !t.is_dir) {
            self.refuse_unuploaded(&target.item, &target.shown.display().to_string()).await?;
        }
        let walks: Vec<(PathBuf, bool)> = targets.iter().map(|t| (t.shown.clone(), t.is_dir)).collect();
        // The other descriptors close here: one of our own left open on a
        // file would refuse the write lease its free-up takes.
        let own: Vec<PinTarget> = targets.into_iter().filter(|target| target.own).collect();
        let (unpinned, failed) = self.set_pins(own, false).await;
        for shown in &unpinned {
            self.pins.unpinned(shown);
        }
        if let Some(e) = failed {
            return Err(e);
        }
        let mut total = FreedUp::default();
        let mut stopped = None;
        for (shown, is_dir) in walks {
            let (root, start) = (reg.root.path.clone(), shown.clone());
            let (candidates, pinned) = tokio::task::spawn_blocking(move || {
                // Looked at again now: a folder above pinned since the check
                // keeps everything under it.
                let inherited = pin::pinned_above(&root, &start).is_some();
                pin::downloaded_under(&start, inherited)
            })
            .await
            .map_err(|e| SyncError::Io(format!("the walk of the folder failed: {e}")))?;
            let (freed, stop) = self.free_each(candidates).await;
            if freed.files > 0 {
                let detail = if is_dir { freed_detail(freed.files, freed.bytes) } else { activity::human_size(freed.bytes) };
                let event = activity::event(Kind::Freed, shown.display().to_string(), detail);
                self.report.activity.record(vec![event]).await;
            }
            total.files += freed.files;
            total.bytes += freed.bytes;
            total.busy += freed.busy;
            total.modified += freed.modified;
            total.pinned += pinned;
            if stop.is_some() {
                stopped = stop;
                break;
            }
        }
        self.report.space.kick();
        match stopped {
            Some(SyncError::NoRoot) | None => Ok(total),
            Some(e) => Err(e),
        }
    }

    /// One file freed up, and how many bytes of blocks that gave back —
    /// `Dehydrate`'s whole sequence. `Wait::No` answers `InUse` rather than
    /// wait for a fill or a free-up of the same file (`FreeUpSpace`).
    ///
    /// # Open, wait for the file, and only then decide
    ///
    /// The mode decides what the punch may go by, and it must not change
    /// between that decision and the punch (see `folder`). The version
    /// this replaces took the state lock first and then waited for a
    /// fill of the same inode — a download of any length — holding up every
    /// registration and Forget meanwhile. Now the file is opened and its
    /// inode lock taken first, and the state lock after: the
    /// registration is looked at again under it, and a folder forgotten or
    /// registered anew meanwhile is refused, with nothing punched.
    async fn free_one(&self, path: &Path, wait: Wait) -> Result<(u64, String), SyncError> {
        let reg = self.require_record()?;
        if reg.intercepted() {
            // Refused before a wait that could only end in the same refusal.
            self.require_link()?;
        }
        let root = reg.root.clone();
        let target = path.to_path_buf();
        let (file, shown) = tokio::task::spawn_blocking(move || open_shown(&root, &target))
            .await
            .map_err(|e| SyncError::Io(format!("the dehydration task failed: {e}")))??;
        let key = InodeKey::of(&file).map_err(|e| SyncError::Io(e.to_string()))?;

        let _guard = match wait {
            Wait::Yes => self.locks.lock(key).await,
            Wait::No => self.locks.try_lock(key).ok_or(SyncError::InUse)?,
        };
        // A change waiting to be uploaded is only here (`docs/design/writes.md` §11):
        // looked at under the inode lock, and refused when it cannot be told.
        self.refuse_unuploaded(&file, &shown).await?;
        let folder = self.folder.read().await;
        let reg = match folder.acted_on() {
            Some(now) if now.root.path == reg.root.path && now.root.root_id == reg.root.root_id => now.clone(),
            _ => return Err(SyncError::NoRoot),
        };
        // local rule decides at the punch (`Clearance`,
        // `root::dehydrate_opened`). An intercepted root is refused outright
        // without its link: freed up while nothing intercepts, the file
        // would read zeros until the helper is back. A root registered
        // without interception reads zeros by design, and goes by the rule:
        // its link if it has one — the helper then clears the mark, which it
        // grants on ownership of the file alone — or, with none, whether a
        // helper is running at all.
        let clearance = if reg.intercepted() {
            Clearance::Link(self.require_link()?)
        } else {
            self.clearance()
        };
        // Measured under the lock, so no fill of the same file changes it in
        // between, and on a second descriptor for the same inode, since the
        // first is handed over whole.
        let probe = file.try_clone().map_err(|e| SyncError::Io(e.to_string()))?;
        let blocks = |file: &File| file.metadata().map(|m| m.blocks()).unwrap_or(0);
        let before = blocks(&probe);
        crate::hydration::dehydrate::dehydrate_opened(&clearance, file).await.map_err(SyncError::from)?;
        Ok((before.saturating_sub(blocks(&probe)) * 512, shown))
    }

    /// Refuses a free-up of the file open as `file` (shown as `shown`) while a
    /// change of it waits to be uploaded: freeing it up would lose that change
    /// (`NotUploaded`). Fails closed (the outbox on the bus): a OneDrive folder's
    /// outbox that cannot be read — its sync not started yet, a store error —
    /// refuses. Only a downloaded file is asked about: one that is not has
    /// nothing to lose, and its own refusal says so (`NotHydrated`, M5).
    async fn refuse_unuploaded(&self, file: &File, shown: &str) -> Result<(), SyncError> {
        let Some(reg) = self.record() else { return Ok(()) };
        if reg.source != crate::sync::RootSource::OneDrive {
            return Ok(());
        }
        let cannot_tell = |why: String| {
            SyncError::Io(format!("{shown} is not freed up: cannot tell whether a change of it waits to be uploaded ({why}); try again in a moment"))
        };
        // A state that cannot be read cannot tell either.
        match placeholder::read_state(file) {
            Ok(Some(placeholder::State::Hydrated)) => {}
            Ok(_) => return Ok(()),
            Err(e) => return Err(cannot_tell(e.to_string())),
        }
        let Some(store) = self.tree_store() else { return Err(cannot_tell("the folder's sync has not started".into())) };
        let id = placeholder::read_item_id(file).map_err(|e| cannot_tell(e.to_string()))?;
        let meta = file.metadata().map_err(|e| cannot_tell(e.to_string()))?;
        let inode = Inode { dev: meta.dev(), ino: meta.ino(), handle: FileHandle::of(file).ok() };
        // Through the shared connection, off the async runtime: a change an
        // examination is recording now is waited for, not missed.
        let waiting = store
            .call(move |s| {
                let by_item = match &id {
                    Some(id) => !s.outbox_for_item(id)?.is_empty(),
                    None => false,
                };
                Ok(by_item || !s.outbox_for_inode(&inode)?.is_empty())
            })
            .await
            .map_err(|e| cannot_tell(e.to_string()))?;
        if waiting {
            return Err(SyncError::NotUploaded(shown.to_owned()));
        }
        Ok(())
    }
}

/// Whether a free-up waits for the per-inode lock ([`SyncService::free_one`]).
#[derive(Clone, Copy)]
enum Wait {
    Yes,
    No,
}

/// What `FreeUpSpace()` and `FreeUp()` did: files freed up, the bytes of
/// blocks that gave back, files left as they were because they were in use
/// or changed here, and downloaded files left because a pin keeps them on
/// this device. `FreeUp` answers `busy + modified` as its `busy`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FreedUp {
    pub files: u32,
    pub bytes: u64,
    pub busy: u32,
    pub modified: u32,
    pub pinned: u32,
}

/// A `freed` event's detail for more than one file: "2 files, 1.5 MiB".
fn freed_detail(files: u32, bytes: u64) -> String {
    format!("{files} file{}, {}", if files == 1 { "" } else { "s" }, activity::human_size(bytes))
}
