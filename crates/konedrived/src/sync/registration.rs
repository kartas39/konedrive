use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::sync::SyncService;
use crate::helper::{Clearance, HelperError, HelperLink};
use crate::hydration::recovery::{RecoveryError, RecoveryReport};
use crate::folder::root::{RegisterError, SyncRoot};
use crate::config::RootConfig;
use crate::account::state::SignInState;
use crate::sync::{NO_INTERCEPTION_WARNING, Persisted, Registration, RootSource, SyncError};
use crate::status::snapshot::RootState;
use crate::folder::root;
use crate::sync::hub;

impl SyncService {
    /// Binds an empty (or previously-registered) folder to the
    /// account, then runs startup recovery on it (recovery
    /// always runs *after* registration, on the same live helper link, so a
    /// file left `dehydrating` mid-`ClearIgnore` can still be cleaned up).
    ///
    /// Refused before anything is touched when nobody is signed in, or when
    /// a root is already registered (§3.1). The second of those
    /// used to be accepted: a second `register_root` returned `Ok(())` and
    /// silently replaced the root, leaving the first one registered with the
    /// helper — still marked, still walked — while `ItemState` started
    /// calling its files `not-managed`.
    ///
    /// Refused without a helper, too: no helper means no
    /// interception, and a placeholder nobody intercepts reads as zeros.
    /// [`register_root_without_interception`](Self::register_root_without_interception)
    /// is the explicit way to ask for that anyway.
    pub async fn register_root(&self, path: &Path) -> Result<(), SyncError> {
        let _lifecycle = self.lifecycle.write().await;
        self.restore_locked().await;
        self.check_held()?;
        self.require_sign_in()?;
        self.check_no_root_yet()?;
        self.require_link()?;
        let _registering = self.hub.registering.lock().await;
        self.check_overlap(path).await?;
        self.bind(path, true, true).await
    }

    /// The developer's mode (HS2): `RegisterRoot`'s folder
    /// checks, root id and placeholders, and nothing intercepting anything.
    /// The folder is always local — filled with `PopulateFromDirectory`,
    /// never from OneDrive, whoever is signed in: a OneDrive folder is kept in
    /// step only with the helper (HS2). Without the helper, a file that is
    /// not downloaded reads as zeros.
    ///
    /// A separate method rather than a second argument to `RegisterRoot`:
    /// D-Bus has no optional arguments, so a flag would change the signature
    /// of a method clients already call — and, more to the point, a separate
    /// name is what an introspection dump, a `busctl` transcript and a bug
    /// report all show. Nobody reaches this mode by fumbling a boolean, and
    /// nobody reaches it by accident when the helper merely happens to be
    /// down.
    /// # Why this one does not ask whether anybody is signed in
    ///
    /// `RegisterRoot` does, because §3.1 binds a folder "to the signed-in
    /// drive". This method is the developer's, for a machine with no drive
    /// and no helper: the folder is driven from a local directory
    /// (`PopulateFromDirectory`) rather than from OneDrive. Requiring a
    /// Microsoft sign-in here would put the one path that works without the
    /// cloud behind the cloud, which is the whole thing set out
    /// to unblock. Nothing in this mode touches the account: the content
    /// comes from a directory the caller names.
    ///
    /// # Made with no helper connected, it switches when one connects
    ///
    /// The window calls this when `RegisterRoot` was refused for want of a
    /// helper ("Use Without the Helper"), and a helper installed later used
    /// to change nothing: the folder read as zeros until a Forget and a new
    /// registration. So a registration made with no helper connected is
    /// recorded as one to switch, and [`resume`](Self::resume) switches it
    /// to interception when the helper connects (`upgrade`). Made with a
    /// helper connected, the mode was chosen with interception on offer, and
    /// it stays. (A folder that shows OneDrive left without interception by
    /// a daemon from before HS2 switches whatever it was recorded as.)
    pub async fn register_root_without_interception(
        &self,
        path: &Path,
    ) -> Result<(), SyncError> {
        let _lifecycle = self.lifecycle.write().await;
        self.restore_locked().await;
        self.check_held()?;
        self.check_no_root_yet()?;
        let _registering = self.hub.registering.lock().await;
        self.check_overlap(path).await?;
        self.bind(path, false, true).await
    }

