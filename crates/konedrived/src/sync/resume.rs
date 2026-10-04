use std::path::Path;

use crate::sync::SyncService;
use crate::helper::{Clearance, HelperError, HelperLink};
use crate::folder::root::SyncRoot;
use crate::sync::{Persisted, Registration, RootSource, SyncError};
use crate::status::snapshot::RootState;
use crate::folder::root;
use crate::sync::hub;

/// How a switch to interception ([`SyncService::upgrade`]) that did not go
/// through left the folder.
enum NotSwitched {
    /// As it was, without interception: the helper holds nothing of it. Why.
    Kept(String),
    /// Intercepted, waiting for the next connect: the helper may hold it.
    Held,
}

/// What `LastError` adds when a folder registered without the helper could
/// not be switched to interception once the helper connected.
const SWITCH_FAILED: &str =
    "the konedrive helper is connected, but switching this folder to interception failed";

impl SyncService {
    /// Brings the sync folder up, or back up: re-registers the root with the
    /// helper — which re-marks the whole tree a restarted helper has
    /// forgotten — and re-runs §4.4's recovery walk, or, when no root is
    /// registered yet, restores the one persisted at the last start.
    ///
    /// Called once at startup and again after every helper reconnect.
    /// Without the startup call, this walk
    /// never ran in the shipped
    /// daemon at all: it only ever ran inside a `RegisterRoot` D-Bus call,
    /// so after a crash, files left `hydrating`/`dehydrating` stayed that
    /// way until a human registered the folder again.
    ///
    /// The sign-in gate `RegisterRoot` applies is deliberately not applied
    /// here. This is not a new registration but the return of one made
    /// earlier, and the folder is full of placeholders either way: refusing
    /// to re-register it because a token has not been restored yet would
    /// leave those placeholders unmarked and uninterceptable, reading as
    /// zeros, which is worse than anything a signed-out daemon can be.
    ///
    /// # An intercepted root is held before the helper is back
    ///
    /// A restored intercepted root becomes this daemon's registration at
    /// once, link or no link ([`hold`](Self::hold)), before any call that
    /// changes the registration is decided — this one, or a registration or
    /// Forget that reaches a freshly started daemon first
    /// ([`restore_locked`](Self::restore_locked)) — and it stays so if
    /// bringing it up then fails. It used to exist nowhere until the helper
    /// came back and the bind succeeded: in between — the start of every
    /// session, and a D-Bus-activated first call in particular — the daemon
    /// held no root, `RootState` said `none`, and
    /// `RegisterWithoutInterception` of the very folder the helper still
    /// held, marks, ignore marks and all, was accepted. Measured in the VM
    /// suite: the next dehydration there punched a file that was still
    /// ignored, and it read 65536 zero bytes. Held, it answers what an
    /// intercepted root with its helper gone answers — "already registered"
    /// to a second registration, `NoHelper` to a Forget or a dehydration.
    pub async fn resume(&self) {
        let _lifecycle = self.lifecycle.write().await;
        if self.is_retiring() || self.held.lock().unwrap().is_some() {
            return;
        }
        self.restore_locked().await;
        match self.registration() {
            // Registered without interception: there is no helper
            // registration to renew. A folder registered that way because no
            // helper was connected switches to interception now that one is
            //, and its switch recovers it with this link; one
            // registered that way on purpose stays as it is — unless it shows
            // OneDrive, which is kept in step only with interception (HS2):
            // for such a folder no choice stands against the helper. A
            // recovery that had to leave files alone because a helper was
            // running with no link to it runs again now that
            // there is one.
            Some(reg) if !reg.intercepted => {
                let Some(link) = self.link() else { return };
                if reg.upgrade_when_helper || reg.source == RootSource::OneDrive {
                    self.upgrade(reg, link).await;
                } else if reg.recovery_deferred {
                    self.bring_up(&reg.root.path, false).await;
                }
            }
            // Nothing to register with yet; `supervise_helper` calls back
            // the moment there is.
            Some(_) if self.link().is_none() => {}
            Some(reg) => self.bring_up(&reg.root.path, true).await,
            None => {
                if let Some(persisted) = self.persisted_root().filter(|p| !p.intercepted) {
                    self.bring_up(&persisted.path, false).await;
                    let reg = self
                        .registration()
                        .filter(|r| !r.intercepted && (r.upgrade_when_helper || r.source == RootSource::OneDrive));
                    if let (Some(reg), Some(link)) = (reg, self.link()) {
                        self.upgrade(reg, link).await;
                    }
                }
            }
        }
        // What left the folder is marked again first (`docs/design/writes.md` §10), then the helper marked
        // nothing new while it was away (§3.3).
        self.outbox_helper_back();
        self.watcher_helper_back();
    }

