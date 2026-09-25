//! The outbox as `org.konedrive.Sync1` shows it (`docs/design/writes.md` §11):
//! what waits to be uploaded, the pause, the ignore list, the mass-delete
//! guard's decision and what stays local. `sync::dbus` is the thin wrapper
//! around these; the worker itself is `sync::upload`.
//!
//! **Pause** is per account and kept in the tree store (`meta.paused_until`),
//! so it survives a restart and a timed one ends by itself. It stops the
//! outbox, the poll (and so the replacements it runs) and the thumbnails;
//! fills on open, `Hydrate` and the watcher's detection go on, and rows keep
//! coalescing.

use std::fs::File;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder;

use super::local::ignore::{IgnoreList, SharedIgnore};
use super::upload::{self, OutboxHost, WorkerStatus};
use super::{Persist, SyncError, SyncService};
use crate::config::ConfigError;
use crate::tree::outbox::{Inode, OutboxState};
use crate::tree::{ActivityRow, Store};

/// One row as `Outbox()` lists it: (seq, kind, full path, state, bytes sent,
/// bytes in all, reason, next try in unix seconds or 0).
pub type OutboxEntry = (u64, String, String, String, u64, u64, String, i64);

/// The ignore list `config.toml` gives the account, or the defaults.
pub(super) fn configured_ignore(persist: Option<&Persist>) -> SharedIgnore {
    let account = persist.and_then(|p| p.store.account(&p.account));
    IgnoreList::configured(account.as_ref().and_then(|a| a.ignore.as_deref())).shared()
}

impl SyncService {
    /// The tree store of this account's OneDrive folder: refused as `Refresh`
    /// is for a folder that is not connected to OneDrive, and `NoRoot`
    /// before its sync has opened one.
    fn outbox_store(&self) -> Result<Store, SyncError> {
        self.require_onedrive()?;
        self.store.lock().unwrap().clone().ok_or(SyncError::NoRoot)
    }