    /// Holds this account's folder back (design §3.1): `config.toml` gives
    /// the account an id, a label, a drive or a folder an earlier account
    /// has. The folder is not brought up, `RootState` reads `error`,
    /// `LastError` says why, and a registration is refused the same way.
    pub fn hold_back(&self, why: &str) {
        let message = format!("this account is held back: {why}; correct config.toml and start konedrived again");
        tracing::warn!("{message}");
        let path = self.persisted_root().map(|p| p.path.display().to_string()).unwrap_or_default();
        *self.held.lock().unwrap() = Some(message.clone());
        self.state.update(|s| {
            s.root_path = path;
            s.root_state = RootState::Error;
            s.last_error = message;
        });
    }

    fn check_held(&self) -> Result<(), SyncError> {
        if self.is_retiring() {
            return Err(SyncError::Io("this account is being removed".into()));
        }
        match self.held.lock().unwrap().clone() {
            Some(why) => Err(SyncError::Io(why)),
            None => Ok(()),
        }
    }

    /// Says again why the account is held back, once its folder is
    /// forgotten: a Forget clears everything published about the folder.
    pub(super) fn publish_held(&self) {
        if let Some(message) = self.held.lock().unwrap().clone() {
            self.state.update(|s| {
                s.root_state = RootState::Error;
                s.last_error = message;
            });
        }
    }

    /// Design §8.3: a folder that is, is inside, or contains another
    /// account's folder is refused, naming that account. Called with the
    /// hub's `registering` held, so that two accounts cannot both pass it.
    /// The helper would refuse an intercepted overlap anyway (`EINVAL`);
    /// checking first names the refusal, and covers a folder registered
    /// without interception, which the helper never sees.
    async fn check_overlap(&self, path: &Path) -> Result<(), SyncError> {
        match self.hub.overlapping(self, path).await {
            Some(label) => Err(SyncError::Overlaps(label)),
            None => Ok(()),
        }
    }

    /// The account's label, as `config.toml` has it — for a refusal that
    /// names it.
    pub(super) fn label(&self) -> String {
        self.persist
            .as_ref()
            .and_then(|persist| persist.store.account(&persist.account))
            .map(|account| account.label)
            .unwrap_or_else(|| "another account".into())
    }

    /// Every folder this account holds or records: the registered one, and
    /// the one `config.toml` names (held, or not brought up yet).
    pub(super) fn folders(&self) -> Vec<PathBuf> {
        let mut folders: Vec<PathBuf> = self.registration().map(|reg| reg.root.path).into_iter().collect();
        folders.extend(self.persisted_root().map(|p| p.path));
        folders
    }

    /// §3.1: a root is bound to the signed-in drive, so there has to be one.
    fn require_sign_in(&self) -> Result<(), SyncError> {
        match &self.account {
            Some(account) if account.get().state != SignInState::SignedIn => {
                Err(SyncError::NotSignedIn)
            }
            _ => Ok(()),
        }
    }

    /// §3.1's refusal on overlap, which holds whichever way a root is
    /// registered: this daemon keeps exactly one.
    fn check_no_root_yet(&self) -> Result<(), SyncError> {
        if self.registration().is_some() {
            return Err(SyncError::AlreadyRegistered);
        }
        Ok(())
    }

    /// A folder registered while signed in, with a drive
    /// configured, and with interception, shows OneDrive; any other is
    /// local. Asked only of a new registration — a folder brought back keeps
    /// what `config.toml` records, whoever is signed in by then.
    ///
    /// "With interception" is HS2's: a registration without it is always
    /// the developer's local folder, filled with `PopulateFromDirectory`. A
    /// OneDrive folder nobody intercepts would read as zeros wherever a file
    /// is not downloaded, and is never made any more.
    fn fresh_source(&self, intercepted: bool) -> RootSource {
        let signed_in = self.account.as_ref().is_some_and(|a| a.get().state == SignInState::SignedIn);
        let configured = self.drive.lock().unwrap().is_some() && self.sync_paths.lock().unwrap().is_some();
        if signed_in && configured && intercepted {
            RootSource::OneDrive
        } else {
            RootSource::Local
        }
    }

