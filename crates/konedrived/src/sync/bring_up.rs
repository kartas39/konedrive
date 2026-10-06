//! Bringing a folder up: a new registration (`Register`, `RegisterWithoutInterception`), the
//! folder `config.toml` records taken up at the start ([`SyncService::restore`]), a folder
//! brought up or back up when the helper connects ([`SyncService::resume`]), and the switch
//! of a folder registered without the helper to interception.
//!
//! Each is one change of the folder's state ([`SyncService::change`]): what `Folder` was,
//! what it is now, and the sync started that the new state needs.

use std::path::Path;
use std::sync::Arc;

use super::folder::{Content, Down, Kept, Folder, Interception, Is, OneDriveFolder, Record, Recovery, Standing, Stopped, Up};
use super::running_sync::{Sync, Why};
use super::persisted::{no_root_id, Persisted};
use super::{registry, RootSource, SyncError, SyncService};
use crate::account::state::SignInState;
use crate::folder::root::{self, SyncRoot};
use crate::helper::{Clearance, HelperError, HelperLink};
use crate::hydration::recovery::{self, RecoveryReport};
use crate::local::ScanReason;

/// Whether the helper sequence ([`SyncService::with_helper`]) is for a folder the daemon
/// does not hold with interception yet, or for one it does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Taking {
    /// A fresh registration, or a switch to interception: recorded in `config.toml` before
    /// the helper hears of it, and undone on both sides when it fails.
    New,
    /// A folder that is the daemon's already, in memory and on disk: a failure leaves it
    /// as it was.
    Again,
}

/// How the helper sequence failed.
enum NotTaken {
    /// The helper holds nothing of the folder, and `config.toml` is as it was.
    Undone(SyncError),
    /// The helper could not confirm it let go (`let_go`), so it may still hold `root`.
    Kept { root: SyncRoot, error: SyncError, let_go: HelperError },
}

/// The one sentence for a folder the helper may still hold after `what` ("registering …",
/// "switching … to interception") failed: `LastError`'s, and the refusal's.
fn kept_text(what: &str, error: &SyncError, let_go: &HelperError) -> String {
    format!(
        "{what} failed ({error}), and the helper could not be told to let go of it ({let_go}); it is kept \
         with interception and brought up the next time the helper connects; forget it if you do not want it"
    )
}

/// Refuses for an account that takes no folder now.
fn check_standing(folder: &Folder) -> Result<(), SyncError> {
    match &folder.standing {
        Standing::Active => Ok(()),
        Standing::HeldBack(why) => Err(SyncError::HeldBack(why.clone())),
        Standing::Retiring { .. } => Err(SyncError::Removing),
    }
}

/// §3.1's refusal on overlap, which holds whichever way a root is registered: an account
/// keeps exactly one folder, up or not.
fn check_absent(folder: &Folder) -> Result<(), SyncError> {
    match folder.is {
        Is::Absent => Ok(()),
        _ => Err(SyncError::AlreadyRegistered),
    }
}

impl SyncService {
    /// Binds an empty (or previously-registered) folder to the
    /// account, then runs startup recovery on it (recovery
    /// always runs *after* registration, on the same live helper link, so a
    /// file left `dehydrating` mid-`ClearIgnore` can still be cleaned up).
    ///
    /// Refused before anything is touched when nobody is signed in, or when
    /// a folder is already recorded (§3.1). Refused without a helper, too: no helper means
    /// no interception, and a placeholder nobody intercepts reads as zeros.
    /// [`register_root_without_interception`](Self::register_root_without_interception)
    /// is the explicit way to ask for that anyway.
    pub async fn register_root(&self, path: &Path) -> Result<(), SyncError> {
        // A folder that is up refuses at once: its sync is not stopped for a call that
        // changes nothing.
        if self.view().record.is_some() && self.view().down.is_none() {
            self.require_sign_in()?;
            return Err(SyncError::AlreadyRegistered);
        }
        let mut stopped = self.change().await;
        let registered = self.register_in(&mut stopped, path, true).await;
        if registered.is_err() {
            // A refusal leaves the folder as it was, its sync included: a bring-up that
            // ended while this call waited for the state may have started one.
            self.start_again(&mut stopped).await;
        }
        registered
    }

