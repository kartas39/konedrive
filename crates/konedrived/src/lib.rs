//! konedrived: the KOneDrive user daemon.

pub mod config;
pub mod account;
pub(crate) mod folder;
pub mod conditions;
pub mod status;
pub(crate) mod content_source;
pub(crate) mod local;
pub(crate) mod sync_parts;
pub mod mount;
pub(crate) mod folder_steps;
pub(crate) mod onedrive_steps;
pub(crate) mod listing;
pub mod desktop;
pub(crate) mod sync_plan_task;
pub(crate) mod sync_tasks;
pub mod upload_queue;
pub mod account_folder;
pub mod daemon;
pub mod dbus;

/// Not a layer: the wall clock, read in one place.
pub(crate) mod clock;

/// Not a layer: what a caught panic said, and a lock taken after one.
pub mod panic;

/// A fake OneDrive on wiremock: the tests of every area that talks to OneDrive, and the VM
/// suite (`fault-injection`).
#[cfg(any(test, feature = "fault-injection"))]
#[path = "tests/fake_onedrive/mod.rs"]
pub mod fake_onedrive;