    /// Registers `path`, recovers it, and publishes the result — the half
    /// shared by a `RegisterRoot` call, a `RegisterWithoutInterception`
    /// call, and a root brought back up at startup or after the helper
    /// reconnected. `fresh` is true for the first two: a registration the
    /// daemon did not hold before this call.
    ///
    /// # Nothing is committed until nothing can still fail
    ///
    /// The root is stored and published only once registration *and*
    /// recovery are done. The version this replaces stored the root and
    /// published `RootPath`/`RootState = ready` first and then returned
    /// `Err` on a `RecoveryError` — a call that failed, having already
    /// committed. With `RegisterRoot` now refusing a second root, that would
    /// be worse than untidy: the failed call would leave a root behind that
    /// makes every retry answer "already registered".
    ///
    /// A per-file recovery failure (`report.failed > 0`) does *not* fail
    /// this call — the root itself is registered and usable — but it does
    /// flip `RootState` to `error` and fill `LastError` with what could not
    /// be fixed: a refused `ClearIgnore` leaves a file in the state
    /// calls silently unrecoverable, so it must not stay silent here.
    ///
    /// # Whatever the helper holds, the daemon holds (link 2)
    ///
    /// A folder the helper holds and the daemon does not is a folder the
    /// daemon will accept for `RegisterWithoutInterception` — and then
    /// free up files in with no `ClearIgnore`, while the helper still has
    /// them ignore-marked. So a fresh intercepted registration:
    ///
    /// - is written to `config.toml` **before** the helper is told, and is
    ///   refused outright if it cannot be. A crash anywhere after that — in
    ///   the helper's walk, in recovery's — leaves a root the next start
    ///   restores as intercepted and holds until the helper is back, which is
    ///   the direction to fail in. The version this replaces wrote it last,
    ///   after recovery had walked the whole tree, and H70's own comment
    ///   notes that a helper timeout could already leave the helper holding
    ///   a root the daemon could not see;
    /// - is undone *at the helper* when it fails after the helper may have
    ///   saved it, and is kept instead when the helper cannot confirm it let
    ///   go ([`abandon`](Self::abandon)).
    ///
    /// A root brought back up is already the daemon's, in memory and on
    /// disk, and a failure leaves it exactly as it was.
    ///
    /// A root registered without interception is never announced to the
    /// helper: not registered, not marked, not
    /// unregistered. What its recovery may still ask is `ClearIgnore`, by the
    /// same local rule every punch follows.
    ///
    /// # What the folder shows
    ///
    /// A new registration shows OneDrive when it is made signed in with a
    /// drive configured ([`fresh_source`](Self::fresh_source)); a folder
    /// brought back shows what it showed before — the registration held, or
    /// else what `config.toml` records — whoever is signed in by then.
    pub(super) async fn bind(&self, path: &Path, intercepted: bool, fresh: bool) -> Result<(), SyncError> {
        // A OneDrive folder remembers its account's drive (design §8.3):
        // one forgotten by another account is that account's files, and a
        // folder carrying a root id may be registered again without being
        // empty — so it is refused unless the drive is this account's, or
        // the folder is empty: then there is nothing to adopt, and the stale
        // drive comes off (Remove, then Add, on the same folder).
        if fresh && !root::drive_allows(path, self.account_drive()).await {
            return Err(SyncError::ForeignFolder);
        }
        let source = if fresh { self.fresh_source(intercepted) } else { self.source_brought_back()? };
        if fresh && source == RootSource::OneDrive {
            // A tree store left by a folder forgotten earlier describes
            // another folder.
            self.remove_tree_store().await;
        }

        if !intercepted {
            // A new registration without interception made with no
            // helper connected is the window's fallback, and switches to
            // interception when one connects; made with one connected, it is
            // a choice, and stays. A folder brought back keeps what it had.
            let upgrade_when_helper = if fresh {
                self.link().is_none()
            } else {
                self.registration()
                    .map(|reg| reg.upgrade_when_helper)
                    .or_else(|| self.persisted_root().map(|p| p.upgrade_when_helper))
                    .unwrap_or(false)
            };
            let root = root::register_root_unprotected(path).await?;
            let recovery = crate::hydration::recovery::recover(&self.clearance(), &root, &self.locks).await;
            let report = self.recovered(&root, recovery)?;
            self.commit(root, false, source, fresh, upgrade_when_helper, report).await;
            return Ok(());
        }

        let link = self.require_link()?;
        let (dir, root) = root::prepare(path).await?;
        let previous = if fresh {
            let previous = self.persisted_root();
            // Baloo is not checked yet at this point (it runs, at most, once
            // `commit` below has recovery's word that the registration
            // stuck); `commit`'s own `remember` corrects this the moment it
            // knows.
            self.save_root(Some(&Persisted::of(&root, true, source, false, false))).map_err(SyncError::Io)?;
            Some(previous)
        } else {
            None
        };
        let registered = link
            .register_root(&dir, &root.root_id)
            .await
            .map_err(|e| SyncError::from(RegisterError::Helper(e.to_string())));
        drop(dir);
        let outcome = match registered {
            Ok(()) => {
                let clearance = Clearance::Link(link.clone());
                self.recovered(&root, crate::hydration::recovery::recover(&clearance, &root, &self.locks).await)
            }
            Err(e) => Err(e),
        };
        match outcome {
            Ok(report) => {
                self.commit(root, true, source, fresh, false, report).await;
                Ok(())
            }
            Err(error) => {
                if let Some(previous) = previous {
                    self.abandon(&link, root, source, previous, &error).await;
                }
                Err(error)
            }
        }
    }

