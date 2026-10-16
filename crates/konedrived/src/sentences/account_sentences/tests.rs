use super::say_writes_refusal;
use crate::account::account_trouble::{AccountTrouble, ReadWriteFailure, Refused, SignInFailure};
use crate::account::state::ModeNote;
use crate::config::AccountId;

/// Each way a sign-in in the browser gives no sign-in; a cancel says nothing.
#[test]
fn each_failure_of_a_sign_in_in_the_browser_says_what_happened() {
    let detail = || "connection reset".to_owned();
    let said = [
        (SignInFailure::Cancelled, ""),
        (SignInFailure::CannotListen { detail: detail() }, "cannot listen on localhost: connection reset"),
        (
            SignInFailure::BrowserTimedOut,
            "Timed out waiting for the browser. If Microsoft showed an error page, check the app registration: client ID \
             and redirect URI http://localhost.",
        ),
        (SignInFailure::Browser { detail: detail() }, "connection reset"),
        (SignInFailure::AccessDenied, "Access was denied in the browser."),
        (
            SignInFailure::MicrosoftRefused { error: "invalid_request".into(), description: "bad scope".into() },
            "Microsoft reported an error: invalid_request: bad scope",
        ),
        (SignInFailure::CodeNotExchanged { detail: detail() }, "Sign-in failed: connection reset"),
        (SignInFailure::NoRefreshToken, "Microsoft did not return a refresh token."),
        (SignInFailure::Unidentified { detail: detail() }, "Could not check which account this is (connection reset); try again."),
    ];
    for (failure, sentence) in said {
        assert_eq!(failure.to_string(), sentence);
    }
}

/// Each refusal of a sign-in for the account it was for, or for a new account.
#[test]
fn each_refusal_of_a_sign_in_says_why() {
    let id: AccountId = serde_json::from_str("\"0123456789ab\"").unwrap();
    let said = [
        (Refused::Failed(SignInFailure::AccessDenied), "Access was denied in the browser."),
        (Refused::AlreadyConnected { id, label: "Home".into() }, "This Microsoft account is already connected as 'Home'."),
        (Refused::NotToldApart, "Could not check which account this is; try again."),
        (Refused::Removed, "This account was removed."),
        (
            Refused::OtherMicrosoftAccount { who: "'Work' (w@example.com)".into() },
            "This account is 'Work' (w@example.com). You signed in as a different Microsoft account; to connect that one, \
             add a new account.",
        ),
        (Refused::NotStored { detail: "the wallet is locked".into() }, "the wallet is locked"),
        (Refused::NotSaved { detail: "config.toml cannot be written".into() }, "config.toml cannot be written"),
        (Refused::NotMade { detail: "no directory".into() }, "no directory"),
        (Refused::NotOnBus { detail: "name taken".into() }, "cannot put the account on the bus: name taken"),
    ];
    for (refused, sentence) in said {
        assert_eq!(refused.to_string(), sentence);
    }
}

/// Each way a switch to read-write does not go through says that the account stays
/// read-only, where it does.
#[test]
fn each_failure_of_a_switch_to_read_write_says_the_account_stays_read_only() {
    let said = [
        (ReadWriteFailure::Failed(SignInFailure::AccessDenied), "Access was denied in the browser.".to_owned()),
        (
            ReadWriteFailure::NotGranted { granted: "Files.Read".into() },
            "Microsoft did not allow konedrive to change files in OneDrive (the sign-in granted \"Files.Read\"); the account \
             stays read-only."
                .to_owned(),
        ),
        (ReadWriteFailure::NoRefreshToken, "Microsoft did not return a refresh token; the account stays read-only.".to_owned()),
        (
            ReadWriteFailure::Unidentified { detail: "timed out".into() },
            "Could not check which account this is (timed out); the account stays read-only.".to_owned(),
        ),
        (ReadWriteFailure::Removed, "This account was removed.".to_owned()),
        (ReadWriteFailure::DriveNotKnown, format!("{}; the account stays read-only.", crate::account::DRIVE_NOT_KNOWN)),
        (
            ReadWriteFailure::OtherMicrosoftAccount { who: "'Work'".into() },
            "This account is 'Work', and the browser signed in as a different Microsoft account; the account stays read-only."
                .to_owned(),
        ),
        (
            ReadWriteFailure::ConfigUnreadable,
            "config.toml cannot be read now, so the switch to read-write was not saved; the account stays read-only.".to_owned(),
        ),
        (ReadWriteFailure::NotStored { detail: "the wallet is locked".into() }, "the wallet is locked; the account stays read-only.".to_owned()),
        (ReadWriteFailure::NotSaved { detail: "config.toml cannot be written".into() }, "config.toml cannot be written".to_owned()),
    ];
    for (failure, sentence) in said {
        assert_eq!(failure.to_string(), sentence);
    }
}

/// What an account's `LastError` says of each trouble.
#[test]
fn each_trouble_of_an_account_says_what_went_wrong() {
    let said = [
        (AccountTrouble::SignIn(Refused::Removed), "This account was removed."),
        (AccountTrouble::ReadWrite(ReadWriteFailure::Removed), "This account was removed."),
        (AccountTrouble::ProfileNotRead { detail: "timed out".into() }, "Could not load account info: timed out"),
        (AccountTrouble::NoClientId, "Set a client ID first, then sign in again."),
        (AccountTrouble::StoredSignInLost, "The stored sign-in was lost. Sign in again."),
        (AccountTrouble::Token { detail: "the session expired".into() }, "the session expired"),
    ];
    for (trouble, sentence) in said {
        assert_eq!(trouble.to_string(), sentence);
    }
}

/// Why an account runs read-only, in `LastError` and as the upload queue says it.
#[test]
fn each_note_of_the_mode_says_why_the_account_runs_read_only_and_why_nothing_is_uploaded() {
    let mismatch = || ModeNote::DriveMismatch { live: "D2".into(), recorded: "D1".into() };
    let said = [
        (ModeNote::ConfigUnreadable, "config.toml cannot be read now, so this account runs read-only until it can", "config.toml cannot be read"),
        (
            ModeNote::NoDriveRecorded,
            "config.toml sets this account to read-write, but records no OneDrive drive for it; it runs read-only until a \
             sign-in records one",
            "config.toml records no drive for the account",
        ),
        (
            ModeNote::DriveNotSeen,
            "config.toml sets this account to read-write, but which OneDrive its sign-in reaches has not been checked yet; \
             it runs read-only until it is",
            "which OneDrive drive the account's sign-in reaches has not been seen yet",
        ),
        (
            mismatch(),
            "this account's sign-in reaches the OneDrive drive D2, but config.toml records drive D1 for it; it runs \
             read-only until the two agree",
            "the account's token was last seen to reach drive \"D2\", not drive \"D1\", which config.toml records",
        ),
        (
            ModeNote::SignInToWrite,
            "Changes made here are not uploaded: this account's sign-in does not allow konedrive to change files in \
             OneDrive. Switch it to read-write again to sign in with that permission.",
            "the account's sign-in does not allow changes",
        ),
    ];
    for (note, last_error, refusal) in said {
        assert_eq!(note.to_string(), last_error);
        assert_eq!(say_writes_refusal(&note), refusal);
    }
    let wider = ModeNote::WiderGrant("Files.ReadWrite".into()).to_string();
    assert!(wider.starts_with("Microsoft answered a request for read-only access with a token that can also change files (Files.ReadWrite)"), "{wider}");
}
