//! konedrived: the KOneDrive user daemon.

/// The outbox at scale (issue #38): ignored tests, run by hand in release.
#[cfg(test)]
mod bench;
pub mod account;
pub mod accounts;
pub mod account_cache;
pub mod config;
pub mod dbus;
pub mod drive;
pub mod graph;
pub mod loopback;
pub mod migrate;
pub mod oauth;
pub mod pkce;
pub mod pool;
pub mod quickxor;
pub mod secret;
pub mod state;
pub mod sync;
pub mod token;
pub mod tree;