    /// What a folder brought back shows (`SY6`). One that is up, or held with a
    /// `source` that was read, keeps the one it has, whatever `config.toml` says by
    /// now. Otherwise `config.toml` says — the file as it is now, read again, so that
    /// a word corrected while the daemon runs counts — and a word that is neither
    /// `"onedrive"` nor `"local"` refuses the bring-up: it is not taken for a local
    /// folder. A folder held with a guess for such a word never comes up on the
    /// guess: a file that cannot be read now, or no longer records it, refuses too.
    /// Every refusal is tried again at the helper's next connect and at the next
    /// start (`resume`).
    fn source_brought_back(&self) -> Result<RootSource, SyncError> {
        let recorded = match (self.registration(), self.persisted_root_now()) {
            (Some(reg), _) if !reg.source_guessed => return Ok(reg.source),
            (Some(_), None) => {
                return Err(SyncError::Io(
                    "its source in config.toml was neither \"onedrive\" nor \"local\", and config.toml cannot \
                     be read now; it is read again when the helper next connects, or konedrive starts"
                        .into(),
                ))
            }
            (Some(reg), Some(None)) => {
                return Err(SyncError::Io(format!(
                    "config.toml no longer records {}; forget the folder and add it again",
                    reg.root.path.display()
                )))
            }
            (Some(_), Some(now)) => now,
            // Not held: the file as it is now, or else as the daemon last read it.
            (None, now) => now.flatten().or_else(|| self.persisted_root()),
        };
        match recorded {
            Some(persisted) => match persisted.unread_source() {
                Some(why) => Err(SyncError::Io(why)),
                None => Ok(persisted.source),
            },
            None => Ok(RootSource::Local),
        }
    }

    /// Recovery's report, or — when recovery could not run at all — its
    /// error, published before it is returned.
    fn recovered(
        &self,
        root: &SyncRoot,
        recovery: Result<RecoveryReport, RecoveryError>,
    ) -> Result<RecoveryReport, SyncError> {
        recovery.map_err(|e| {
            let message = e.to_string();
            tracing::error!("startup recovery on {}: {message}", root.path.display());
            self.state.update(|s| {
                s.root_state = RootState::Error;
                s.last_error = message.clone();
            });
            SyncError::Io(message)
        })
    }