    /// A registration inside its change: refused before anything is touched, or made.
    async fn register_in(&self, stopped: &mut Stopped<'_>, path: &Path, intercepted: bool) -> Result<(), SyncError> {
        self.restore_in(stopped).await;
        check_standing(stopped.folder())?;
        if intercepted {
            self.require_sign_in()?;
        }
        check_absent(stopped.folder())?;
        let link = if intercepted { Some(self.require_link()?) } else { None };
        let _registering = self.wiring.registry.registering.lock().await;
        self.check_overlap(path).await?;
        self.register(stopped, path, link).await
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
    ///
    /// # Why this one does not ask whether anybody is signed in
    ///
    /// `RegisterRoot` does, because §3.1 binds a folder "to the signed-in
    /// drive". This method is the developer's, for a machine with no drive
    /// and no helper: the folder is driven from a local directory
    /// (`PopulateFromDirectory`) rather than from OneDrive. Nothing in this mode touches
    /// the account: the content comes from a directory the caller names.
    ///
    /// # Made with no helper connected, it switches when one connects
    ///
    /// The window calls this when `RegisterRoot` was refused for want of a
    /// helper ("Use Without the Helper"), and a helper installed later used
    /// to change nothing: the folder read as zeros until a Forget and a new
    /// registration. So a registration made with no helper connected is
    /// recorded as one to switch, and [`resume`](Self::resume) switches it
    /// to interception when the helper connects. Made with a
    /// helper connected, the mode was chosen with interception on offer, and
    /// it stays. (A folder that shows OneDrive left without interception by
    /// a daemon from before HS2 switches whatever it was recorded as.)
    pub async fn register_root_without_interception(&self, path: &Path) -> Result<(), SyncError> {
        if self.view().record.is_some() && self.view().down.is_none() {
            return Err(SyncError::AlreadyRegistered);
        }
        let mut stopped = self.change().await;
        let registered = self.register_in(&mut stopped, path, false).await;
        if registered.is_err() {
            self.start_again(&mut stopped).await;
        }
        registered
    }

    /// Holds this account's folder back (design §3.1): `config.toml` gives
    /// the account an id, a label, a drive or a folder an earlier account
    /// has. The folder is not brought up, `State` reads `error`,
    /// `LastError` says why, and a registration is refused the same way. The folder it
    /// records is held as recorded, for a Forget: one registered with interception in an
    /// earlier session is still the helper's until the helper lets go of it.
    pub async fn hold_back(&self, why: &str) {
        let message = format!("this account is held back: {why}; correct config.toml and start konedrived again");
        tracing::warn!("{message}");
        let mut stopped = self.change().await;
        stopped.folder_mut().standing = Standing::HeldBack(message);
        self.restore_in(&mut stopped).await;
    }

    /// Design §8.3: a folder that is, is inside, or contains another
    /// account's folder is refused, naming that account. Called with the
    /// registry's `registering` held, so that two accounts cannot both pass it.
    /// The helper would refuse an intercepted overlap anyway (`EINVAL`);
    /// checking first names the refusal, and covers a folder registered
    /// without interception, which the helper never sees.
    async fn check_overlap(&self, path: &Path) -> Result<(), SyncError> {
        match self.wiring.registry.overlapping(self.id(), path).await? {
            Some(label) => Err(SyncError::Overlaps(label)),
            None => Ok(()),
        }
    }

    /// The account's label, as `config.toml` has it — for a refusal that
    /// names it.
    pub(super) fn label(&self) -> String {
        let persist = &self.wiring.persist;
        persist.store.account(&persist.account).map(|account| account.label).unwrap_or_else(|| "another account".into())
    }

    /// §3.1: a root is bound to the signed-in drive, so there has to be one.
    fn require_sign_in(&self) -> Result<(), SyncError> {
        if self.wiring.account.snapshot().state != SignInState::SignedIn {
            return Err(SyncError::NotSignedIn);
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
        let signed_in = self.wiring.account.snapshot().state == SignInState::SignedIn;
        if signed_in && self.wiring.onedrive.is_some() && intercepted {
            RootSource::OneDrive
        } else {
            RootSource::Local
        }
    }

    /// A registration the daemon did not hold before this call: with interception over
    /// `link`, or without.
    ///
    /// # Nothing is committed until nothing can still fail
    ///
    /// The folder is up, and published, only once registration *and* recovery are done: a
    /// call that fails leaves no folder behind that would make every retry answer "already
    /// registered" — but for the one the helper may still hold (below).
    ///
    /// A per-file recovery failure does *not* fail the call — the root itself is
    /// registered and usable — but `State` reads `error` and `LastError` says what could
    /// not be fixed ([`Recovery`]).
    ///
    /// # Whatever the helper holds, the daemon holds (link 2)
    ///
    /// A folder the helper holds and the daemon does not is a folder the
    /// daemon will accept for `RegisterWithoutInterception` — and then
    /// free up files in with no `ClearIgnore`, while the helper still has
    /// them ignore-marked. So a registration that failed after the helper may have saved
    /// it, and that the helper cannot confirm it let go of, is kept ([`Down::Kept`]): the
    /// call is refused, the refusal says the folder is kept, and it comes up at the next
    /// connect unless it is forgotten first.
    ///
    /// A root registered without interception is never announced to the
    /// helper: not registered, not marked, not unregistered. What its recovery may still
    /// ask is `ClearIgnore`, by the same local rule every punch follows.
    async fn register(&self, stopped: &mut Stopped<'_>, path: &Path, link: Option<HelperLink>) -> Result<(), SyncError> {
        // A OneDrive folder remembers its account's drive (design §8.3):
        // one forgotten by another account is that account's files, and a
        // folder carrying a root id may be registered again without being
        // empty — so it is refused unless the drive is this account's, or
        // the folder is empty: then there is nothing to adopt, and the stale
        // drive comes off (Remove, then Add, on the same folder).
        match root::drive_of(path, self.account_drive()).await {
            root::DriveOf::Free => {}
            root::DriveOf::Stale(_) => root::forget_drive(path).await,
            root::DriveOf::Foreign(_) => return Err(SyncError::ForeignFolder),
        }
        let source = self.fresh_source(link.is_some());
        if source == RootSource::OneDrive {
            // A tree store left by a folder forgotten earlier describes
            // another folder.
            self.remove_tree_store().await;
        }
        let Some(link) = link else {
            // Made with no helper connected, it is the window's fallback, and switches to
            // interception when one connects; made with one connected, it is a choice,
            // and stays.
            let interception = Interception::Without { switch_when_helper: self.link().is_none() };
            let root = root::register_root_unprotected(path).await?;
            let report = self.recover(&self.clearance(), &root).await?;
            self.come_up(stopped, root, interception, source, true, report).await;
            return Ok(());
        };
        match self.with_helper(&link, path, source, false, Taking::New).await {
            Ok((root, report)) => {
                self.come_up(stopped, root, Interception::Intercepted, source, true, report).await;
                Ok(())
            }
            Err(NotTaken::Undone(error)) => Err(error),
            Err(NotTaken::Kept { root, error, let_go }) => {
                let why = kept_text(&format!("registering {}", root.path.display()), &error, &let_go);
                tracing::error!("{why}");
                let dev = registry::device_of(&root.path).await;
                // Nothing was added to Baloo for it.
                let record = Record { root, interception: Interception::Intercepted, source, baloo: false, dev, kept: Kept::default() };
                stopped.folder_mut().is = Is::Down(record, Down::Kept { why: why.clone() });
                Err(SyncError::Helper(why))
            }
        }
    }

    /// The helper's part of taking a folder with interception, written once: the folder
    /// prepared, recorded in `config.toml`, registered with the helper — whose walk marks
    /// every directory in it — and recovered on the same link; or, when that fails, the
    /// helper told to let go and `config.toml` put back.
    ///
    /// For a folder that is not the daemon's with interception yet ([`Taking::New`]):
    ///
    /// - it is written to `config.toml` **before** the helper is told, and is
    ///   refused outright if it cannot be. A crash anywhere after that — in
    ///   the helper's walk, in recovery's — leaves a root the next start
    ///   restores as intercepted and holds until the helper is back, which is
    ///   the direction to fail in. Baloo is not asked yet at that point (`baloo` is what
    ///   is known); the bring-up corrects the record the moment it knows;
    /// - it is undone *at the helper* when it fails after the helper may have
    ///   saved it. `EPERM` counts as having let go: the helper answers it when it holds
    ///   no root of this uid under that id, which is what a registration it
    ///   refused leaves behind. Anything but an answer — the link dropped, the call
    ///   timed out — is [`NotTaken::Kept`].
    ///
    /// A folder taken again ([`Taking::Again`]) is neither recorded nor undone.
    async fn with_helper(
        &self,
        link: &HelperLink,
        path: &Path,
        source: RootSource,
        baloo: bool,
        taking: Taking,
    ) -> Result<(SyncRoot, RecoveryReport), NotTaken> {
        let (dir, root) = root::prepare(path).await.map_err(|e| NotTaken::Undone(e.into()))?;
        let previous = match taking {
            Taking::New => {
                let previous = self.persisted_root();
                self.save_root(Some(&Persisted::of(&root, Interception::Intercepted, source, baloo))).map_err(NotTaken::Undone)?;
                Some(previous)
            }
            Taking::Again => None,
        };
        let registered = link.register_root(&dir, &root.root_id).await.map_err(|e| SyncError::Helper(e.to_string()));
        drop(dir);
        let outcome = match registered {
            Ok(()) => self.recover(&Clearance::Link(link.clone()), &root).await,
            Err(e) => Err(e),
        };
        let error = match outcome {
            Ok(report) => return Ok((root, report)),
            Err(error) => error,
        };
        let Some(previous) = previous else { return Err(NotTaken::Undone(error)) };
        match link.unregister_root(&root.root_id).await {
            Ok(()) | Err(HelperError::Refused(libc::EPERM)) => {
                self.persist_or_log(previous.as_ref());
                Err(NotTaken::Undone(error))
            }
            Err(let_go) => Err(NotTaken::Kept { root, error, let_go }),
        }
    }

    /// §4.4's recovery walk on `root`, or why it could not run at all.
    async fn recover(&self, clearance: &Clearance, root: &SyncRoot) -> Result<RecoveryReport, SyncError> {
        recovery::recover(clearance, root, &self.locks).await.map_err(|e| {
            tracing::error!("startup recovery on {}: {e}", root.path.display());
            SyncError::Io(e.to_string())
        })
    }

    /// A folder that has been registered and recovered comes up: it is kept out of Baloo,
    /// marked with its account's drive, recorded, and its sync started.
    async fn come_up(
        &self,
        stopped: &mut Stopped<'_>,
        root: SyncRoot,
        interception: Interception,
        source: RootSource,
        fresh: bool,
        report: RecoveryReport,
    ) {
        let recovery = Recovery::of(&report);
        let baloo = self.keep_out_of_baloo(&root, source, fresh).await;
        if source == RootSource::OneDrive {
            self.mark_drive(&root).await;
        }
        // `config.toml` names what is registered now: a fresh root without
        // interception is written down here (a fresh intercepted one already
        // was, before the helper heard of it), and a root recorded without its id
        // (an old `config.toml`) gets the id its folder carries.
        self.remember(&Persisted::of(&root, interception, source, baloo));
        let dev = registry::device_of(&root.path).await;
        // What is kept with the folder stays with it: what a local one was filled from by
        // hand, a OneDrive one's tree store and tree lock.
        let mut kept = stopped.folder().record().map(|record| record.kept.clone()).unwrap_or_default();
        if source == RootSource::OneDrive && kept.source.is_none() {
            // Files are downloaded from the drive whether or not the folder can be kept
            // in step, from its first bring-up on.
            kept.source = self.drive().map(|drive| self.wiring.sources.onedrive(drive));
        }
        let record = Record { root, interception, source, baloo, dev, kept };
        let content = match source {
            RootSource::Local => Content::Local,
            RootSource::OneDrive => Content::OneDrive(OneDriveFolder { sync: Sync::Stopped(Why::NotStarted), watcher_ended: None }),
        };
        stopped.folder_mut().is = Is::Up(Up { record, recovery, switch_failed: None, content });
        stopped.publish();
        // `LocalBytes` for the folder now registered.
        self.report.space.kick();
        self.run(stopped, ScanReason::Start).await;
    }

    /// Runs what a folder that is up needs: a OneDrive folder's sync; for any other, the
    /// pins' sweep at start. A OneDrive folder's sync sweeps after its first reconcile,
    /// which is Full; any other folder is swept here, in the background: the walk must not
    /// hold up the registration.
    async fn run(&self, stopped: &mut Stopped<'_>, reason: ScanReason) {
        if stopped.folder().syncs() {
            self.start_sync(stopped, reason).await;
        } else if let Some(up) = stopped.folder().up() {
            let (pins, root) = (Arc::clone(&self.pins), up.record.root.path.clone());
            tokio::spawn(async move {
                pins.sweep(root).await;
            });
        }
    }

    /// A OneDrive folder is kept out of Baloo, unless it — or
    /// a directory above it — is excluded already, in which case nothing
    /// is added and nothing this daemon did not add is ever taken off
    /// (a Forget). A folder brought back up that `config.toml`
    /// records as excluded by this daemon carries that forward, so a
    /// restart between a registration and its Forget still gets the
    /// Forget right. Any other is asked again at every bring-up, fresh or
    /// not: the exclusion of a fresh folder can
    /// have failed or timed out, been cut off by a kill before it was
    /// recorded, or been refused by a registration kept after it failed; or Baloo came
    /// later. It only ever adds. Whether this daemon is the one that excluded it.
    async fn keep_out_of_baloo(&self, root: &SyncRoot, source: RootSource, fresh: bool) -> bool {
        if source != RootSource::OneDrive {
            return false;
        }
        if !fresh && self.persisted_root().is_some_and(|p| p.baloo_excluded) {
            return true;
        }
        if self.wiring.baloo.is_excluded(&root.path).await {
            false
        } else {
            self.wiring.baloo.exclude(&root.path).await
        }
    }

    /// The folder remembers its account's drive once the drive is known
    /// (design §8.3): from its registration on, and at the first
    /// bring-up of a folder from before multiple accounts.
    async fn mark_drive(&self, root: &SyncRoot) {
        let Some(drive) = self.account_drive() else { return };
        let (marked, shown) = (root.clone(), root.path.display().to_string());
        match tokio::task::spawn_blocking(move || root::mark_drive(&marked, &drive)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!("cannot record the drive on {shown}: {e}"),
            Err(e) => tracing::warn!("the task recording the drive on {shown} failed: {e}"),
        }
    }

    /// Brings the sync folder up, or back up: re-registers the root with the
    /// helper — which re-marks the whole tree a restarted helper has
    /// forgotten — and re-runs §4.4's recovery walk; a folder registered without the
    /// helper is brought up without one, and switched to interception when one is
    /// connected.
    ///
    /// Called once at startup and again after every helper reconnect.
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
    /// A recorded intercepted root is this daemon's at once, link or no link
    /// ([`restore`](Self::restore)), before any call that
    /// changes the registration is decided, and it stays so if
    /// bringing it up then fails. It used to exist nowhere until the helper
    /// came back and the bind succeeded: in between, the daemon
    /// held no root, `State` said `none`, and
    /// `RegisterWithoutInterception` of the very folder the helper still
    /// held, marks, ignore marks and all, was accepted. Measured in the VM
    /// suite: the next dehydration there punched a file that was still
    /// ignored, and it read 65536 zero bytes. Held, it answers what an
    /// intercepted root with its helper gone answers — "already registered"
    /// to a second registration, `NoHelper` to a Forget or a dehydration.
    pub async fn resume(&self) {
        let mut stopped = self.change().await;
        if stopped.folder().standing != Standing::Active {
            return;
        }
        self.restore_in(&mut stopped).await;
        let link = self.link();
        // What the folder is decides what is asked of the helper.
        enum Step {
            Nothing,
            /// As it was: the sync the change stopped runs again.
            AsItWas,
            BringUp,
            Switch(HelperLink),
        }
        let step = match (&stopped.folder().is, link.clone()) {
            (Is::Absent, _) => Step::Nothing,
            // Registered without interception: there is no helper registration to renew.
            // One that switches does so now that a helper is connected, and its switch
            // recovers it with this link. A recovery that had to leave files alone
            // because a helper was running with no link to it runs again now that there
            // is one.
            (Is::Up(up), Some(link)) if up.record.switches_with_helper() => Step::Switch(link),
            (Is::Up(up), Some(_)) if !up.record.intercepted() && matches!(up.recovery, Recovery::Deferred(_)) => Step::BringUp,
            (Is::Up(up), _) if !up.record.intercepted() => Step::AsItWas,
            // Nothing to register with yet; the hub's supervisor calls back the moment
            // there is.
            (Is::Up(_), None) => Step::AsItWas,
            (Is::Up(_), Some(_)) => Step::BringUp,
            (Is::Down(record, _), None) if record.intercepted() => Step::Nothing,
            (Is::Down(..), _) => Step::BringUp,
        };
        match step {
            Step::Nothing => {}
            Step::AsItWas => self.start_again(&mut stopped).await,
            Step::Switch(link) => self.switch(&mut stopped, &link).await,
            Step::BringUp => {
                self.bring_up(&mut stopped).await;
                // Brought up without interception, with a helper connected: switched in
                // the same breath.
                if let (Some(link), Some(up)) = (link, stopped.folder().up()) {
                    if up.record.switches_with_helper() {
                        self.switch(&mut stopped, &link).await;
                    }
                }
            }
        }
        // What left the folder is marked again first (`docs/design/writes.md` §10), then the
        // helper marked nothing new while it was away (§3.3).
        self.outbox_helper_back();
        self.watcher_helper_back();
    }

    /// Takes up the folder `config.toml` records, if the daemon holds none yet: an
    /// intercepted one is held until the helper connects, one without interception until
    /// [`resume`](Self::resume) brings it up. Called by `main`
    /// before the bus name is claimed, so that the first thing a client
    /// reads is the folder rather than `none`; quick, since nothing is asked
    /// of the helper and at most one xattr is read.
    pub(crate) async fn restore(&self) {
        let mut stopped = self.change().await;
        self.restore_in(&mut stopped).await;
        drop(stopped);
        // The account's pause, kept in `config.toml`, is shown from the start, whether a
        // sync follows or not.
        self.show_pause();
    }

    /// [`restore`](Self::restore), inside a change. Every call that changes the
    /// registration runs this first, so none of them can be decided — "no root yet",
    /// say — before the root `config.toml` records has been looked at, whichever of them
    /// reaches a freshly started daemon first.
    pub(super) async fn restore_in(&self, stopped: &mut Stopped<'_>) {
        if !matches!(stopped.folder().is, Is::Absent) || matches!(stopped.folder().standing, Standing::Retiring { .. }) {
            return;
        }
        if let Some(persisted) = self.persisted_root() {
            let (record, down) = self.recorded(persisted).await;
            stopped.folder_mut().is = Is::Down(record, down);
        }
    }

    /// Brings the recorded folder up, or up again. When that fails the folder is down,
    /// and says why; it is tried again at the helper's next connect, at the next
    /// start, and by `Refresh()`.
    pub(super) async fn bring_up(&self, stopped: &mut Stopped<'_>) {
        let Some(record) = stopped.folder().record().cloned() else { return };
        let source = match self.source_brought_back(&stopped.folder().is) {
            Ok(source) => source,
            Err(down) => {
                tracing::error!("{}", down.why(&record.root));
                stopped.folder_mut().is = Is::Down(record, down);
                return;
            }
        };
        let path = record.root.path.clone();
        // The directory at the path must be the folder that is recorded, before anything
        // is stamped on it or said to the helper: a bring-up never adopts whatever stands
        // there now (an empty directory made where the folder was moved away from would be
        // stamped, registered and synced against the old tree store, and a read-write
        // folder's scan would then take every item for deleted here). It is the folder
        // when it carries the id recorded; a record with no usable id takes the id the
        // directory carries, and none is minted for it.
        let carried = root::recorded_root_id(&path).await;
        let stands = if root::looks_like_a_root_id(&record.root.root_id) {
            (carried.as_deref() != Some(record.root.root_id.as_str())).then(|| {
                format!(
                    "the sync folder is not at {} any more: it was moved or deleted, or another folder stands in \
                     its place; move the sync folder back, or forget it",
                    path.display()
                )
            })
        } else {
            carried.is_none().then(|| no_root_id(&path))
        };
        if let Some(why) = stands {
            tracing::error!("{why}");
            stopped.folder_mut().is = Is::Down(Record { source, ..record }, Down::Failed { why });
            return;
        }
        let taken = match (record.interception, self.link()) {
            (Interception::Without { .. }, _) => match root::register_root_unprotected(&path).await {
                Ok(root) => self.recover(&self.clearance(), &root).await.map(|report| (root, report)),
                Err(e) => Err(e.into()),
            },
            (Interception::Intercepted, Some(link)) => match self.with_helper(&link, &path, source, record.baloo, Taking::Again).await {
                Ok(taken) => Ok(taken),
                Err(NotTaken::Undone(error) | NotTaken::Kept { error, .. }) => Err(error),
            },
            (Interception::Intercepted, None) => Err(SyncError::NoHelper),
        };
        match taken {
            Ok((root, report)) => self.come_up(stopped, root, record.interception, source, false, report).await,
            Err(e) => {
                let why = format!("cannot bring up the sync folder {}: {e}", path.display());
                tracing::error!("{why}");
                stopped.folder_mut().is = Is::Down(Record { source, ..record }, Down::Failed { why });
            }
        }
    }

    /// What a folder brought back shows (`SY6`). One that is up, or recorded with a
    /// `source` that was read, keeps the one it has, whatever `config.toml` says by
    /// now. A folder held with a `source` that could not be read never comes up on the
    /// guess made for it: `config.toml` says — the file as it is now, read again, so that
    /// a word corrected while the daemon runs counts — and a word that is still neither
    /// `"onedrive"` nor `"local"`, a file that cannot be read now, or one that no longer
    /// records the folder, leaves it down.
    fn source_brought_back(&self, is: &Is) -> Result<RootSource, Down> {
        let Is::Down(record, Down::UnreadSource { written }) = is else {
            return Ok(is_record(is).map_or(RootSource::Local, |record| record.source));
        };
        match self.persisted_root_now() {
            Some(Some(now)) => match now.source_as_written {
                Some(written) => Err(Down::UnreadSource { written }),
                None => Ok(now.source),
            },
            Some(None) => {
                tracing::error!("config.toml no longer records {}; forget the folder and add it again", record.root.path.display());
                Err(Down::UnreadSource { written: written.clone() })
            }
            None => {
                tracing::error!("config.toml cannot be read now; the source of {} is read again at the next connect", record.root.path.display());
                Err(Down::UnreadSource { written: written.clone() })
            }
        }
    }

    /// Switches a folder registered without interception because no helper
    /// was connected to interception, now that one is. Called by
    /// [`resume`](Self::resume), inside its change, so no
    /// registration, Forget, populate or free-up runs meanwhile, and the folder's sync is
    /// stopped: a running sync decided at its start that nothing is to be marked, and
    /// would go on placing directories unmarked (invariant M1).
    ///
    /// Found in real use: a folder registered before the helper
    /// was installed stayed without interception once it was, and every file
    /// in it read as zeros until a Forget and a new registration.
    ///
    /// The switch is the helper sequence of a fresh registration
    /// ([`with_helper`](Self::with_helper)): written down in `config.toml` before the helper
    /// hears of the folder, registered, recovered with this link, and then up as
    /// intercepted — `ready`, the no-interception warning gone, and its sync started
    /// intercepted, so that everything it places from then on is marked first.
    ///
    /// # When it fails
    ///
    /// The helper is asked to let go of anything it may have saved, and
    /// `config.toml` is put back: the folder stays exactly as it was, without
    /// interception, its sync running again, and `LastError` says why. The
    /// next connect tries again. The one exception is a helper that cannot
    /// confirm it let go: the folder is then kept intercepted, down, waiting for
    /// the next connect to bring it up ([`Down::Kept`]).
    async fn switch(&self, stopped: &mut Stopped<'_>, link: &HelperLink) {
        let Some(up) = stopped.folder().up() else { return };
        let (record, deferred) = (up.record.clone(), matches!(up.recovery, Recovery::Deferred(_)));
        let shown = record.root.path.display().to_string();
        tracing::info!("the konedrive helper is connected: switching {shown} to interception");
        match self.with_helper(link, &record.root.path, record.source, record.baloo, Taking::New).await {
            Ok((root, report)) => {
                self.come_up(stopped, root, Interception::Intercepted, record.source, false, report).await;
                tracing::info!("{shown} is intercepted now");
            }
            Err(NotTaken::Kept { root, error, let_go }) => {
                let why = kept_text(&format!("switching {shown} to interception"), &error, &let_go);
                tracing::error!("{why}");
                let record = Record { root, interception: Interception::Intercepted, ..record };
                stopped.folder_mut().is = Is::Down(record, Down::Kept { why });
            }
            Err(NotTaken::Undone(error)) => {
                let why = error.to_string();
                tracing::error!("switching {shown} to interception failed, so it stays without: {why}");
                if deferred {
                    // What `resume` runs for such a folder instead; it starts the sync
                    // again as every bring-up does.
                    self.bring_up(stopped).await;
                } else {
                    self.start_again(stopped).await;
                }
                if let Is::Up(up) = &mut stopped.folder_mut().is {
                    up.switch_failed = Some(why);
                }
            }
        }
    }

    /// The sync the change stopped runs again, when the folder is still one that syncs.
    pub(super) async fn start_again(&self, stopped: &mut Stopped<'_>) {
        if stopped.ran() && stopped.folder().syncs() {
            self.start_sync(stopped, ScanReason::Start).await;
        }
    }
}

fn is_record(is: &Is) -> Option<&Record> {
    match is {
        Is::Absent => None,
        Is::Down(record, _) => Some(record),
        Is::Up(up) => Some(&up.record),
    }
}
