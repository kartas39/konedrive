//! Cached profile and quota (`account.json`), so the page can show the account offline, and
//! what the account's last token was valid for. Contains no secrets.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::write_atomic;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub display_name: String,
    pub email: String,
    pub quota_used: u64,
    pub quota_total: u64,
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
mod tests {
    use super::*;

    fn info() -> AccountInfo {
        AccountInfo {
            display_name: "Ann".into(),
            email: "ann@example.com".into(),
            quota_used: 1,
            quota_total: 2,
            fetched_at: 3,
            granted_scopes: "Files.Read User.Read".into(),
            drive_id: "D1".into(),
        }
    }

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state").join("account.json");
        save(&file, &info()).unwrap();
        assert_eq!(load(&file), Some(info()));
    }

    #[test]
    fn missing_or_corrupt_files_load_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("account.json");
        assert_eq!(load(&file), None);
        std::fs::write(&file, "{not json").unwrap();
        assert_eq!(load(&file), None);
    }

    #[test]
    fn remove_tolerates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("account.json");
        remove(&file);
        save(&file, &info()).unwrap();
        remove(&file);
        assert!(!file.exists());
    }
}