    /// Runs `f` on the store on a blocking thread.
    async fn with_outbox<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut crate::tree::TreeStore) -> Result<T, crate::tree::TreeError> + Send + 'static,
    ) -> Result<T, SyncError> {
        self.outbox_store()?.run(f).await.map_err(|e| SyncError::Io(e.to_string()))
    }

    /// Wakes the worker of the sync running now, if any.
    pub(super) fn wake_outbox(&self) {
        if let Some(outbox) = self.syncing.lock().unwrap().as_ref().and_then(|s| s.outbox.as_ref()) {
            outbox.wake();
        }
    }

    /// `Refresh()`'s part for the outbox: rows in backoff go now.
    pub(super) fn retry_outbox(&self) {
        let syncing = self.syncing.lock().unwrap();
        if let Some(outbox) = syncing.as_ref().and_then(|s| s.outbox.as_ref()) {
            if let Err(e) = outbox.retry_now() {
                tracing::warn!("cannot make the outbox's waiting rows due: {e}");
            }
        }
    }

    /// `Pause(seconds)`: nothing is uploaded, and OneDrive is not asked for
    /// changes, until `seconds` have passed — or until `Resume()` when 0.
    /// Kept in the tree store, so it outlasts a restart.
    pub async fn pause_syncing(&self, seconds: u32) -> Result<(), SyncError> {
        let store = self.outbox_store()?;
        let until = if seconds == 0 { 0 } else { crate::sync::activity::unix_now() + i64::from(seconds) };
        tokio::task::spawn_blocking(move || upload::set_paused(&store, Some(until)))
            .await
            .map_err(|e| SyncError::Io(format!("the pause task failed: {e}")))?
            .map_err(|e| SyncError::Io(e.to_string()))?;
        tracing::info!("syncing paused{}", if seconds == 0 { " until resumed".to_owned() } else { format!(" for {seconds} s") });
        self.show_pause();
        Ok(())
    }

    /// `Resume()`: the pause ends now; the outbox and the poll go at once.
    pub async fn resume_syncing(&self) -> Result<(), SyncError> {
        let store = self.outbox_store()?;
        tokio::task::spawn_blocking(move || upload::set_paused(&store, None))
            .await
            .map_err(|e| SyncError::Io(format!("the resume task failed: {e}")))?
            .map_err(|e| SyncError::Io(e.to_string()))?;
        tracing::info!("syncing resumed");
        self.show_pause();
        Ok(())
    }

    /// Reads the pause from the store into `Paused`/`PausedUntil`. When it
    /// changed, what it holds back is woken — the outbox, and the poll, which
    /// looks at the pause before each cycle. A timed pause gets a timer that
    /// does this again when it ends. Called whenever the pause may have
    /// changed: `Pause`, `Resume`, a sync starting, the timer.
    pub(super) fn show_pause(&self) {
        let Some(store) = self.store.lock().unwrap().clone() else { return };
        self.pause_shown.fetch_add(1, Ordering::SeqCst);
        let paused = upload::paused(&store);
        let before = self.state.get().paused_until;
        self.state.update(|s| s.paused_until = paused);
        if before != paused {
            self.wake_outbox();
            self.nudge();
        }
        if paused.is_some_and(|until| until > 0) {
            self.watch_pause_end();
        }
    }

    /// One timer per account that looks at the pause at most every
    /// [`PAUSE_LOOK`], by the wall clock, so that a suspend does not stretch it
    /// (the outbox on the bus), and shows its end.
    fn watch_pause_end(&self) {
        let mut timer = self.pause_timer.lock().unwrap();
        if timer.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let me = self.me.clone();
        *timer = Some(runtime.spawn(async move {
            loop {
                let Some(service) = me.upgrade() else { return };
                let seen = service.pause_shown.load(Ordering::SeqCst);
                let left = match service.state.get().paused_until {
                    Some(until) if until > 0 => (until - crate::sync::activity::unix_now()).max(0) as u64 + 1,
                    _ if service.pause_timer_done(seen, false) => return,
                    _ => continue,
                };
                drop(service);
                tokio::time::sleep(Duration::from_secs(left).min(PAUSE_LOOK)).await;
                let Some(service) = me.upgrade() else { return };
                let store = service.store.lock().unwrap().clone();
                let Some(store) = store else { return };
                let still = tokio::task::spawn_blocking(move || upload::paused(&store)).await.ok().flatten();
                if still.is_none() && service.pause_timer_done(seen, true) {
                    service.wake_outbox();
                    service.nudge();
                    return;
                }
            }
        }));
    }

    /// The timer is done — `end`: the store said the pause is over, and so
    /// does the bus now — unless a pause was shown since it had seen `seen`
    /// of them, or (`end` false) the bus shows a timed pause again: then it
    /// looks again (the outbox on the bus). Under the timer's lock, so that a
    /// pause shown meanwhile is either seen here or finds no timer and
    /// starts one.
    pub(super) fn pause_timer_done(&self, seen: u64, end: bool) -> bool {
        let mut timer = self.pause_timer.lock().unwrap();
        let timed = self.state.get().paused_until.is_some_and(|until| until > 0);
        if self.pause_shown.load(Ordering::SeqCst) != seen || (!end && timed) {
            return false;
        }
        if end {
            self.state.update(|s| s.paused_until = None);
        }
        *timer = None;
        true
    }

    /// The folder is forgotten: so is its pause, on the bus.
    pub(super) fn forget_pause(&self) {
        if let Some(timer) = self.pause_timer.lock().unwrap().take() {
            timer.abort();
        }
        self.state.update(|s| s.paused_until = None);
    }

    /// The outbox's counts on the bus are 0: its worker stopped, or its rows
    /// were dropped (the outbox on the bus). A worker that starts counts again.
    pub(super) fn clear_outbox_counts(&self) {
        self.state.update(|s| {
            s.pending_count = 0;
            s.pending_bytes = 0;
            s.blocked_count = 0;
            s.held_count = 0;
            s.uploads.clear();
        });
    }

    /// `Outbox(limit)`: the rows waiting to be uploaded, oldest first, at most
    /// `limit` (0 for all): (seq, kind, full path, state, bytes sent, bytes
    /// in all, reason, next try).
    pub async fn outbox(&self, limit: u32) -> Result<Vec<OutboxEntry>, SyncError> {
        let take = if limit == 0 { usize::MAX } else { limit as usize };
        let rows = self.with_outbox(move |s| Ok(s.outbox_rows()?.into_iter().take(take).collect::<Vec<_>>())).await?;
        let root = self.registration().map(|reg| reg.root.path).unwrap_or_default();
        let uploads = self.state.get().uploads;
        tokio::task::spawn_blocking(move || entries(rows, &root, &uploads))
            .await
            .map_err(|e| SyncError::Io(format!("the outbox task failed: {e}")))
    }
}