    /// Stores, records and publishes a registration that has been made and
    /// recovered, and starts — or nudges — a OneDrive folder's sync.
    pub(super) async fn commit(
        &self,
        root: SyncRoot,
        intercepted: bool,
        source: RootSource,
        fresh: bool,
        upgrade_when_helper: bool,
        report: RecoveryReport,
    ) {
        let mut trouble = None;
        if report.failed > 0 {
            trouble = Some(format!(
                "startup recovery could not reset {} of {} managed file(s); they are left \
                 exactly as found for the next start (reset {}, skipped {})",
                report.failed, report.scanned, report.reset, report.skipped
            ));
            tracing::error!("{}", trouble.as_deref().unwrap_or_default());
        } else if report.skipped > 0 {
            trouble = Some(format!(
                "startup recovery could not inspect {} item(s); the root may not be fully \
                 recovered",
                report.skipped
            ));
            tracing::warn!("{}", trouble.as_deref().unwrap_or_default());
        } else if report.busy > 0 {
            // Not an error: what has such a file
            // open is, as often as not, the very open that will fill it. So
            // it is logged and not said in `LastError` (B-M11): said there,
            // it stayed for the whole session, long after the file was
            // filled.
            tracing::info!(
                "startup recovery left {} interrupted file(s) as they were because they were in \
                 use; each is filled when it is next opened, or reset at the next start",
                report.busy
            );
        } else if report.deferred > 0 {
            // Not an error either: recovery runs again the
            // moment the link is up.
            trouble = Some(format!(
                "startup recovery left {} interrupted file(s) as they were: a konedrive helper \
                 is running and this daemon is not connected to it yet; they are reset once it is",
                report.deferred
            ));
            tracing::info!("{}", trouble.as_deref().unwrap_or_default());
        }

        // A OneDrive folder is kept out of Baloo, unless it — or
        // a directory above it — is excluded already, in which case nothing
        // is added and nothing this daemon did not add is ever taken off
        // (`unregister_root`). A folder brought back up that `config.toml`
        // records as excluded by this daemon carries that forward, so a
        // restart between a registration and its Forget still gets the
        // Forget right. Any other is asked again at every commit, fresh or
        // not: the exclusion of a fresh folder can
        // have failed or timed out, been cut off by a kill before it was
        // recorded, or been refused by a registration kept after it failed
        // (`abandon`); or Baloo came later. It only ever adds.
        let baloo_excluded = if source != RootSource::OneDrive {
            false
        } else if !fresh && self.persisted_root().is_some_and(|p| p.baloo_excluded) {
            true
        } else {
            let baloo = Arc::clone(&self.baloo.lock().unwrap());
            if baloo.is_excluded(&root.path).await {
                false
            } else {
                baloo.exclude(&root.path).await
            }
        };

        // The folder remembers its account's drive once the drive is known
        // (design §8.3): from its registration on, and at the first
        // bring-up of a folder from before multiple accounts.
        if source == RootSource::OneDrive {
            if let Some(drive) = self.account_drive() {
                let (marked, shown) = (root.clone(), root.path.display().to_string());
                match tokio::task::spawn_blocking(move || root::mark_drive(&marked, &drive)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::warn!("cannot record the drive on {shown}: {e}"),
                    Err(e) => tracing::warn!("the task recording the drive on {shown} failed: {e}"),
                }
            }
        }

        // `config.toml` names what is registered now: a fresh root without
        // interception is written down here (a fresh intercepted one already
        // was, before the helper heard of it), and a root brought back up
        // under an id other than the recorded one is corrected.
        self.remember(&Persisted::of(&root, intercepted, source, baloo_excluded, upgrade_when_helper));
        let path = root.path.display().to_string();
        let recovery_deferred = report.deferred > 0;
        let dev = hub::device_of(&root.path).await;
        *self.root.lock().unwrap() = Some(Registration {
            root,
            intercepted,
            recovery_deferred,
            source,
            brought_up: true,
            source_guessed: false,
            baloo_excluded,
            upgrade_when_helper,
            dev,
        });
        self.state.update(|s| {
            s.root_path = path;
            // A recovery that could not reset a file outranks the mode in
            // `RootState`, because it is the louder of the two problems;
            // `LastError` below still carries the no-interception warning.
            s.root_state = if report.failed > 0 {
                RootState::Error
            } else if intercepted {
                RootState::Ready
            } else {
                RootState::NoInterception
            };
            s.last_error = match (intercepted, &trouble) {
                (true, None) => String::new(),
                (true, Some(trouble)) => trouble.clone(),
                (false, None) => NO_INTERCEPTION_WARNING.to_owned(),
                (false, Some(trouble)) => format!("{NO_INTERCEPTION_WARNING}. {trouble}"),
            };
            // HS2: a folder that shows OneDrive is kept in step only with
            // interception; one registered without it before HS waits for
            // the helper, and switches when it connects (`resume`).
            s.waits_for_helper = !intercepted && source == RootSource::OneDrive;
        });
        // `LocalBytes` for the folder now registered.
        self.report.space.kick();
        if source == RootSource::OneDrive && intercepted {
            self.start_sync().await;
        } else {
            // The sweep at start. A OneDrive folder's sync sweeps after its
            // first reconcile, which is Full; any other folder is swept here,
            // in the background: the walk must not hold up the registration.
            let (pins, root) = (Arc::clone(&self.pins), PathBuf::from(&self.state.get().root_path));
            tokio::spawn(async move {
                pins.sweep(root).await;
            });
        }
    }