    /// Holds the intercepted root `config.toml` records, if the daemon does
    /// not hold one yet — see [`resume`](Self::resume) — and publishes the
    /// path of one that is not intercepted. Called by `main`
    /// before the bus name is claimed, so that the first thing a client
    /// reads is the folder rather than `none`; quick, since nothing is asked
    /// of the helper and at most one xattr is read.
    pub async fn restore(&self) {
        let _lifecycle = self.lifecycle.write().await;
        self.restore_locked().await;
    }

    /// [`restore`](Self::restore), under a `lifecycle` lock the caller
    /// already holds. Every call that changes the registration runs this
    /// first, so none of them can be decided — "no root yet", say — before
    /// the root `config.toml` records has been looked at, whichever of them
    /// reaches a freshly started daemon first.
    pub(super) async fn restore_locked(&self) {
        if self.registration().is_some() || self.is_retiring() || self.held.lock().unwrap().is_some() {
            return;
        }
        match self.persisted_root() {
            Some(persisted) if persisted.intercepted => self.hold(persisted).await,
            // Not held: `resume` brings it up, after the bus name is claimed. Until then it
            // is published by its path alone, so that a client can tell a folder that is
            // not brought up yet from an account that has none.
            Some(persisted) => {
                let path = persisted.path.display().to_string();
                if self.state.get().root_path != path {
                    self.state.update(|s| s.root_path = path);
                }
            }
            None => {}
        }
    }

    /// Binds a root that is already this daemon's, publishing why when that
    /// fails. The root is left exactly as it was either way.
    async fn bring_up(&self, path: &Path, intercepted: bool) {
        if let Err(e) = self.bind(path, intercepted, false).await {
            let message = format!("cannot bring up the sync folder {}: {e}", path.display());
            tracing::error!("{message}");
            self.state.update(|s| {
                s.root_path = path.display().to_string();
                s.root_state = RootState::Error;
                s.last_error = message;
            });
        }
    }

