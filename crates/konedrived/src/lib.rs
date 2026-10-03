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

/// The outbox at scale (issue #38): ignored tests, run by hand in release.
#[cfg(test)]
mod bench;
