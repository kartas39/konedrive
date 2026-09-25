//! The client proxies of the multiple-accounts contract (definitions:
//! `dbus/org.konedrive.{Accounts1,Files1,Account1,Sync1,Dev1}.xml`).
//!
//! [`Accounts1Proxy`] and [`Files1Proxy`] have a default path,
//! [`ACCOUNTS_PATH`](crate::ACCOUNTS_PATH). [`Account1Proxy`], [`Sync1Proxy`]
//! and [`Dev1Proxy`] have none: each is built with one account's path, an
//! entry of [`Accounts1Proxy::accounts`] or [`account_path`](crate::account_path):
//!
//! ```no_run
//! # async fn example(conn: &zbus::Connection) -> zbus::Result<()> {
//! use konedrive_dbus::accounts::{Accounts1Proxy, Sync1Proxy};
//!
//! let manager = Accounts1Proxy::new(conn).await?;
//! for path in manager.accounts().await? {
//!     let sync = Sync1Proxy::new(conn, path).await?;
//!     println!("{}", sync.root_path().await?);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! The same object as `Accounts1` serves `org.freedesktop.DBus.ObjectManager`:
//! `zbus::fdo::ObjectManagerProxy` at [`ACCOUNTS_PATH`](crate::ACCOUNTS_PATH).

use zbus::zvariant::{ObjectPath, OwnedObjectPath};

/// `/org/konedrive/Accounts`: the accounts of this user.
#[zbus::proxy(
    interface = "org.konedrive.Accounts1",
    default_service = "org.konedrive.Daemon",
    default_path = "/org/konedrive/Accounts",
    gen_blocking = false
)]
pub trait Accounts1 {
    /// Adds a signed-out, read-only account with no folder; its object path.
    /// Refused `InvalidArgs` for a label that breaks the rules
    /// (`dbus/org.konedrive.Accounts1.xml`).
    fn add(&self, label: &str) -> zbus::Result<OwnedObjectPath>;
    /// Forgets the account's folder as `Sync1.UnregisterRoot` does, deletes
    /// its token, cache and tree store, and removes the object. Refused
    /// `NoAccount` for a path that names no account.
    fn remove(&self, account: &ObjectPath<'_>) -> zbus::Result<()>;
    /// The Entra application every account signs in with.
    fn set_client_id(&self, id: &str) -> zbus::Result<()>;

    /// Every account's object path, in the order the accounts were added.
    #[zbus(property)]
    fn accounts(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(property)]
    fn client_id(&self) -> zbus::Result<String>;
    /// The privileged helper as the daemon sees it: `connected`,
    /// `not-installed`, `stopped`, `failed` or `unknown`
    /// ([`helper_advice`](crate::helper_advice)).
    #[zbus(property)]
    fn helper_state(&self) -> zbus::Result<String>;
    /// Trouble that belongs to no account; empty when there is none.
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;
}

/// `/org/konedrive/Accounts`: per-file calls, each routed by path to the
/// account whose folder holds it. A path in no account's folder is refused
/// `OutsideRoot`; `item_state` answers `not-managed`.
#[zbus::proxy(
    interface = "org.konedrive.Files1",
    default_service = "org.konedrive.Daemon",
    default_path = "/org/konedrive/Accounts",
    gen_blocking = false
)]
pub trait Files1 {
    fn hydrate(&self, path: &str) -> zbus::Result<()>;
    fn dehydrate(&self, path: &str) -> zbus::Result<()>;
    fn item_state(&self, path: &str) -> zbus::Result<String>;
    /// "Always keep on this device" for each path; how many files were
    /// queued for download.
    fn pin(&self, paths: &[&str]) -> zbus::Result<u32>;
    /// Unchecking "Always keep on this device": each path's own pin is
    /// removed, files stay downloaded; how many pins were removed. Refused
    /// `NotAllowed` for a path a folder above it pins.
    fn unpin(&self, paths: &[&str]) -> zbus::Result<u32>;
    /// "Free up space" for each path, its own pin taken off first: (files
    /// freed, bytes freed, files kept because they were in use or changed
    /// here, downloaded files kept by a pin below). Refused `NotAllowed` for
    /// a path a folder above it pins.
    fn free_up(&self, paths: &[&str]) -> zbus::Result<(u32, u64, u32, u32)>;
}

