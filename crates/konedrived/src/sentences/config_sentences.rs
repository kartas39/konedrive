//! What is said of `config.toml` and of the accounts in it (`crate::config::config_trouble`).

use std::fmt;

use crate::config::{AccountHold, ConfigTrouble};

impl fmt::Display for AccountHold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountHold::BadId { id } => write!(f, "its id {id:?} is not 12 lowercase hexadecimal characters"),
            AccountHold::SameId { other } => write!(f, "its id is also the id of {other:?}"),
            AccountHold::SameLabel { other } => write!(f, "its label is also the label of {other:?}"),
            AccountHold::SameDrive { other } => write!(f, "it is the same Microsoft account as {other:?}"),
            AccountHold::SameRoot { other } => write!(f, "its folder has the root id of the folder of {other:?}"),
            AccountHold::NestedFolder { folder, other, other_folder } => write!(
                f,
                "its folder {} is, is inside, or contains the folder of {other:?}, {}",
                folder.display(),
                other_folder.display()
            ),
        }
    }
}

impl fmt::Display for ConfigTrouble {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigTrouble::Unreadable { detail } => write!(f, "{detail}"),
            ConfigTrouble::AccountNotLoaded { label, hold: Some(hold) } => write!(f, "the account {label:?} is not loaded: {hold}"),
            ConfigTrouble::AccountNotLoaded { label, hold: None } => {
                write!(f, "the account {label:?} is not loaded: its id repeats an earlier account's")
            }
            ConfigTrouble::AccountNotBuilt { label, detail } => write!(f, "the account {label:?} cannot be loaded: {detail}"),
            ConfigTrouble::FilesNotMoved { label, detail } => write!(f, "moving the files of account {label:?} failed: {detail}"),
            ConfigTrouble::HoldSettingsNotMoved { detail } => {
                write!(f, "moving the accounts' pause_on_metered and on_battery failed: {detail}")
            }
        }
    }
}

#[cfg(test)]
mod tests;