/// `Outbox()`'s entries for `rows`; a waiting file's size is read from the
/// disk (`lstat`), off the runtime.
fn entries(rows: Vec<crate::tree::outbox::OutboxRow>, root: &std::path::Path, uploads: &[(String, u64, u64)]) -> Vec<OutboxEntry> {
    rows
        .into_iter()
        .map(|row| {
            let path = root.join(&row.rel).display().to_string();
            let (done, total) = match uploads.iter().find(|(p, _, _)| *p == path) {
                Some((_, sent, total)) => (*sent, *total),
                None if row.kind.sends_content() => {
                    let size = row
                        .snapshot
                        .as_deref()
                        .and_then(|s| s.split(' ').next())
                        .and_then(|s| s.parse().ok())
                        .or_else(|| std::fs::symlink_metadata(root.join(&row.rel)).ok().filter(|m| m.is_file()).map(|m| m.len()));
                    (0, size.unwrap_or(0))
                }
                None => (0, 0),
            };
            (
                row.seq as u64,
                row.kind.as_str().to_owned(),
                path,
                row.state.as_str().to_owned(),
                done,
                total,
                row.reason.unwrap_or_default(),
                row.next_try.unwrap_or(0),
            )
        })
        .collect()
}

/// How often at most a timed pause is looked at by the wall clock.
const PAUSE_LOOK: Duration = Duration::from_secs(60);

impl SyncService {

    /// `NotUploaded()`: what stays on this computer and why, as (full path,
    /// reason) — what is never uploaded (a symlink, a file from elsewhere
    /// that is not downloaded, …) and the changes that need the user
    /// (blocked: a name OneDrive refuses, OneDrive full).
    pub async fn not_uploaded(&self) -> Result<Vec<(String, String)>, SyncError> {
        let (skipped, rows) = self.with_outbox(|s| Ok((s.local_skipped()?, s.outbox_rows()?))).await?;
        let root = self.registration().map(|reg| reg.root.path).unwrap_or_default();
        let mut out: Vec<(String, String)> = skipped.into_iter().map(|s| (root.join(&s.rel).display().to_string(), s.reason)).collect();
        out.extend(
            rows.into_iter()
                .filter(|row| row.state == OutboxState::Blocked)
                .map(|row| (root.join(&row.rel).display().to_string(), row.reason.unwrap_or_else(|| "blocked".into()))),
        );
        out.sort();
        Ok(out)
    }

    /// `ConfirmDeletes()`: the removals the mass-delete guard held go ahead;
    /// how many.
    pub async fn confirm_deletes(&self) -> Result<u32, SyncError> {
        let released = self.with_outbox(|s| s.outbox_release_held()).await?;
        self.wake_outbox();
        Ok(released as u32)
    }

