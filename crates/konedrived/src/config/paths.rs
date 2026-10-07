//! Where the daemon's files are, and each account's.

use std::path::{Path, PathBuf};

use crate::config::AccountId;

/// Locations of the daemon's files. Tests point these into a temp dir.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    /// `$XDG_STATE_HOME/konedrive`: each account's state is in `accounts/<id>/` below it.
    pub state_dir: PathBuf,
    /// The cached name and quota of version 1, where the migration finds them. Per account:
    /// [`Paths::account`].
    pub account_cache: PathBuf,
    /// The tree store of version 1, where the migration finds it. Per account:
    /// [`Paths::account`].
    pub tree_db: PathBuf,
    /// Where files are rescued to: version 1 directly below it, each account in `<id>/`.
    pub rescue_dir: PathBuf,
    /// The freedesktop thumbnail cache, shared by every account (keyed by file URI).
    pub thumbnails: PathBuf,
}

/// Where one account's files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountPaths {
    /// `accounts/<id>/` under the state directory: everything in it goes with the account.
    pub dir: PathBuf,
    /// Its cached name and quota.
    pub account_cache: PathBuf,
    /// Its tree store, activity log and conflicts.
    pub tree_db: PathBuf,
    /// `rescued/<id>/`: kept when the account is removed.
    pub rescue_dir: PathBuf,
}

impl Paths {
    /// Standard XDG locations for the current user.
    pub fn from_xdg() -> anyhow::Result<Self> {
        let config = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no XDG config directory"))?;
        let state = dirs::state_dir().ok_or_else(|| anyhow::anyhow!("no XDG state directory"))?;
        let data = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("no XDG data directory"))?;
        let cache = dirs::cache_dir().ok_or_else(|| anyhow::anyhow!("no XDG cache directory"))?;
        let state = state.join("konedrive");
        Ok(Self {
            config_file: config.join("konedrive").join("config.toml"),
            account_cache: state.join("account.json"),
            tree_db: state.join("tree.sqlite"),
            state_dir: state,
            rescue_dir: data.join("konedrive").join("rescued"),
            thumbnails: cache.join("thumbnails"),
        })
    }

    /// Every file directly under `dir`.
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            config_file: dir.join("config.toml"),
            state_dir: dir.to_owned(),
            account_cache: dir.join("account.json"),
            tree_db: dir.join("tree.sqlite"),
            rescue_dir: dir.join("rescued"),
            thumbnails: dir.join("thumbnails"),
        }
    }

    /// The files of account `id`. `None` unless `id` is an account id
    /// ([`AccountId::is_valid`]), so a hand-edited id never names a path outside
    /// `accounts/`.
    pub fn account(&self, id: &AccountId) -> Option<AccountPaths> {
        if !id.is_valid() {
            return None;
        }
        let dir = self.state_dir.join("accounts").join(id.as_str());
        Some(AccountPaths {
            account_cache: dir.join("account.json"),
            tree_db: dir.join("tree.sqlite"),
            dir,
            rescue_dir: self.rescue_dir.join(id.as_str()),
        })
    }
}