    /// Switches a folder registered without interception because no helper
    /// was connected to interception, now that one is. Called by
    /// [`resume`](Self::resume), with `lifecycle` held for writing, so no
    /// registration, Forget, populate or free-up runs meanwhile.
    ///
    /// Found in real use: a folder registered before the helper
    /// was installed stayed without interception once it was, and every file
    /// in it read as zeros until a Forget and a new registration.
    ///
    /// # In this order
    ///
    /// 1. The folder's sync is stopped. A running sync decided at its start
    ///    that nothing is to be marked, and would go on placing directories
    ///    unmarked (invariant M1).
    /// 2. The switch is written down in `config.toml` before the helper hears
    ///    of the folder, as a fresh intercepted registration is: a crash
    ///    after that leaves a folder the next start holds as
    ///    intercepted and brings up at the helper's connect.
    /// 3. The helper registers the root. Its walk marks every directory in
    ///    it — the very walk that brings an intercepted folder back at every
    ///    restart, where content, too, was placed before the marks were. The
    ///    folder is then recovered with this link.
    /// 4. [`commit`](Self::commit) publishes it as intercepted — `ready`, and
    ///    the no-interception warning gone — and starts its sync again,
    ///    intercepted this time, so that everything it places from then on
    ///    is marked first.
    ///
    /// # When it fails (Ruling 2 of)
    ///
    /// The helper is asked to let go of anything it may have saved, and
    /// `config.toml` is put back: the folder stays exactly as it was, without
    /// interception, its sync running again, and `LastError` says why. The
    /// next connect tries again. The one exception is a helper that cannot
    /// confirm it let go: the folder is then kept intercepted, waiting for
    /// the next connect to bring it up ([`NotSwitched::Held`]).
    async fn upgrade(&self, reg: Registration, link: HelperLink) {
        let shown = reg.root.path.display().to_string();
        tracing::info!("the konedrive helper is connected: switching {shown} to interception");
        let was_syncing = self.stop_sync().await;
        match self.switch_to_interception(&reg, &link).await {
            Ok(()) => tracing::info!("{shown} is intercepted now"),
            Err(NotSwitched::Held) => {}
            Err(NotSwitched::Kept(why)) => {
                tracing::error!("switching {shown} to interception failed, so it stays without: {why}");
                if reg.recovery_deferred {
                    // What `resume` runs for such a folder instead; it starts
                    // the sync again as every bring-up does.
                    self.bring_up(&reg.root.path, false).await;
                } else if was_syncing {
                    self.start_sync().await;
                }
                self.note_switch_failed(&why);
            }
        }
    }

    /// Steps 2 to 4 of [`upgrade`](Self::upgrade).
    async fn switch_to_interception(&self, reg: &Registration, link: &HelperLink) -> Result<(), NotSwitched> {
        let (dir, root) =
            root::prepare(&reg.root.path).await.map_err(|e| NotSwitched::Kept(SyncError::from(e).to_string()))?;
        let previous = self.persisted_root();
        self.save_root(Some(&Persisted::of(&root, true, reg.source, reg.baloo_excluded, false)))
            .map_err(NotSwitched::Kept)?;
        let registered = link.register_root(&dir, &root.root_id).await.map_err(|e| e.to_string());
        drop(dir);
        let recovered = match registered {
            Ok(()) => {
                let clearance = Clearance::Link(link.clone());
                crate::hydration::recovery::recover(&clearance, &root, &self.locks).await.map_err(|e| e.to_string())
            }
            Err(why) => Err(why),
        };
        match recovered {
            Ok(report) => {
                self.commit(root, true, reg.source, false, false, report).await;
                Ok(())
            }
            Err(why) => Err(self.undo_switch(link, root, reg, previous, why).await),
        }
    }

    /// Undoes a switch that failed after `config.toml` recorded it — the way
    /// [`abandon`](Self::abandon) undoes a fresh registration: the helper is
    /// told to let go, and `config.toml` is put back. If the helper cannot
    /// confirm it let go, the folder is kept intercepted instead: a folder
    /// the helper may still hold must never be one the daemon
    /// holds without interception. Its sync stays stopped until it is
    /// brought up, at the next connect.
    async fn undo_switch(
        &self,
        link: &HelperLink,
        root: SyncRoot,
        reg: &Registration,
        previous: Option<Persisted>,
        why: String,
    ) -> NotSwitched {
        match link.unregister_root(&root.root_id).await {
            Ok(()) | Err(HelperError::Refused(libc::EPERM)) => {
                self.persist_or_log(previous.as_ref());
                NotSwitched::Kept(why)
            }
            Err(e) => {
                let message = format!(
                    "switching {} to interception failed ({why}), and the helper could not be told \
                     to let go of it ({e}); it is kept with interception, and brought up the next \
                     time the helper connects",
                    root.path.display()
                );
                tracing::error!("{message}");
                let dev = hub::device_of(&root.path).await;
                *self.root.lock().unwrap() = Some(Registration {
                    dev,
                    root,
                    intercepted: true,
                    recovery_deferred: false,
                    source: reg.source,
                    brought_up: false,
                    source_guessed: false,
                    baloo_excluded: reg.baloo_excluded,
                    upgrade_when_helper: false,
                });
                self.state.update(|s| {
                    s.root_state = RootState::Error;
                    s.last_error = message;
                });
                NotSwitched::Held
            }
        }
    }