    /// `RestoreDeletes()`: the removals the mass-delete guard held are dropped,
    /// and their items placed again from OneDrive at once, by a cycle with a
    /// Full reconcile (the outbox on the bus); how many. Under the tree lock, as the
    /// worker's commits are, so that a cycle's swap cannot give the items
    /// their forgotten local objects back.
    pub async fn restore_deletes(&self) -> Result<u32, SyncError> {
        let dropped = {
            let _tree = self.tree_lock.lock().await;
            self.with_outbox(|s| s.outbox_drop_held()).await?
        };
        if !dropped.is_empty() {
            self.nudge_full();
        }
        // What a dropped move out named outside the folder is tidied, before the
        // answer, whether or not a worker runs.
        let store = self.store.lock().unwrap().clone();
        if let (Some(reg), Some(store)) = (self.registration(), store) {
            self.tidy_dropped(&reg.root, &store, &dropped).await;
        }
        self.wake_outbox();
        Ok(dropped.len() as u32)
    }

    /// `IgnorePatterns`: the account's ignore list (`docs/design/writes.md` §4.4).
    pub fn ignore_patterns(&self) -> Vec<String> {
        self.ignore.read().unwrap_or_else(|p| p.into_inner()).patterns().to_vec()
    }

    /// `SetIgnorePatterns(patterns)`: the account's ignore list from now on,
    /// written to `config.toml`. A Full local scan follows, so that a name
    /// no longer ignored is uploaded. Refused `InvalidArgs` for a pattern that
    /// cannot match a name ([`IgnoreList::invalid`]).
    pub async fn set_ignore_patterns(&self, patterns: Vec<String>) -> Result<(), SyncError> {
        if let Some((pattern, why)) = patterns.iter().find_map(|p| IgnoreList::invalid(p).map(|why| (p, why))) {
            return Err(SyncError::InvalidArgs(format!("{pattern:?}: {why}")));
        }
        let mut unique: Vec<String> = Vec::new();
        for pattern in patterns {
            if !unique.contains(&pattern) {
                unique.push(pattern);
            }
        }
        // `config.toml` and the list the watcher reads change together, under the
        // list's own lock: two calls at once leave both the same (the outbox on the bus).
        let (shared, persist) = (Arc::clone(&self.ignore), self.persist.as_ref().map(|p| (Arc::clone(&p.store), p.account.clone())));
        tokio::task::spawn_blocking(move || {
            let mut list = shared.write().unwrap_or_else(|p| p.into_inner());
            if let Some((store, account)) = persist {
                let kept = unique.clone();
                store
                    .update_account(&account, |a| {
                        a.ignore = Some(kept);
                        Ok::<_, ConfigError>(())
                    })
                    .map_err(|e| SyncError::Io(format!("cannot write config.toml: {e}")))?;
            }
            *list = IgnoreList::new(unique);
            Ok::<(), SyncError>(())
        })
        .await
        .map_err(|e| SyncError::Io(format!("the settings task failed: {e}")))??;
        if let Some(watcher) = self.syncing.lock().unwrap().as_ref().and_then(|s| s.watcher.as_ref()) {
            watcher.full_scan();
        }
        Ok(())
    }

    /// `MachineName`: what a conflict copy is named after (`docs/design/writes.md` §7):
    /// `machine_name` in `config.toml`, or the host's name.
    pub fn machine_name(&self) -> String {
        let configured = self.persist.as_ref().and_then(|p| p.store.account(&p.account)).map(|a| a.machine_name).unwrap_or_default();
        if configured.is_empty() {
            upload::default_machine_name()
        } else {
            upload::machine_name(&configured)
        }
    }

