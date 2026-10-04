//! What is kept back from OneDrive, grouped by what the user can do about it
//! (`NotUploadedSummary()`, `NotUploadedFiles()`; issue #20). Every reason a
//! change is kept back — an outbox row that is blocked, or waits with a
//! reason, and what the examination never uploads (`local_skipped`) — falls
//! into one [`Group`]; [`group_of`] is the one place that decides which,
//! from the reason and from whether the row is blocked: a blocked row needs
//! the user, and is never among what goes up by itself.
//! A change that waits for space in OneDrive (issue #2) is kept back too,
//! though its row stays `ready` in its place: see [`kept_reason`].
//!
//! The window shows one line per reason where one action fixes every file of
//! it, and lists files only where something can be done to each.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

pub use konedrive_tree::outbox::Group;
use konedrive_tree::outbox::{key_of, known_group, OutboxGroup, OutboxKind, OutboxState, Reason, SkippedGroup};
use konedrive_tree::{ReadStore, TreeError};

/// The key a reason as stored, a row's or a skip's, is summed under
/// ([`key_of`]).
pub fn reason_key(reason: &str) -> &str {
    key_of(reason)
}

/// How many unknown reasons are remembered as logged; past it, none is
/// logged any more (a backoff's reason can be an error's own text).
const UNKNOWN_LOGGED: usize = 64;

/// The group of what is kept back for `key`, `blocked` when it is an outbox
/// row in that state. A blocked row is never [`Group::Waiting`], whatever
/// its reason: nothing sends it again by itself, so it is listed per file
/// (`BlockedCount` counts it too). An unknown key is logged once, and is
/// [`Group::Waiting`] unless blocked.
pub fn group_of(key: &str, blocked: bool) -> Group {
    let known = known_group(key);
    if known.is_none() {
        static LOGGED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
        let mut logged = crate::panic::lock(&LOGGED);
        if logged.len() < UNKNOWN_LOGGED && logged.insert(key.to_owned()) {
            let shown = if blocked { "per file" } else { "waiting" };
            tracing::warn!("a change is kept back for a reason not in the table: {key:?}; shown as {shown}");
        }
    }
    match known {
        Some(Group::Waiting) | None if blocked => Group::PerFile,
        Some(group) => group,
        None => Group::Waiting,
    }
}

/// The reason rows of `kind`, `state` and `reason` are kept back for, if
/// they are: blocked, or waiting (or in backoff) with a reason; ready but
/// waiting for space (`waiting-for-space`, `too-big:…`); and, while OneDrive
/// is `full`, a change that sends content and says nothing else waits for
/// space, as `Changes()` shows it. Held removals have their own question (the
/// mass-delete guard), and a row running is not kept back.
pub fn kept_reason(kind: OutboxKind, state: OutboxState, reason: Option<&Reason>, full: bool) -> Option<Reason> {
    let said = reason.filter(|r| !r.is_empty()).cloned();
    let for_space = || (full && kind.sends_content()).then_some(Reason::WaitingForSpace);
    match state {
        OutboxState::Blocked => Some(said.unwrap_or(Reason::Blocked)),
        OutboxState::Waiting | OutboxState::Retry => said.or_else(for_space),
        OutboxState::Ready => match said {
            Some(r) => r.waits_for_space().then_some(r),
            None => for_space(),
        },
        OutboxState::Running | OutboxState::Held => None,
    }
}

fn kept_group(group: &OutboxGroup, full: bool) -> Option<Reason> {
    kept_reason(group.kind(), group.state(), group.reason().as_ref(), full)
}

/// One row of `NotUploadedSummary()`: (group, reason, count, bytes).
pub type SummaryRow = (String, String, u32, u64);

/// `NotUploadedSummary()`: one row per reason, in the groups' order, then by
/// reason; `full` while OneDrive is full ([`kept_reason`]). From the store's
/// sums: nothing read from the disk.
pub fn summary(skipped: &[SkippedGroup], groups: &[OutboxGroup], full: bool) -> Vec<SummaryRow> {
    let mut by: BTreeMap<(Group, String), (u64, u64)> = BTreeMap::new();
    let mut add = |key: &str, blocked: bool, count: u64, bytes: u64| {
        let key = key.to_owned();
        let entry = by.entry((group_of(&key, blocked), key)).or_default();
        entry.0 = entry.0.saturating_add(count);
        entry.1 = entry.1.saturating_add(bytes);
    };
    for s in skipped {
        add(s.reason.key(), false, s.count, s.bytes);
    }
    for g in groups {
        if let Some(reason) = kept_group(g, full) {
            add(reason.key(), g.state() == OutboxState::Blocked, g.count, g.bytes);
        }
    }
    by.into_iter()
        .map(|((group, key), (count, bytes))| (group.as_str().to_owned(), key, u32::try_from(count).unwrap_or(u32::MAX), bytes))
        .collect()
}

/// `NotUploadedFiles(reason, limit)`: the files kept back for `reason` (a
/// key as the summary gives it), by path, at most `limit` (0 for all), each
/// with its reason as stored (a `400`'s carries the service's message); and
/// how many there are. Read with a `LIMIT` per group of rows.
pub fn files(store: &ReadStore<'_>, root: &Path, full: bool, reason: &str, limit: u32) -> Result<(Vec<(String, String)>, u32), TreeError> {
    let groups = store.outbox_groups()?;
    let kept: Vec<(&OutboxGroup, Reason)> =
        groups.iter().filter_map(|g| kept_group(g, full).map(|why| (g, why))).filter(|(_, why)| why.key() == reason).collect();
    let skipped: Vec<SkippedGroup> = store.skipped_groups()?.into_iter().filter(|s| s.reason.key() == reason).collect();
    let total = kept.iter().map(|(g, _)| g.count).chain(skipped.iter().map(|s| s.count)).sum::<u64>();
    let of: Vec<&OutboxGroup> = kept.iter().map(|(g, _)| *g).collect();
    let mut all: Vec<(String, String)> = store
        .outbox_places_of(&of, limit)?
        .into_iter()
        .map(|(rel, n)| (root.join(rel).display().to_string(), kept[n].1.to_string()))
        .collect();
    let reasons: Vec<_> = skipped.iter().map(|s| &s.reason).collect();
    all.extend(store.skipped_places_of(&reasons, limit)?.into_iter().map(|(rel, why)| (root.join(rel).display().to_string(), why.to_string())));
    all.sort();
    if limit > 0 {
        all.truncate(limit as usize);
    }
    Ok((all, u32::try_from(total).unwrap_or(u32::MAX)))
}

#[cfg(test)]
mod tests;
