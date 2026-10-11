//! The folder as `config.toml` records it ([`Persisted`]): the account's `[accounts.root]`,
//! read and written only through the daemon's one `ConfigStore`.

use std::path::PathBuf;

use super::folder::{Down, Record};
use super::{SyncError, SyncService};
use crate::config::RootConfig;
use crate::folder::root::{self, SyncRoot};

/// The one word `config.toml` has for what a folder shows (`source`), and `Folder.RootSource`
/// with it.
pub(super) const ONEDRIVE: &str = "onedrive";

/// A root as `config.toml` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Persisted {
    pub path: PathBuf,
    /// Empty in a config written before the id was recorded.
    pub root_id: String,
    /// What `config.toml` has for `source` when it is not `"onedrive"` (a hand edit:
    /// `"OneDrive"`; a folder of another kind). Such a folder is never brought up
    /// ([`Down::UnreadSource`]), and is held for a Forget alone. Written back as it was
    /// read.
    pub source_as_written: Option<String>,
    /// Whether this daemon is the one that excluded the root from Baloo; `false` in a
    /// config written before this existed.
    pub baloo_excluded: bool,
}

impl Persisted {
    /// What records a folder at `root`.
    pub fn of(root: &SyncRoot, baloo_excluded: bool) -> Self {
        Self { path: root.path.clone(), root_id: root.root_id.clone(), source_as_written: None, baloo_excluded }
    }

    fn read(root: RootConfig) -> Self {
        Self {
            path: root.path,
            root_id: root.id,
            source_as_written: (root.source != ONEDRIVE).then_some(root.source),
            baloo_excluded: root.baloo_excluded,
        }
    }
}

/// What is said of a recorded folder that nothing gives a root id for.
pub(super) fn no_root_id(path: &std::path::Path) -> String {
    format!(
        "cannot bring up the sync folder {}: config.toml does not record its root id, \
         and the folder carries none that can be read",
        path.display()
    )
}

impl SyncService {
    /// The root is "persisted, so it survives a restart" — with its id — as the account's
    /// `[accounts.root]`, through the one `ConfigStore`: every write re-reads
    /// the file, so nothing else in it is lost. `Err` when the file could not
    /// be written — or could not be read: what could not be read is never
    /// overwritten. The account's drive stays: it is the account's, not the
    /// folder's (`docs/design/accounts.md` §6.1).
    pub(super) fn save_root(&self, root: Option<&Persisted>) -> Result<(), SyncError> {
        let persist = &self.wiring.persist;
        let root = root.map(|root| RootConfig {
            path: root.path.clone(),
            id: root.root_id.clone(),
            source: root.source_as_written.clone().unwrap_or_else(|| ONEDRIVE.into()),
            baloo_excluded: root.baloo_excluded,
        });
        persist.store.set_root(&persist.account, root).map_err(|e| {
            SyncError::Config(format!("cannot record the sync folder in {}: {e}", persist.store.file().display()))
        })
    }

    /// [`save_root`](Self::save_root) where a failure cannot be undone
    /// anyway. The registration itself stands — the root is bound and
    /// usable right now, or forgotten — but the next start will not know.
    pub(super) fn persist_or_log(&self, root: Option<&Persisted>) {
        if let Err(e) = self.save_root(root) {
            tracing::error!("{e}");
        }
    }

    /// [`persist_or_log`](Self::persist_or_log), only when `config.toml`
    /// does not already say exactly this.
    pub(super) fn remember(&self, root: &Persisted) {
        if self.persisted_root().as_ref() != Some(root) {
            self.persist_or_log(Some(root));
        }
    }

    pub(super) fn persisted_root(&self) -> Option<Persisted> {
        let persist = &self.wiring.persist;
        Some(Persisted::read(persist.store.account(&persist.account)?.root?))
    }

    /// [`persisted_root`](Self::persisted_root) from `config.toml` as it is now, read
    /// again: a hand edit made while the daemon runs counts. `None` when the file cannot
    /// be read now (caught half-saved, say); `Some(None)` when it records no folder.
    pub(super) fn persisted_root_now(&self) -> Option<Option<Persisted>> {
        let persist = &self.wiring.persist;
        let config = persist.store.current()?;
        Some(config.account(&persist.account).and_then(|account| account.root.clone()).map(Persisted::read))
    }

    /// The folder `config.toml` records, as the daemon holds it before it is up, and why
    /// it is not up yet.
    ///
    /// Its id comes from `config.toml` or, from a config written before the id was
    /// recorded, from the folder itself.
    pub(super) async fn recorded(&self, persisted: Persisted) -> (Record, Down) {
        let found = if root::looks_like_a_root_id(&persisted.root_id) {
            Some(persisted.root_id.clone())
        } else {
            root::recorded_root_id(&persisted.path).await
        };
        let down = match persisted.source_as_written {
            // Said at once; the folder is held all the same, for a Forget (`SY6`).
            Some(written) => Down::UnreadSource { written },
            None => Down::NotYetUp,
        };
        let record = Record {
            root: SyncRoot { path: persisted.path, root_id: found.unwrap_or(persisted.root_id) },
            baloo: persisted.baloo_excluded,
            kept: super::folder::Kept::default(),
        };
        (record, down)
    }
}
