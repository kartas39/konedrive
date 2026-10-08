//! Bringing a folder up: a new registration (`Register`), the folder `config.toml` records
//! taken up at the start ([`SyncService::restore`]), and a folder brought up, or back up
//! ([`SyncService::resume`]).
//!
//! Each is one change of the folder's state ([`SyncService::change`]): what `Folder` was,
//! what it is now, and the sync started that the new state needs. A folder that is up
//! shows OneDrive through its mount, which its first sync makes (`start_stop`).

use std::path::Path;

use super::folder::{Down, Folder, Is, Record, Standing, Stopped, Up};
use super::persisted::{no_root_id, Persisted};
use super::running_sync::{Sync, Why};
use super::{SyncError, SyncService};
use crate::account::state::SignInState;
use crate::folder::root::{self, SyncRoot};

/// Refuses for an account that takes no folder now.
fn check_standing(folder: &Folder) -> Result<(), SyncError> {
    match &folder.standing {
        Standing::Active => Ok(()),
        Standing::HeldBack(why) => Err(SyncError::HeldBack(why.clone())),
        Standing::Retiring { .. } => Err(SyncError::Removing),
    }
}

/// `docs/design/hydration.md` §14.1's refusal on overlap: an account keeps exactly one folder, up or not.
fn check_absent(folder: &Folder) -> Result<(), SyncError> {
    match folder.is {
        Is::Absent => Ok(()),
        _ => Err(SyncError::AlreadyRegistered),
    }
}

impl SyncService {
    /// Binds an empty (or previously-registered) folder to the account, and brings it up.
    ///
    /// Refused before anything is touched when nobody is signed in, when the daemon was
    /// given no drive to show, or when a folder is already recorded (§14.1).
    pub async fn register_root(&self, path: &Path) -> Result<(), SyncError> {
        // A folder that is up refuses at once: its sync is not stopped for a call that
        // changes nothing.
        if self.view().record.is_some() && self.view().down.is_none() {
            self.require_sign_in()?;
            return Err(SyncError::AlreadyRegistered);
        }
        let mut stopped = self.change().await;
        let registered = self.register_in(&mut stopped, path).await;
        if registered.is_err() {
            // A refusal leaves the folder as it was, its sync included: a bring-up that
            // ended while this call waited for the state may have started one.
            self.start_again(&mut stopped).await;
        }
        registered
    }

    /// A registration inside its change: refused before anything is touched, or made.
    async fn register_in(&self, stopped: &mut Stopped<'_>, path: &Path) -> Result<(), SyncError> {
        self.restore_in(stopped).await;
        check_standing(stopped.folder())?;
        self.require_sign_in()?;
        check_absent(stopped.folder())?;
        if self.wiring.onedrive.is_none() {
            return Err(SyncError::Unsupported("no drive is configured for this account, so there is nothing to show in a folder".into()));
        }
        let _registering = self.wiring.registry.registering.lock().await;
        self.check_overlap(path).await?;
        self.register(stopped, path).await
    }

    /// `PopulateFromDirectory`: refused. It filled a folder registered without interception
    /// with placeholders of a local directory's files; there is no such folder any more.
    pub async fn populate_from_directory(&self, _source: &Path) -> Result<u64, SyncError> {
        Err(SyncError::Unsupported("a folder cannot be filled from a directory any more: every folder shows OneDrive through its mount".into()))
    }

    /// `RegisterWithoutInterception`: refused. It made a folder filled by hand from a local
    /// directory, whose files were placeholders; every folder shows OneDrive through its
    /// mount now, and `Register` is the one way to make one.
    pub async fn register_root_without_interception(&self, _path: &Path) -> Result<(), SyncError> {
        Err(SyncError::Unsupported(
            "a folder cannot be registered without interception any more: every folder shows OneDrive through its mount; register it as any other".into(),
        ))
    }

