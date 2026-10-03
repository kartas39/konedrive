//! The quota of one account's drive (issue #78): one copy, served by `org.konedrive.Account`
//! (`QuotaUsed`, `QuotaTotal`, `QuotaRemaining`, `QuotaState`), whoever reads it — the
//! account's info (a sign-in, `RefreshInfo`) or the uploads' space check (`Folder.Refresh`, a
//! refused upload, the check every 30 minutes, `sync::upload::space`). Between two reads the
//! bytes uploaded come off `QuotaRemaining` and are added to `QuotaUsed`. It is kept in the
//! account's state ([`AccountSnapshot`]), and cached in `account.json` at every read.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::drive::DriveQuota;
use crate::state::{AccountSnapshot, StateHandle};

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
        self.state.update(|s| apply(s, quota, at));
        if let Some(keep) = &self.keep {
            keep(&self.state.get());
        }
    }

    /// The last read, if it was made within `seconds` of now: what a refusal of another
    /// upload may use instead of asking again. Its figures are the quota's now.
    pub fn read_within(&self, seconds: i64) -> Option<DriveQuota> {
        let s = self.state.get();
        (s.quota_read_at > 0 && unix_now() - s.quota_read_at < seconds).then(|| as_drive_quota(&s))
    }

    /// `bytes` went up: they come off what is left and are added to what is used, once a read
    /// has said what is left.
    pub fn uploaded(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.state.update(|s| {
            if s.quota_read_at > 0 {
                s.quota_remaining = s.quota_remaining.saturating_sub(bytes);
                s.quota_used = s.quota_used.saturating_add(bytes);
            }
        });
    }
}

/// `quota`, read at `at`, into `s`: `used` and `total` when Graph gave them (a total of 0 is
/// none given), `remaining` and `state` likewise.
pub fn apply(s: &mut AccountSnapshot, quota: &DriveQuota, at: i64) {
    if quota.total > 0 {
        s.quota_total = quota.total;
        s.quota_used = quota.used;
    }
    if let Some(remaining) = quota.remaining {
        s.quota_remaining = remaining;
    }
    if !quota.state.is_empty() {
        s.quota_state = quota.state.clone();
    }
    s.quota_read_at = at;
}

/// The quota in `s`, as a read gives it.
pub fn as_drive_quota(s: &AccountSnapshot) -> DriveQuota {
    DriveQuota {
        total: s.quota_total,
        used: s.quota_used,
        remaining: (s.quota_read_at > 0).then_some(s.quota_remaining),
        state: s.quota_state.clone(),
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests;
