use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use konedrive_fs::placeholder::{read_stamp, read_state, stamp_matches, State, StateError};

use crate::hydration::tracked::Tracked;
use crate::helper::{Clearance, NotCleared};
use crate::folder::root::DehydrateError;
use crate::folder::root::SyncRoot;
use crate::hydration::source::{Answered, ContentSource, Fetched, FillError, SourceError};
use crate::folder::locks::{InodeKey, unless_removed};
use crate::sync::{SyncError, SyncService};
use crate::hydration::server::fill_event;
use crate::hydration::source;

impl SyncService {
    /// Fills one placeholder now — see the type's own doc comment for why
    /// this fills directly rather than only through kernel interception.
    ///
    /// # Open first, then lock, then look again
    ///
    /// The order matters three times over.
    ///
    /// *Open through the gate.* The descriptor comes from
    /// `SyncRoot::open_inside` — `openat2` with `RESOLVE_BENEATH`,
    /// `RESOLVE_NO_SYMLINKS` and `O_NOFOLLOW`, from the root's own
    /// descriptor, only while the folder still carries this root's id. The
    /// version this replaces checked a canonicalized *string*, awaited a
    /// lock with no time limit, and only then opened that string by name;
    /// measured, an ordinary directory rename inside the root in that window
    /// was enough to have `source::hydrate` `pwrite` a file **outside** the
    /// root and `Hydrate` report success.
    ///
    /// *Lock after the open, not before it.* Taking the lock first
    /// deadlocked the very path it exists to coordinate with: with a real
    /// helper and a marked root, opening an `online-only` file is
    /// intercepted, and the interception travels helper →
    /// `serve_hydrations` → the same lock, which this call is holding while
    /// it waits for that open to return. Nothing in `cargo test` could reach
    /// it, because nothing unprivileged can install a fanotify group.
    ///
    /// *Read the state again under the lock.* Whoever held the lock first
    /// may well have been a fill of this same inode, and it may have
    /// finished the job.
    pub async fn hydrate_now(&self, path: &Path) -> Result<(), SyncError> {
        // "Download now" is an open, for the pool: it goes first.
        match self.fill_now(path, Some(konedrive_graph::pool::Class::Open)).await? {
            // No link to the helper, on a file that may carry an ignore mark.
            Answered::Failed(FillError::NotCleared(NotCleared::Unlinked | NotCleared::NoWay)) => Err(SyncError::NoHelper),
            Answered::Failed(FillError::NotCleared(e)) => Err(SyncError::Io(format!("nothing was filled: {e}"))),
            Answered::Failed(FillError::Errno(errno)) => Err(SyncError::Io(format!(
                "hydration failed: {}",
                std::io::Error::from_raw_os_error(errno)
            ))),
            _ => Ok(()),
        }
    }

    /// [`hydrate_now`](Self::hydrate_now)'s fill, and what came of it —
    /// recorded as any fill is. A pinned download goes through here too
    /// ([`pin::PinFill`]).
    ///
    /// `class` is the slot of the account's transfer pool it takes, before the per-inode
    /// lock (never waiting for a slot with the lock held); `None` when the caller holds one
    /// already (a pinned download). A pinned download of a large file goes in parallel parts
    /// (`source::parts`, issue #28), the slot held for it being its first stream's; a file
    /// being opened, and `Hydrate`, keep one stream.
    pub(super) async fn fill_now(&self, path: &Path, class: Option<konedrive_graph::pool::Class>) -> Result<Answered, SyncError> {
        let reg = self.require_record()?;
        let Some(source) = self.source.lock().unwrap().clone() else {
            return Err(SyncError::NoSource);
        };

        let root = reg.root.clone();
        let target = path.to_path_buf();
        let (file, shown) = tokio::task::spawn_blocking(move || open_shown(&root, &target))
            .await
            .map_err(|e| SyncError::Io(format!("the hydration task failed: {e}")))??;
        let key = InodeKey::of(&file).map_err(|e| SyncError::Io(e.to_string()))?;

        // A placeholder has its full size: whether this is a large transfer.
        let size = konedrive_graph::pool::Size::of(file.metadata().map_or(0, |meta| meta.len()));
        let mut slot = match class {
            Some(class) => Some(self.pool.acquire_sized(class, size).await),
            None => None,
        };
        let split = (class.is_none() && size == konedrive_graph::pool::Size::Large)
            .then(|| source::Split::new(Arc::clone(&self.pool), Arc::clone(&self.parts)));
        // Serializes against `dehydrate()` and against `serve_hydrations`'s
        // own fills of the same inode (both share this table).
        let guard = self.locks.lock(key).await;

        let (file, decision) = tokio::task::spawn_blocking(move || {
            let decision = classify_for_hydration(&file);
            (file, decision)
        })
        .await
        .map_err(|e| SyncError::Io(format!("the hydration task failed: {e}")))?;
        match decision? {
            Fill::AlreadyThere => return Ok(Answered::AlreadyThere),
            Fill::Needed => {}
        }
        // A file that could be carrying an ignore mark has the way cleared
        // before the fill can fail and punch it, by local rule. Whether this
        // file is one is the fill's to decide (`source::hydrate_with`); here
        // it is only given what there is to clear with. An intercepted root
        // needs its link for that — without one the fill gets nothing, and
        // refuses rather than fill a file a failure would then empty under
        // its mark. A root registered without interception clears it the
        // same way when there is a link, and otherwise goes by whether a
        // helper is running at all (`Clearance`).
        let clearance = if reg.intercepted() { self.link().map(Clearance::Link) } else { Some(self.clearance()) };

        let fd: std::os::fd::OwnedFd = file.into();
        // Shown in `Transfers.Downloads` while it downloads; `Hydrate` (an open, for the pool)
        // as a file being opened.
        let tracked = if class == Some(konedrive_graph::pool::Class::Open) {
            Tracked::opening(source, self.report.transfers.clone(), shown.clone())
        } else {
            Tracked::new(source, self.report.transfers.clone(), shown.clone())
        };
        // Stopped where it is when the file is taken off the disk because
        // OneDrive removed its item (issue #104).
        let fill = async {
            match &split {
                Some(split) => source::hydrate_in_parts(fd, &tracked, clearance.as_ref(), split).await,
                None => source::hydrate_with(fd, &tracked, clearance.as_ref()).await,
            }
        };
        let filled = unless_removed(Some(&guard), fill).await.unwrap_or(Err(FillError::Errno(libc::ENOENT)));
        let size = tracked.fetched();
        drop(tracked);
        drop(guard);
        let answered = match filled {
            Ok(()) => Answered::Filled,
            Err(e) => Answered::Failed(e),
        };
        if let (Some(slot), Answered::Filled) = (slot.as_mut(), &answered) {
            slot.succeeded();
        }
        drop(slot);
        // A fill that never started for want of the helper is a refusal
        // the caller is told of, not a download that failed.
        let refused = matches!(answered, Answered::Failed(FillError::NotCleared(_)));
        if let Some(event) = fill_event(&answered, &shown, size).filter(|_| !refused) {
            self.report.activity.record(vec![event]).await;
            self.report.space.kick();
        }
        Ok(answered)
    }
}

