//! Daemon configuration (`~/.config/konedrive/config.toml`, version 2) and file locations.
//!
//! One [`ConfigStore`] owns the file: it loads it, migrates version 1 into account #1
//! ([`crate::config::migrate`]), holds back accounts that collide ([`Config::holds`]), and runs every
//! read-modify-write under one lock ([`ConfigStore::update`]).
//!
//! - `paths` — where the daemon's files are, and each account's;
//! - `ids` — [`AccountId`] and [`DriveId`];
//! - `model` — what the file holds, and the rules of what it may hold;
//! - `store` — [`ConfigStore`];
//! - `atomic` — one file replaced in one step;
//! - `migrate` — version 1, and the moves a migration leaves for the start.

mod atomic;
mod ids;
pub mod migrate;
mod model;
mod paths;
mod store;

pub use atomic::write_atomic;
pub use ids::{AccountId, DriveId};
pub use model::{
    check_label, is_valid_client_id, AccountConfig, Config, HoldSettings, Mode, OnBattery, Origin, RootConfig, TransfersConfig,
    CONFIG_VERSION, DEFAULT_CLIENT_ID, MIGRATED_LABEL,
};
pub use paths::{AccountPaths, Paths};
pub use store::{ConfigError, ConfigStore, WriteStanding};

#[cfg(test)]
mod tests;
