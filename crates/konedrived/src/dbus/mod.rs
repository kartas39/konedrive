//! The interfaces of one account's folder — `org.konedrive.Folder`, `Transfers`,
//! `UploadQueue`, `Conflicts`, `LocalScan` and `ActivityLog` — on the account's object
//! `/org/konedrive/Accounts/<id>`, beside its `Account` (definitions: `dbus/*.xml`).
//! The per-file calls are `org.konedrive.Files`'s, routed by path (`crate::daemon::manager`).
//!
//! Follows the same split as `crate::dbus`/`crate::account`: this module is
//! the thin zbus wrapper, and `SyncService` (in `sync/mod.rs`) does the
//! actual work, so the latter can be exercised without a bus at all.
//!
//! `org.konedrive.Account` and `org.konedrive.TokenExport`, one of each per account on the
//! account's object `/org/konedrive/Accounts/<id>` (definitions: `dbus/*.xml`). The
//! accounts themselves, and the client id every account signs in with, are
//! `org.konedrive.Accounts`'s (`crate::dbus::accounts`).

pub mod account;
pub mod accounts;
pub mod activity_log;
pub mod conflicts;
pub mod export;
pub mod fault;
pub mod files;
pub mod folder;
pub mod local_scan;
pub mod signals;
#[cfg(feature = "dev-tools")]
pub mod token_export;
pub mod transfers;
pub mod upload_queue;

use std::sync::Arc;

use crate::sync::SyncService;

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
