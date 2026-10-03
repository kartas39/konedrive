//! Cached profile and quota (`account.json`), so the page can show the account offline, and
//! what the account's last token was valid for. Contains no secrets.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::write_atomic;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub display_name: String,
    pub email: String,
    /// The account's quota as last read (`crate::quota`), whoever read it; `quota_read_at`,
    /// when (unix seconds). `remaining`, `state` and the time are missing in a file written
    /// before they were kept, which reads as not read.
    pub quota_used: u64,
    pub quota_total: u64,
    #[serde(default)]
    pub quota_remaining: u64,
    #[serde(default)]
    pub quota_state: String,
    #[serde(default)]
    pub quota_read_at: i64,
    /// Unix time in seconds.
    pub fetched_at: u64,
    /// The last token response's `scope` (`docs/design/writes.md` §2): what decides, at the next start,
    /// whether a read-write account's refresh may ask for `Files.ReadWrite` again. Missing
    /// in a file written before it existed, which reads as nothing granted: read-only.
    #[serde(default)]
    pub granted_scopes: String,
    /// The drive the account's token was last seen to reach: at the next start the account is
    /// read-write only if it is the drive `config.toml` records. Missing in an older file,
    /// which reads as not seen: read-only until it is.
    #[serde(default)]
    pub drive_id: String,
}

/// Any problem reading the file means "no cache".
pub fn load(path: &Path) -> Option<AccountInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save(path: &Path, info: &AccountInfo) -> anyhow::Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(info)?)
}

pub fn remove(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("cannot remove {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests;