    /// Holds this account's folder back (`docs/design/accounts.md` §4.1): `config.toml` gives
    /// the account an id, a label, a drive or a folder an earlier account
    /// has. The folder is not brought up, `State` reads `error`,
    /// `LastError` says why, and a registration is refused the same way. The folder it
    /// records is held as recorded, for a Forget.
    pub async fn hold_back(&self, why: &str) {
        let message = format!("this account is held back: {why}; correct config.toml and start konedrived again");
        tracing::warn!("{message}");
        let mut stopped = self.change().await;
        stopped.folder_mut().standing = Standing::HeldBack(message);
        self.restore_in(&mut stopped).await;
    }

    /// `docs/design/accounts.md` §6.3: a folder that is, is inside, or contains another
    /// account's folder is refused, naming that account. Called with the
    /// registry's `registering` held, so that two accounts cannot both pass it.
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

    /// `docs/design/hydration.md` §14.1: a root is bound to the signed-in drive, so there has to be one.
    fn require_sign_in(&self) -> Result<(), SyncError> {
        if self.wiring.account.snapshot().state != SignInState::SignedIn {
            return Err(SyncError::NotSignedIn);
        }
        Ok(())
    }

    /// A registration the daemon did not hold before this call.
    ///
    /// The folder is up, and published, only once it is registered: a call that fails
    /// leaves no folder behind that would make every retry answer "already registered".
    async fn register(&self, stopped: &mut Stopped<'_>, path: &Path) -> Result<(), SyncError> {
        // A OneDrive folder remembers its account's drive (`docs/design/accounts.md` §6.3):
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
        // A tree store left by a folder forgotten earlier describes another folder.
        self.remove_tree_store().await;
        let root = root::register_root(path).await?;
        self.come_up(stopped, root, true).await;
        Ok(())
    }

    /// A folder that has been registered comes up: it is kept out of Baloo, marked with
    /// its account's drive, recorded, and its sync started.
    async fn come_up(&self, stopped: &mut Stopped<'_>, root: SyncRoot, fresh: bool) {
        let baloo = self.keep_out_of_baloo(&root, fresh).await;
        // Through a mount that is up the folder takes no attribute, and has its drive already.
        if stopped.folder().mount().is_none() {
            self.mark_drive(&root).await;
        }
        // `config.toml` names what is registered now, and a root recorded without its id
        // (an old `config.toml`) gets the id its folder carries.
        self.remember(&Persisted::of(&root, baloo));
        // What is kept with the folder stays with it: its tree store, its tree lock and
        // its mount.
        let mut kept = stopped.folder().record().map(|record| record.kept.clone()).unwrap_or_default();
        if kept.source.is_none() {
            // Files are downloaded from the drive from the folder's first bring-up on.
            kept.source = self.drive().map(|drive| self.wiring.sources.onedrive(drive));
        }
        let record = Record { root, baloo, kept };
        stopped.folder_mut().is = Is::Up(Up { record, sync: Sync::Stopped(Why::NotStarted) });
        stopped.publish();
        // `LocalBytes` for the folder now registered.
        self.report.space.kick();
        self.start_sync(stopped).await;
    }

