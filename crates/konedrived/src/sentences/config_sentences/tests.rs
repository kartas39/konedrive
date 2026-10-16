use std::path::PathBuf;

use crate::config::{AccountHold, AccountId, ConfigTrouble};

/// Each reason an account is held back, as `Accounts.LastError` and the folder say it.
#[test]
fn each_hold_says_what_the_earlier_account_has() {
    let other = || "Home".to_owned();
    let id: AccountId = serde_json::from_str("\"my-account\"").unwrap();
    let said = [
        (AccountHold::BadId { id }, "its id \"my-account\" is not 12 lowercase hexadecimal characters"),
        (AccountHold::SameId { other: other() }, "its id is also the id of \"Home\""),
        (AccountHold::SameLabel { other: other() }, "its label is also the label of \"Home\""),
        (AccountHold::SameDrive { other: other() }, "it is the same Microsoft account as \"Home\""),
        (AccountHold::SameRoot { other: other() }, "its folder has the root id of the folder of \"Home\""),
        (
            AccountHold::NestedFolder { folder: PathBuf::from("/h/a/b"), other: other(), other_folder: PathBuf::from("/h/a") },
            "its folder /h/a/b is, is inside, or contains the folder of \"Home\", /h/a",
        ),
    ];
    for (hold, sentence) in said {
        assert_eq!(hold.to_string(), sentence);
    }
}

/// Each trouble of the file or of an account in it.
#[test]
fn each_trouble_of_the_file_says_what_happened_and_to_which_account() {
    let label = || "Work".to_owned();
    let said = [
        (ConfigTrouble::Unreadable { detail: "config.toml cannot be read".into() }, "config.toml cannot be read"),
        (
            ConfigTrouble::AccountNotLoaded { label: label(), hold: Some(AccountHold::SameLabel { other: "Home".into() }) },
            "the account \"Work\" is not loaded: its label is also the label of \"Home\"",
        ),
        (ConfigTrouble::AccountNotLoaded { label: label(), hold: None }, "the account \"Work\" is not loaded: its id repeats an earlier account's"),
        (ConfigTrouble::AccountNotBuilt { label: label(), detail: "no wallet".into() }, "the account \"Work\" cannot be loaded: no wallet"),
        (ConfigTrouble::FilesNotMoved { label: label(), detail: "disk full".into() }, "moving the files of account \"Work\" failed: disk full"),
        (
            ConfigTrouble::HoldSettingsNotMoved { detail: "disk full".into() },
            "moving the accounts' pause_on_metered and on_battery failed: disk full",
        ),
    ];
    for (trouble, sentence) in said {
        assert_eq!(trouble.to_string(), sentence);
    }
}
