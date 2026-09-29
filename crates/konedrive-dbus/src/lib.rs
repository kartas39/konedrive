//! D-Bus names and the client proxies of `konedrived` (definitions: `dbus/*.xml`).
//!
//! The daemon, under [`SERVICE_NAME`], serves:
//!
//! - [`ACCOUNTS_PATH`]: `org.konedrive.Accounts`, `org.konedrive.Files` and
//!   `org.freedesktop.DBus.ObjectManager`;
//! - one object per account, [`account_path`]: `org.konedrive.Account` (the
//!   sign-in and the quota), its folder's `org.konedrive.Folder`,
//!   `org.konedrive.Transfers`, `org.konedrive.UploadQueue`,
//!   `org.konedrive.Conflicts`, `org.konedrive.LocalScan` and
//!   `org.konedrive.ActivityLog`, and `org.konedrive.TokenExport`.
//!
//! No name carries a version: the daemon and every client of it ship together.
//!
//! Their proxies are in [`accounts`]. Nothing is served at
//! `/org/konedrive/Daemon`, the single-account object of earlier versions.

pub mod accounts;
pub mod testing;

use zbus::zvariant::OwnedObjectPath;

pub const SERVICE_NAME: &str = "org.konedrive.Daemon";

/// The account manager: `Accounts`, `Files` and the `ObjectManager` of the
/// account objects below it.
pub const ACCOUNTS_PATH: &str = "/org/konedrive/Accounts";

pub const ACCOUNTS_INTERFACE_NAME: &str = "org.konedrive.Accounts";
pub const FILES_INTERFACE_NAME: &str = "org.konedrive.Files";
pub const ACCOUNT_INTERFACE_NAME: &str = "org.konedrive.Account";
pub const TOKEN_EXPORT_INTERFACE_NAME: &str = "org.konedrive.TokenExport";
pub const FOLDER_INTERFACE_NAME: &str = "org.konedrive.Folder";
pub const TRANSFERS_INTERFACE_NAME: &str = "org.konedrive.Transfers";
pub const UPLOAD_QUEUE_INTERFACE_NAME: &str = "org.konedrive.UploadQueue";
pub const CONFLICTS_INTERFACE_NAME: &str = "org.konedrive.Conflicts";
pub const LOCAL_SCAN_INTERFACE_NAME: &str = "org.konedrive.LocalScan";
pub const ACTIVITY_LOG_INTERFACE_NAME: &str = "org.konedrive.ActivityLog";

/// The object path of the account `id`: `/org/konedrive/Accounts/<id>`.
///
/// `None` for an id that cannot be one element of an object path (empty, or
/// anything but ASCII letters, digits and `_`). The daemon's ids — 12
/// lowercase hex characters — always can; one read from a hand-edited
/// `config.toml` might not.
pub fn account_path(id: &str) -> Option<OwnedObjectPath> {
    let one_element = !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    one_element.then(|| {
        OwnedObjectPath::try_from(format!("{ACCOUNTS_PATH}/{id}")).expect("a checked element makes a valid path")
    })
}

/// The prefix every named refusal carries. A client that
/// wants to tell "the file was modified locally" from "the file is not
/// downloaded" matches on `<prefix>.ModifiedLocally` and
/// `<prefix>.NotHydrated` rather than on the message.
pub const ERROR_PREFIX: &str = "org.konedrive.Error";

/// What to tell a person about the helper in each `Accounts.HelperState`
/// that is not `connected`: what it means and how to start it. One wording
/// for the daemon's `LastError` and the CLI's `Helper:` line alike. `None` for
/// `connected`, and for anything this build does not know.
pub fn helper_advice(state: &str) -> Option<&'static str> {
    match state {
        "not-installed" => Some(
            "the konedrive helper is not installed: files are not kept in step and do not \
             download when opened. Install it: sudo scripts/install-helper.sh (see README)",
        ),
        "stopped" => Some(
            "the konedrive helper is not running: start it with `sudo systemctl start konedrive-helper`",
        ),
        "failed" => Some("the konedrive helper failed: see `systemctl status konedrive-helper`"),
        "unknown" => Some("the konedrive helper is not connected"),
        _ => None,
    }
}

/// The D-Bus error name a failed call carries, if it carries one.
///
/// `zbus::Error::MethodError` keeps the name and the message apart; matching
/// on the message is what a named error exists to replace.
pub fn error_name(error: &zbus::Error) -> Option<&str> {
    match error {
        zbus::Error::MethodError(name, _, _) => Some(name.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_path_is_one_element_below_the_manager() {
        assert_eq!(account_path("3f9a1c0e5b7d").unwrap().as_str(), "/org/konedrive/Accounts/3f9a1c0e5b7d");
        for bad in ["", "a/b", "a-b", "..", "é"] {
            assert_eq!(account_path(bad), None, "{bad:?}");
        }
    }
}