    /// A OneDrive folder is kept out of Baloo, unless it — or
    /// a directory above it — is excluded already, in which case nothing
    /// is added and nothing this daemon did not add is ever taken off
    /// (a Forget). A folder brought back up that `config.toml`
    /// records as excluded by this daemon carries that forward, so a
    /// restart between a registration and its Forget still gets the
    /// Forget right. Any other is asked again at every bring-up, fresh or
    /// not: the exclusion of a fresh folder can
    /// have failed or timed out, or been cut off by a kill before it was
    /// recorded; or Baloo came later. It only ever adds. Whether this daemon is the one
    /// that excluded it.
    async fn keep_out_of_baloo(&self, root: &SyncRoot, fresh: bool) -> bool {
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
    /// (`docs/design/accounts.md` §6.3): from its registration on, and at the first
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

    /// Brings the sync folder up, or back up: a folder that is down is brought up, and
    /// the sync of one that is up, which the change stopped, runs again.
    ///
    /// Called once at startup, for every account in turn.
    ///
    /// The sign-in gate `RegisterRoot` applies is deliberately not applied
    /// here. This is not a new registration but the return of one made
    /// earlier: the folder is mounted and shows its names whoever is signed in, and its
    /// sync says what it lacks.
    pub async fn resume(&self) {
        let mut stopped = self.change().await;
        if stopped.folder().standing != Standing::Active {
            return;
        }
        self.restore_in(&mut stopped).await;
        match &stopped.folder().is {
            Is::Absent => {}
            Is::Up(_) => self.start_again(&mut stopped).await,
            Is::Down(..) => self.bring_up(&mut stopped).await,
        }
    }

    /// Takes up the folder `config.toml` records, if the daemon holds none yet: it is held,
    /// down, until [`resume`](Self::resume) brings it up. Called by `main`
    /// before the bus name is claimed, so that the first thing a client
    /// reads is the folder rather than `none`; quick, since at most one xattr is read.
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
    /// and says why; it is tried again at the next start, and by `Refresh()`.
    pub(super) async fn bring_up(&self, stopped: &mut Stopped<'_>) {
        let Some(record) = stopped.folder().record().cloned() else { return };
        if let Err(down) = self.source_brought_back(&stopped.folder().is) {
            tracing::error!("{}", down.why(&record.root));
            stopped.folder_mut().is = Is::Down(record, down);
            return;
        }
        let path = record.root.path.clone();
        if record.kept.mount.is_some() {
            // Mounted already: the folder is what it was, and its path is the mount.
            return self.come_up(stopped, record.root.clone(), false).await;
        }
        // What a daemon that died left mounted goes before the folder is looked at.
        let stale = path.clone();
        let _ = tokio::task::spawn_blocking(move || crate::fuse::clear_stale(&stale)).await;
        // The directory at the path must be the folder that is recorded, before anything
        // is stamped on it or mounted over it: a bring-up never adopts whatever stands
        // there now (an empty directory made where the folder was moved away from would be
        // stamped, mounted and synced against the old tree store). It is the folder
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
            stopped.folder_mut().is = Is::Down(record, Down::Failed { why });
            return;
        }
        match root::register_root(&path).await {
            Ok(root) => self.come_up(stopped, root, false).await,
            Err(e) => {
                let why = format!("cannot bring up the sync folder {}: {}", path.display(), SyncError::from(e));
                tracing::error!("{why}");
                stopped.folder_mut().is = Is::Down(record, Down::Failed { why });
            }
        }
    }

    /// Whether a folder brought back shows OneDrive (`SY6`). One that is up, or recorded
    /// with a `source` that was read, does. A folder held with a `source` that could not
    /// be read never comes up on a guess: `config.toml` says — the file as it is now, read
    /// again, so that a word corrected while the daemon runs counts — and a word that is
    /// still not `"onedrive"`, a file that cannot be read now, or one that no longer
    /// records the folder, leaves it down.
    fn source_brought_back(&self, is: &Is) -> Result<(), Down> {
        let Is::Down(record, Down::UnreadSource { written }) = is else { return Ok(()) };
        match self.persisted_root_now() {
            Some(Some(now)) => match now.source_as_written {
                Some(written) => Err(Down::UnreadSource { written }),
                None => Ok(()),
            },
            Some(None) => {
                tracing::error!("config.toml no longer records {}; forget the folder and add it again", record.root.path.display());
                Err(Down::UnreadSource { written: written.clone() })
            }
            None => {
                tracing::error!("config.toml cannot be read now; the source of {} is read again at the next start", record.root.path.display());
                Err(Down::UnreadSource { written: written.clone() })
            }
        }
    }

    /// The sync the change stopped runs again, when the folder is still up.
    pub(super) async fn start_again(&self, stopped: &mut Stopped<'_>) {
        if stopped.ran() {
            self.start_sync(stopped).await;
        }
    }
}
