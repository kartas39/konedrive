//! The daemon on D-Bus: thin zbus wrappers, one file for each interface, named like its
//! definition in `dbus/*.xml`. The work is done below this layer, so that it can be
//! exercised with no bus at all.
//!
//! - `/org/konedrive/Accounts`: `org.konedrive.Accounts` (the accounts, the settings they
//!   share) and `org.konedrive.Files` (the per-file calls, routed by
//!   path), over the `daemon::manager::AccountManager`; `org.konedrive.DevTools` too in a
//!   development build.
//! - `/org/konedrive/Accounts/<id>`: one account's `org.konedrive.Account` (and
//!   `TokenExport` in a development build), over its `account::AccountService`; and the
//!   interfaces of its folder — `Folder`, `Transfers`, `UploadQueue`, `Conflicts`,
//!   `LocalScan`, `ActivityLog` — over its `account_folder::SyncService`.
//!
//! `export` puts them on the bus, `signals` announces what changes by itself
//! (`properties` is the table of it), and `fault` is how every call is refused.

pub mod account;
pub(crate) mod accounts;
pub(crate) mod activity_log;
pub mod conflicts;
#[cfg(feature = "dev-tools")]
pub(crate) mod dev_tools;
pub mod export;
pub(crate) mod fault;
pub(crate) mod files;
pub(crate) mod folder;
pub(crate) mod local_scan;
mod properties;
pub(crate) mod signals;
#[cfg(feature = "dev-tools")]
pub(crate) mod token_export;
pub(crate) mod transfers;
pub mod upload_queue;

use std::sync::Arc;

use crate::account_folder::SyncService;

/// The interfaces of one account's folder, each over the same `SyncService`
/// (definitions: `dbus/org.konedrive.{Folder,Transfers,UploadQueue,Conflicts,LocalScan,ActivityLog}.xml`).
macro_rules! over_the_service {
    ($($name:ident),*) => {$(
        pub struct $name {
            service: Arc<SyncService>,
        }

        impl $name {
            pub fn new(service: Arc<SyncService>) -> Self {
                Self { service }
            }
        }
    )*};
}

over_the_service!(Folder, Transfers, UploadQueue, Conflicts, LocalScan, ActivityLog);