    /// Refuses a free-up of the file open as `file` (shown as `shown`) while a
    /// change of it waits to be uploaded: freeing it up would lose that change
    /// (`NotUploaded`). Fails closed (the outbox on the bus): a OneDrive folder's
    /// outbox that cannot be read — its sync not started yet, a store error —
    /// refuses. Only a downloaded file is asked about: one that is not has
    /// nothing to lose, and its own refusal says so (`NotHydrated`, M5).
    pub(super) fn refuse_unuploaded(&self, file: &File, shown: &str) -> Result<(), SyncError> {
        let Some(reg) = self.registration() else { return Ok(()) };
        if reg.source != super::RootSource::OneDrive {
            return Ok(());
        }
        let cannot_tell = |why: String| {
            SyncError::Io(format!("{shown} is not freed up: cannot tell whether a change of it waits to be uploaded ({why}); try again in a moment"))
        };
        // A state that cannot be read cannot tell either (the outbox on the bus re-review).
        match placeholder::read_state(file) {
            Ok(Some(placeholder::State::Hydrated)) => {}
            Ok(_) => return Ok(()),
            Err(e) => return Err(cannot_tell(e.to_string())),
        }
        let Some(store) = self.store.lock().unwrap().clone() else { return Err(cannot_tell("the folder's sync has not started".into())) };
        let id = placeholder::read_item_id(file).map_err(|e| cannot_tell(e.to_string()))?;
        let meta = file.metadata().map_err(|e| cannot_tell(e.to_string()))?;
        let inode = {
            use std::os::unix::fs::MetadataExt;
            Inode { dev: meta.dev(), ino: meta.ino(), handle: FileHandle::of(file).ok() }
        };
        let waiting = store
            .with(|s| {
                let by_item = match &id {
                    Some(id) => !s.outbox_for_item(id)?.is_empty(),
                    None => false,
                };
                Ok(by_item || !s.outbox_for_inode(&inode)?.is_empty())
            })
            .map_err(|e| cannot_tell(e.to_string()))?;
        if waiting {
            return Err(SyncError::NotUploaded(shown.to_owned()));
        }
        Ok(())
    }
}

/// The outbox worker's view of its account's sync: live activity, its
/// status in the published state, and a cycle when OneDrive changed under a
/// row.
pub(super) struct Host {
    sync: Weak<SyncService>,
}

impl Host {
    pub(super) fn new(sync: Weak<SyncService>) -> Self {
        Self { sync }
    }
}

impl OutboxHost for Host {
    /// `ActivityAdded`: the worker has written the event into the store with
    /// its commit.
    fn activity(&self, event: &ActivityRow) {
        if let Some(service) = self.sync.upgrade() {
            service.report.activity.announce(event.clone());
        }
    }

    /// `PendingCount`, `PendingBytes`, `BlockedCount` and `Uploads`.
    fn status(&self, status: &WorkerStatus) {
        let Some(service) = self.sync.upgrade() else { return };
        let root = service.registration().map(|reg| reg.root.path).unwrap_or_default();
        let uploads: Vec<(String, u64, u64)> =
            status.uploads.iter().map(|u| (root.join(&u.rel).display().to_string(), u.sent, u.total)).collect();
        service.state.update(|s| {
            s.pending_count = status.counts.pending;
            s.pending_bytes = status.counts.pending_bytes;
            s.blocked_count = status.counts.blocked;
            s.held_count = status.counts.held;
            s.uploads = uploads;
        });
    }

    /// OneDrive changed under a row (`docs/design/writes.md` §7), or a folder a row
    /// needs is gone there: the next cycle comes now, and its delta carries
    /// the change (the outbox on the bus: no Full reconcile).
    fn cycle_wanted(&self) {
        if let Some(service) = self.sync.upgrade() {
            service.nudge();
        }
    }

    /// An item's local object was forgotten (delete × edit, a folder deleted
    /// only in part): the next cycle comes now, with a Full reconcile, so
    /// that it is placed again though the delta may have carried it already.
    fn full_cycle_wanted(&self) {
        if let Some(service) = self.sync.upgrade() {
            service.nudge_full();
        }
    }

    /// The write gate, asked again before each row.
    fn may_write(&self) -> Result<(), String> {
        match self.sync.upgrade() {
            Some(service) => service.write_gate(),
            None => Err("the folder's sync is gone".into()),
        }
    }
}