    /// Adds why a switch failed to `LastError`, in place of what an earlier
    /// failed switch said there.
    fn note_switch_failed(&self, why: &str) {
        let note = format!("{SWITCH_FAILED}: {why}; it is tried again the next time the helper connects");
        self.state.update(|s| {
            let before = s.last_error.split(SWITCH_FAILED).next().unwrap_or_default();
            let before = before.trim_end_matches(". ");
            s.last_error = if before.is_empty() { note } else { format!("{before}. {note}") };
        });
    }

    /// Takes an intercepted root restored from `config.toml` as this
    /// daemon's registration, before anything is asked of the helper, and
    /// publishes it as waiting for the helper.
    ///
    /// Its id comes from `config.toml` — it is the name the helper holds the
    /// root by, and all a Forget needs even when the folder is gone — or,
    /// from a config written before the id was recorded, from the folder
    /// itself. With neither, the root is not held, and the failure is
    /// published as a startup failure always was; that leaves the one case
    /// lists.
    async fn hold(&self, persisted: Persisted) {
        // A `source` that cannot be read is said at once, helper or no helper; the folder
        // is held all the same, so that a Forget still reaches the helper (`SY6`).
        let unread = persisted
            .unread_source()
            .map(|why| format!("cannot bring up the sync folder {}: {why}", persisted.path.display()));
        let root_id = if root::looks_like_a_root_id(&persisted.root_id) {
            persisted.root_id
        } else if let Some(root_id) = root::recorded_root_id(&persisted.path).await {
            root_id
        } else {
            let message = format!(
                "cannot bring up the sync folder {}: config.toml does not record its root id, \
                 and the folder carries none that can be read",
                persisted.path.display()
            );
            tracing::error!("{message}");
            self.state.update(|s| {
                s.root_path = persisted.path.display().to_string();
                s.root_state = RootState::Error;
                s.last_error = message;
            });
            return;
        };
        let shown = persisted.path.display().to_string();
        let dev = hub::device_of(&persisted.path).await;
        *self.root.lock().unwrap() = Some(Registration {
            dev,
            root: SyncRoot { path: persisted.path, root_id },
            intercepted: true,
            recovery_deferred: false,
            source: persisted.source,
            brought_up: false,
            source_guessed: unread.is_some(),
            // Held, not yet brought up: a Forget with no link fails before it
            // reaches Baloo (`forget_locked`), and a bring-up asks `config.toml`
            // again (`commit`). What is left is a folder that stays held with
            // the helper connected — its `source` cannot be read — and its
            // Forget takes off the exclusion `config.toml` records.
            baloo_excluded: persisted.baloo_excluded,
            upgrade_when_helper: false,
        });
        self.state.update(|s| {
            s.root_path = shown;
            s.root_state = RootState::Error;
            s.last_error = unread.unwrap_or_default();
            s.waits_for_helper = true;
        });
    }

    /// Publishes the helper's disappearance: `RootState` used
    /// to stay `ready` with an empty `LastError` while the sync folder was,
    /// in the only sense that matters, dead — nothing intercepting, nothing
    /// reconnecting, and every un-hydrated file reading as zeros. Now it
    /// reads `error`, and `LastError` says what `HelperState` says (HS3):
    /// how to start the helper. The rest of what `LastError` said stays.
    pub fn report_helper_lost(&self) {
        let Some(reg) = self.registration() else {
            return;
        };
        if reg.intercepted || reg.source == RootSource::OneDrive {
            self.state.update(|s| s.waits_for_helper = true);
        }
    }
}