/// `/org/konedrive/Accounts/<id>`: one Microsoft account.
#[zbus::proxy(
    interface = "org.konedrive.Account1",
    default_service = "org.konedrive.Daemon",
    gen_blocking = false
)]
pub trait Account1 {
    fn begin_sign_in(&self) -> zbus::Result<String>;
    fn cancel_sign_in(&self) -> zbus::Result<()>;
    fn sign_out(&self) -> zbus::Result<()>;
    fn refresh_account_info(&self) -> zbus::Result<()>;
    /// Same rules as [`Accounts1Proxy::add`].
    fn set_label(&self, label: &str) -> zbus::Result<()>;
    /// Switches the mode to `read-only` or `read-write`; the URL of the
    /// sign-in the switch needs, empty when it needs none. Refused
    /// `WritesNotAllowed` (the development gate), `NotSignedIn`, or
    /// `PendingUploads` unless `force` (`dbus/org.konedrive.Account1.xml`).
    fn set_mode(&self, mode: &str, force: bool) -> zbus::Result<String>;

    /// The last element of the object path.
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn label(&self) -> zbus::Result<String>;
    /// The mode the account runs in: `read-only` or `read-write`.
    #[zbus(property)]
    fn mode(&self) -> zbus::Result<String>;
    /// `signed-out`, `signing-in` or `signed-in`.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn display_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn email(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn quota_used(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn quota_total(&self) -> zbus::Result<u64>;
}

/// `/org/konedrive/Accounts/<id>`: that account's folder.
#[zbus::proxy(
    interface = "org.konedrive.Sync1",
    default_service = "org.konedrive.Daemon",
    gen_blocking = false
)]
pub trait Sync1 {
    /// Refused `Overlaps` for a folder that is, is inside, or contains
    /// another account's folder.
    fn register_root(&self, path: &str) -> zbus::Result<()>;
    fn register_root_without_interception(&self, path: &str) -> zbus::Result<()>;
    fn unregister_root(&self) -> zbus::Result<()>;
    fn populate_from_directory(&self, source_dir: &str) -> zbus::Result<u64>;
    fn refresh(&self) -> zbus::Result<()>;
    fn skipped(&self) -> zbus::Result<Vec<(String, String)>>;
    /// (unix time, kind, full path, detail), newest first.
    fn recent_activity(&self, limit: u32) -> zbus::Result<Vec<(i64, String, String, String)>>;
    /// (unix time, original full path, full path it was moved to).
    fn conflicts(&self) -> zbus::Result<Vec<(i64, String, String, String)>>;
    fn dismiss_conflict(&self, rescued_path: &str) -> zbus::Result<()>;
    /// (files freed, bytes freed, files kept because they were in use).
    fn free_up_space(&self) -> zbus::Result<(u32, u64, u32)>;
    /// The changes waiting to be uploaded, oldest first, at most `limit` (0
    /// for all): (seq, kind, full path, state, bytes sent, bytes in all,
    /// reason, next try).
    #[allow(clippy::type_complexity)]
    fn outbox(&self, limit: u32) -> zbus::Result<Vec<(u64, String, String, String, u64, u64, String, i64)>>;
    /// Nothing is uploaded, and OneDrive is not asked, for `seconds` — or
    /// until [`resume`](Self::resume) when 0.
    fn pause(&self, seconds: u32) -> zbus::Result<()>;
    fn resume(&self) -> zbus::Result<()>;
    /// Refused `org.freedesktop.DBus.Error.InvalidArgs` for a pattern that
    /// cannot match a name.
    fn set_ignore_patterns(&self, patterns: &[&str]) -> zbus::Result<()>;
    /// The held removals go ahead; how many.
    fn confirm_deletes(&self) -> zbus::Result<u32>;
    /// The held removals are dropped and their items placed again; how many.
    fn restore_deletes(&self) -> zbus::Result<u32>;
    /// What stays on this computer and why: (full path, reason).
    fn not_uploaded(&self) -> zbus::Result<Vec<(String, String)>>;

