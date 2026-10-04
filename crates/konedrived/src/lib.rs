//! konedrived: the KOneDrive user daemon.

pub mod config;
pub mod account;
pub mod helper;
pub mod folder;
pub mod conditions;
pub mod status;
pub mod hydration;
pub mod local;
pub mod upload;
pub mod remote;
pub mod desktop;
pub mod sync;
pub mod daemon;
pub mod dbus;

/// Not a layer: what a caught panic said, for whoever catches one.
pub mod panic;

/// A fake OneDrive on wiremock: the tests of every area that talks to OneDrive, and the VM
/// suite's write scenarios (`fault-injection`).
#[cfg(any(test, feature = "fault-injection"))]
#[path = "tests/fake_onedrive/mod.rs"]
pub mod fake_onedrive;

/// The outbox at scale (issue #38): ignored tests, run by hand in release.
#[cfg(test)]
#[path = "tests/bench.rs"]
mod bench;
