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
//! Their proxies are in [`accounts`], and the rows they answer with in [`rows`]. Nothing is served at
//! `/org/konedrive/Daemon`, the single-account object of earlier versions.

pub mod accounts;
mod helper;
mod refusal;
pub mod rows;
#[cfg(feature = "testing")]
pub mod testing;
pub mod version;

use zbus::zvariant::OwnedObjectPath;

pub use helper::HelperState;
pub use refusal::Refusal;

pub const SERVICE_NAME: &str = "org.konedrive.Daemon";

/// The account manager: `Accounts`, `Files` and the `ObjectManager` of the
/// account objects below it.
pub const ACCOUNTS_PATH: &str = "/org/konedrive/Accounts";

/// The rules of a label, which `Accounts.Add` and `Account.SetLabel` enforce (the daemon's
/// `config::check_label`), as the one sentence that tells a person about them: `konedrivectl`
/// shows it in the help of `account add` and after a refused label.
pub const LABEL_RULE: &str = "A label has 1 to 40 characters, no \"/\" and no control character, is not 12 \
                              hexadecimal digits (the shape of an account's id), and is not another account's \
                              label, whatever the case";

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

/// The prefix every named refusal of the daemon's own carries. A client that
/// wants to tell "the file was modified locally" from "the file is not
/// downloaded" matches on `<prefix>.ModifiedLocally` and
/// `<prefix>.NotHydrated` rather than on the message: in Rust, on
/// [`Refusal::ModifiedLocally`] and [`Refusal::NotHydrated`].
pub const ERROR_PREFIX: &str = "org.konedrive.Error";

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

/// Whether a failed read of a property failed because the daemon has no such property: a
/// daemon of an older build, not restarted since a newer client was installed. A client
/// treats the value as absent.
pub fn is_unknown_property(error: &zbus::Error) -> bool {
    const NAME: &str = "org.freedesktop.DBus.Error.UnknownProperty";
    match error {
        zbus::Error::FDO(inner) => matches!(inner.as_ref(), zbus::fdo::Error::UnknownProperty(_)),
        other => error_name(other) == Some(NAME),
    }
}

#[cfg(test)]
mod tests;