    #[zbus(signal)]
    fn activity_added(&self, time: i64, kind: String, path: String, detail: String) -> zbus::Result<()>;

    #[zbus(property)]
    fn root_path(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn root_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn root_source(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn items_listed(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn items_placed(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn skipped_count(&self) -> zbus::Result<u64>;
    /// Unix seconds of the last successful check with OneDrive; 0 = never.
    #[zbus(property)]
    fn last_checked(&self) -> zbus::Result<i64>;
    /// What the folder's files take on disk (`st_blocks * 512`).
    #[zbus(property)]
    fn local_bytes(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn conflict_count(&self) -> zbus::Result<u32>;
    /// Files and folders with an "Always keep on this device" pin of their own.
    #[zbus(property)]
    fn pinned_count(&self) -> zbus::Result<u32>;
    /// Downloads under way: (full path, bytes done, bytes total).
    #[zbus(property)]
    fn transfers(&self) -> zbus::Result<Vec<(String, u64, u64)>>;
    /// Changes waiting to be uploaded, and the size of what they send.
    #[zbus(property)]
    fn pending_count(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn pending_bytes(&self) -> zbus::Result<u64>;
    /// Changes that need the user before they can go up.
    #[zbus(property)]
    fn blocked_count(&self) -> zbus::Result<u32>;
    /// Removals held by the mass-delete guard: `confirm_deletes` or
    /// `restore_deletes` decides them.
    #[zbus(property)]
    fn held_count(&self) -> zbus::Result<u32>;
    /// Uploads under way: (full path, bytes sent, bytes total).
    #[zbus(property)]
    fn uploads(&self) -> zbus::Result<Vec<(String, u64, u64)>>;
    #[zbus(property)]
    fn paused(&self) -> zbus::Result<bool>;
    /// Unix seconds when the pause ends by itself; 0 until resumed, or not paused.
    #[zbus(property)]
    fn paused_until(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn ignore_patterns(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn machine_name(&self) -> zbus::Result<String>;
}

/// `/org/konedrive/Accounts/<id>`: development only.
#[zbus::proxy(
    interface = "org.konedrive.Dev1",
    default_service = "org.konedrive.Daemon",
    gen_blocking = false
)]
pub trait Dev1 {
    /// An access token of this account that can change nothing, whatever
    /// its mode. Refused `NotSignedIn` when there is none.
    fn access_token(&self) -> zbus::Result<String>;
    /// The test-account harness's token, which can change files. Refused
    /// `WritesNotAllowed` for an account the development gate does not let
    /// through, `ModeNotGranted` for one that is not read-write.
    fn read_write_access_token(&self) -> zbus::Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ACCOUNTS_INTERFACE_NAME, ACCOUNTS_PATH, ACCOUNT_INTERFACE_NAME, DEV_INTERFACE_NAME, FILES_INTERFACE_NAME,
        SERVICE_NAME, SYNC_INTERFACE_NAME,
    };
    use zbus::proxy::Defaults;

    /// The macro takes literals; this keeps them in step with the constants.
    fn defaults<P: Defaults>() -> (String, String, Option<String>) {
        (
            P::INTERFACE.as_ref().expect("an interface").to_string(),
            P::DESTINATION.as_ref().expect("a destination").to_string(),
            P::PATH.as_ref().map(|path| path.to_string()),
        )
    }

    #[test]
    fn proxies_use_the_published_names() {
        let manager = |interface: &str| {
            (
                interface.to_owned(),
                SERVICE_NAME.to_owned(),
                Some(ACCOUNTS_PATH.to_owned()),
            )
        };
        let account = |interface: &str| (interface.to_owned(), SERVICE_NAME.to_owned(), None);
        assert_eq!(defaults::<Accounts1Proxy>(), manager(ACCOUNTS_INTERFACE_NAME));
        assert_eq!(defaults::<Files1Proxy>(), manager(FILES_INTERFACE_NAME));
        assert_eq!(defaults::<Account1Proxy>(), account(ACCOUNT_INTERFACE_NAME));
        assert_eq!(defaults::<Sync1Proxy>(), account(SYNC_INTERFACE_NAME));
        assert_eq!(defaults::<Dev1Proxy>(), account(DEV_INTERFACE_NAME));
    }
}