/// A file opened through the gate (`SyncRoot::open_inside`), and its full
/// path as the activity log names it: inside the root as it was registered,
/// however `path` spelled it (a link on the way, `..`) — events are kept
/// only for paths inside the registered folder.
pub(super) fn open_shown(root: &SyncRoot, path: &Path) -> Result<(File, String), DehydrateError> {
    let file = root.open_inside(path)?;
    let shown = root.relative(path).map(|rel| root.path.join(rel)).unwrap_or_else(|_| path.to_path_buf());
    Ok((file, shown.display().to_string()))
}

/// Whether [`SyncService::hydrate_now`] still has work to do.
enum Fill {
    /// It does. Whether the file may carry an ignore mark, and so has to be
    /// cleared first, is decided by the fill (`source::hydrate_with`).
    Needed,
    AlreadyThere,
}

/// What a file's own state says about whether it needs filling.
///
/// # The `hydrated` label is not believed on its own
///
/// `check_dehydratable` verifies the stamp before it punches; this did not,
/// so a file whose `user.konedrive.state` says `hydrated` over a hole —
/// measured: 4096 bytes, 0 blocks, hand-labelled — reported success from
/// `Hydrate` and stayed empty. §9 names that state exactly, and repairing it
/// is what a manual "download it now" is for.
///
/// The three cases are told apart deliberately:
///
/// - **no stamp at all** → fill. Nothing this daemon wrote can be in that
///   state: `fill` writes the stamp *before* it writes `hydrated`, so a
///   `hydrated` file with no stamp was labelled by something else, and its
///   content is exactly as unproven as an `online-only` file's.
/// - **a stamp that matches** → nothing to do.
/// - **a stamp that does not match** → refuse with "modified locally",
///   never fill. There is no upload in this sub-project, so a local edit is
///   the only copy of that data (§8), and overwriting it with remote content
///   would be the same class of permanent loss this whole component exists
///   to avoid — just pointing the other way. Refusing is loud, and it leaves
///   the user a file they can still copy out.
///
/// A zero-byte file is `hydrated` from birth and carries no stamp by design
/// (`create_placeholder`), so it is answered before any of that.
fn classify_for_hydration(file: &File) -> Result<Fill, SyncError> {
    match read_state(file) {
        Ok(None) => Err(SyncError::NotManaged),
        Ok(Some(State::Hydrated)) => {
            let empty = file.metadata().map_err(|e| SyncError::Io(e.to_string()))?.len() == 0;
            if empty {
                return Ok(Fill::AlreadyThere);
            }
            match read_stamp(file).map_err(|e| SyncError::Io(e.to_string()))? {
                None => Ok(Fill::Needed),
                Some(_) => {
                    if stamp_matches(file).map_err(|e| SyncError::Io(e.to_string()))? {
                        Ok(Fill::AlreadyThere)
                    } else {
                        Err(SyncError::ModifiedLocally)
                    }
                }
            }
        }
        // `dehydrating` is not "somebody is busy with it": under the
        // per-inode lock this call holds, no dehydration of this inode can
        // be running. It is what a crash — or a cancelled `Dehydrate`
        // (`root::dehydrate`'s) — left behind, and §5.2 says
        // exactly what to do with it: treat it as "hydrate it again".
        // Reporting success over whatever the punch got to is the one thing
        // that must not happen.
        Ok(Some(State::OnlineOnly | State::Hydrating | State::Dehydrating)) => Ok(Fill::Needed),
        Err(StateError::Io(e)) => Err(SyncError::Io(e.to_string())),
        Err(StateError::Corrupt(value)) => {
            Err(SyncError::Io(format!("unrecognised state {value:?}")))
        }
    }
}

/// `SyncService` is itself a valid, if initially empty, `ContentSource`:
/// `serve_hydrations` is started once, at daemon startup,
/// before any root — let alone any source directory — necessarily exists
/// yet. Delegating to whatever `populate_from_directory` most recently
/// registered means `serve_hydrations` does not need to be restarted (or
/// handed a source through some other side channel) once a root and a
/// source do exist.
#[async_trait]
impl ContentSource for SyncService {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let source = self.source.lock().unwrap().clone();
        match source {
            Some(source) => source.fetch(item_id, from, end).await,
            None => Err(SourceError::NotFound(format!(
                "{item_id}: no content source is registered"
            ))),
        }
    }
}
