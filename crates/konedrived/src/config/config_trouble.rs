//! What is wrong with `config.toml` or with an account in it, as cases with their data:
//! why an account is held back ([`AccountHold`], `docs/design/accounts.md` §4.2), and what
//! `Accounts.LastError` says ([`ConfigTrouble`]). Their sentences are `crate::sentences`'s.

use std::path::PathBuf;

use super::AccountId;

/// Why an account of `config.toml` cannot be brought up: its id cannot name it, or it has
/// what an earlier account has. `other` is the earlier account's label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountHold {
    /// Its id is not 12 lowercase hexadecimal characters.
    BadId { id: AccountId },
    SameId { other: String },
    SameLabel { other: String },
    /// The same Microsoft account: the same drive.
    SameDrive { other: String },
    /// Its folder has the root id of the earlier account's folder.
    SameRoot { other: String },
    /// Its folder is, is inside, or contains the earlier account's folder.
    NestedFolder { folder: PathBuf, other: String, other_folder: PathBuf },
}

/// One trouble `Accounts.LastError` says: of the file, or of an account the file gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigTrouble {
    /// The file could not be read: no account is loaded, and it is not written. `detail`
    /// is what the read ran into.
    Unreadable { detail: String },
    /// An account is not loaded: why it is held back, or `None` when its id repeats an
    /// account loaded already.
    AccountNotLoaded { label: String, hold: Option<AccountHold> },
    /// An account could not be built.
    AccountNotBuilt { label: String, detail: String },
    /// The files of a migrated account could not be moved where this version keeps them.
    FilesNotMoved { label: String, detail: String },
    /// The accounts' `pause_on_metered` and `on_battery` could not be moved to the one
    /// setting every account shares.
    HoldSettingsNotMoved { detail: String },
}