    /// Undoes a fresh intercepted registration that failed after the helper
    /// may have saved it: the helper is told to let go, and `config.toml` is
    /// put back the way it was (on both sides).
    ///
    /// If the helper cannot confirm it let go — the link dropped, the call
    /// timed out, anything but an answer — the root is kept instead, as
    /// intercepted and published as an error. A folder the helper may still
    /// hold must never be one the daemon holds nothing of: the next thing it
    /// would accept for that folder is a registration without interception.
    /// It leaves the way every intercepted root leaves, through the helper
    ///, and a retry is answered "already registered" until it
    /// has.
    ///
    /// `EPERM` counts as having let go: the helper answers it when it holds
    /// no root of this uid under that id, which is what a registration it
    /// refused leaves behind.
    ///
    /// A root kept is kept with the `source` `config.toml` now records for
    /// it, and with no sync: that starts when the root is brought up.
    async fn abandon(
        &self,
        link: &HelperLink,
        root: SyncRoot,
        source: RootSource,
        previous: Option<Persisted>,
        why: &SyncError,
    ) {
        match link.unregister_root(&root.root_id).await {
            Ok(()) | Err(HelperError::Refused(libc::EPERM)) => {
                self.persist_or_log(previous.as_ref());
            }
            Err(e) => {
                let message = format!(
                    "registering {} failed ({why}), and the helper could not be told to let go \
                     of it ({e}); it stays registered, with interception, until a Forget \
                     reaches the helper",
                    root.path.display()
                );
                tracing::error!("{message}");
                let path = root.path.display().to_string();
                let dev = hub::device_of(&root.path).await;
                *self.root.lock().unwrap() = Some(Registration {
                    root,
                    intercepted: true,
                    recovery_deferred: false,
                    source,
                    brought_up: false,
                    source_guessed: false,
                    baloo_excluded: false,
                    upgrade_when_helper: false,
                    dev,
                });
                self.state.update(|s| {
                    s.root_path = path;
                    s.root_state = RootState::Error;
                    s.last_error = message;
                });
            }
        }
    }

    /// The root is "persisted, so it survives a restart" — with its
    /// mode, and with the id the helper holds it by — as the account's
    /// `[accounts.root]`, through the one `ConfigStore`: every write re-reads
    /// the file, so nothing else in it is lost. `Err` when the file could not
    /// be written — or could not be read: what could not be read is never
    /// overwritten. The account's drive stays: it is the account's, not the
    /// folder's (design §8.1).
    pub(super) fn save_root(&self, root: Option<&Persisted>) -> Result<(), String> {
        let Some(persist) = &self.persist else {
            return Ok(());
        };
        let root = root.map(|root| RootConfig {
            path: root.path.clone(),
            id: root.root_id.clone(),
            intercepted: root.intercepted,
            source: root.source_as_written.clone().unwrap_or_else(|| root.source.as_str().into()),
            baloo_excluded: root.baloo_excluded,
            upgrade_when_helper: Some(root.upgrade_when_helper),
        });
        persist.store.set_root(&persist.account, root).map_err(|e| {
            format!("cannot record the sync folder in {}: {e}", persist.store.file().display())
        })
    }

    /// [`save_root`](Self::save_root) where a failure cannot be undone
    /// anyway. The registration itself stands — the root is bound and
    /// usable right now, or forgotten — but the next start will not know,
    /// and §4.4's recovery walk is what the next start owes this folder.
    pub(super) fn persist_or_log(&self, root: Option<&Persisted>) {
        if let Err(e) = self.save_root(root) {
            tracing::error!("{e}");
        }
    }

    /// [`persist_or_log`](Self::persist_or_log), only when `config.toml`
    /// does not already say exactly this.
    fn remember(&self, root: &Persisted) {
        if self.persist.is_some() && self.persisted_root().as_ref() != Some(root) {
            self.persist_or_log(Some(root));
        }
    }

    pub(super) fn persisted_root(&self) -> Option<Persisted> {
        let persist = self.persist.as_ref()?;
        Some(Persisted::read(persist.store.account(&persist.account)?.root?))
    }

    /// [`persisted_root`](Self::persisted_root) from `config.toml` as it is now, read
    /// again: a hand edit made while the daemon runs counts. `None` when the file cannot
    /// be read now (caught half-saved, say); `Some(None)` when it records no folder.
    fn persisted_root_now(&self) -> Option<Option<Persisted>> {
        let persist = self.persist.as_ref()?;
        let config = persist.store.current()?;
        Some(config.account(&persist.account).and_then(|account| account.root.clone()).map(Persisted::read))
    }
}
