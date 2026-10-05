//! The quota of one account's drive: one copy, served by `org.konedrive.Account`
//! (`QuotaUsed`, `QuotaTotal`, `QuotaRemaining`, `QuotaState`), whoever reads it — the
//! account's info (a sign-in, `RefreshInfo`) or the uploads' space check (`Folder.Refresh`, a
//! refused upload, the check every 30 minutes, `upload::space`). Between two reads the
//! bytes uploaded come off `QuotaRemaining` and are added to `QuotaUsed`. Its figures
//! ([`QuotaFigures`]) are kept in the account's state ([`AccountSnapshot`]), and cached in
//! `account.json` at every read.

use std::sync::Arc;

use konedrive_graph::drive::DriveQuota;
use serde::{Deserialize, Serialize};

use crate::clock::unix_now;
use crate::account::state::{AccountSnapshot, StateHandle};

/// The quota as last read, whoever read it: the one shape it has in the account's state and
/// in `account.json`, whose keys are the fields' with `quota_` before them. `remaining`,
/// `state` and the time are missing in a file written before they were kept, which reads as
/// not read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaFigures {
    /// `QuotaUsed`; 0 until read.
    #[serde(rename = "quota_used")]
    pub used: u64,
    /// `QuotaTotal`; 0 until read.
    #[serde(rename = "quota_total")]
    pub total: u64,
    /// `QuotaRemaining`: Graph's `remaining`, less what went up since.
    #[serde(default, rename = "quota_remaining")]
    pub remaining: u64,
    /// `QuotaState`: `normal`, `nearing`, `critical` or `exceeded`; empty until read.
    #[serde(default, rename = "quota_state")]
    pub state: String,
    /// Unix seconds of the last read; 0 for none. Not on the bus.
    #[serde(default, rename = "quota_read_at")]
    pub read_at: i64,
}

impl QuotaFigures {
    /// `quota`, read at `at`: `used` and `total` when Graph gave them (a total of 0 is none
    /// given), `remaining` and `state` likewise.
    pub fn read(&mut self, quota: &DriveQuota, at: i64) {
        if quota.total > 0 {
            self.total = quota.total;
            self.used = quota.used;
        }
        if let Some(remaining) = quota.remaining {
            self.remaining = remaining;
        }
        if !quota.state.is_empty() {
            self.state = quota.state.clone();
        }
        self.read_at = at;
    }

    /// `bytes` went up: they come off what is left and are added to what is used, once a read
    /// has said what is left.
    fn uploaded(&mut self, bytes: u64) {
        if self.read_at > 0 {
            self.remaining = self.remaining.saturating_sub(bytes);
            self.used = self.used.saturating_add(bytes);
        }
    }

    /// The figures as a read gives them.
    pub fn as_drive_quota(&self) -> DriveQuota {
        DriveQuota {
            total: self.total,
            used: self.used,
            remaining: (self.read_at > 0).then_some(self.remaining),
            state: self.state.clone(),
        }
    }
}

/// What a read writes beside the state: the account's cache, which keeps the quota across
/// restarts. `None` keeps it nowhere (a test, a tool).
pub type Keep = Arc<dyn Fn(&AccountSnapshot) + Send + Sync>;

/// The account's one quota. Cheap to clone: every clone is the same quota.
#[derive(Clone)]
pub struct Quota {
    state: StateHandle,
    keep: Option<Keep>,
}

impl Quota {
    /// The quota in `state`, the account's, cached by `keep` at every read.
    pub fn new(state: StateHandle, keep: Option<Keep>) -> Self {
        Self { state, keep }
    }

    /// A quota of its own, kept nowhere: for a folder with no account (tests).
    pub fn detached() -> Self {
        Self::new(StateHandle::new(AccountSnapshot::default()), None)
    }

    /// The account state the quota is kept in.
    pub fn state(&self) -> &StateHandle {
        &self.state
    }

    /// A read of the quota, now: every figure it gave replaces the last one, and what it did
    /// not give keeps its last value.
    pub fn read(&self, quota: &DriveQuota) {
        self.read_at(quota, unix_now());
    }

    /// As [`read`](Self::read), made at `at` (unix seconds).
    pub fn read_at(&self, quota: &DriveQuota, at: i64) {
        self.state.update(|s| s.quota.read(quota, at));
        if let Some(keep) = &self.keep {
            keep(&self.state.get());
        }
    }

    /// The last read, if it was made within `seconds` of now: what a refusal of another
    /// upload may use instead of asking again. Its figures are the quota's now.
    pub fn read_within(&self, seconds: i64) -> Option<DriveQuota> {
        let quota = self.state.get().quota;
        (quota.read_at > 0 && unix_now() - quota.read_at < seconds).then(|| quota.as_drive_quota())
    }

    /// `bytes` went up: they come off what is left and are added to what is used, once a read
    /// has said what is left.
    pub fn uploaded(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.state.update(|s| s.quota.uploaded(bytes));
    }
}

#[cfg(test)]
mod tests;
