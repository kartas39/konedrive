//! konedrived: the KOneDrive user daemon.

/// The outbox at scale (issue #38): ignored tests, run by hand in release.
#[cfg(test)]
mod bench;
pub mod account;
pub mod accounts;
pub mod account_cache;
pub mod config;
pub mod dbus;
pub mod migrate;
pub mod quota;
pub mod secret;
pub mod state;
pub mod stop;
pub mod sync;
