//! What is kept back from OneDrive, grouped by what the user can do about it
//! (`NotUploadedSummary()`, `NotUploadedFiles()`; issue #20). Every reason a
//! change is kept back — an outbox row that is blocked, or waits with a
//! reason, and what the examination never uploads (`local_skipped`) — falls
//! into one [`Group`]; [`group_of`] is the one place that decides which.
//! A change that waits for space in OneDrive (issue #2) is kept back too,
//! though its row stays `ready` in its place: see [`kept_reason`].
//!
//! The window shows one line per reason where one action fixes every file of
//! it, and lists files only where something can be done to each.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use super::upload::{reason, space};
use crate::tree::outbox::{OutboxGroup, OutboxKind, OutboxState, SkippedGroup};
use crate::tree::{TreeError, TreeStore};

/// What the user can do about a reason, in the order the window shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// One action fixes every file of the reason: OneDrive full, a sign-in
    /// that does not allow writes.
    OneAction,
    /// Each file needs the user: a name OneDrive refuses, a file too large,
    /// refused by OneDrive with a message.
    PerFile,
    /// Never uploaded, and nothing to do: symbolic links, pipes, another device.
    Never,
    /// Goes up by itself.
    Waiting,
}

impl Group {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneAction => "one-action",
            Self::PerFile => "per-file",
            Self::Never => "never",
            Self::Waiting => "waiting",
        }
    }
}

/// The reason `refused: <the service's message>` is listed under, so that
/// every `400` is one reason with the service's text kept per file.
pub const REFUSED: &str = reason::REFUSED;

/// The key a reason is summed under: its code, `refused` for every
/// `refused: <message>`, and `too-big` for every `too-big:<needs>:<free>`.
pub fn reason_key(reason: &str) -> &str {
    if reason == REFUSED || reason.starts_with("refused: ") {
        REFUSED
    } else if space::parse_too_big(reason).is_some() {
        space::TOO_BIG_KEY
    } else {
        reason
    }
}

/// The group of a reason key ([`reason_key`]); `None` for one no code of
/// the daemon writes, which the caller shows as [`Group::Waiting`].
fn known_group(key: &str) -> Option<Group> {
    use super::local::examine::{NOT_SELECTED, OPEN_FOR_WRITING, OTHER_DEVICE};
    use reason::*;
    Some(match key {
        // `quota-exceeded` only until a start converts it to `waiting-for-space` (#2).
        QUOTA | space::WAITING | space::TOO_BIG_KEY | FORBIDDEN => Group::OneAction,
        // `not-selected`: a file where the selection syncs no files (issue #58) goes up once
        // it is moved into a chosen folder, or its folder is chosen.
        "name-characters" | "name-spaces" | "name-reserved" | "name-not-utf8" | "too-large" | REFUSED | NOT_SELECTED => Group::PerFile,
        // `reserved-name` is a `.konedrive-` name, which the daemon keeps for itself.
        "symlink" | "fifo" | "socket" | "device" | OTHER_DEVICE | "reserved-name" | "hard-link" | "ignored" => Group::Never,
        OPEN_FOR_WRITING | LOCKED | NOT_FOUND | NOT_LOCAL | CHANGED | PARENT | HASH | MOVE_OUT | NO_HELPER | UNREACHABLE
        | BACK_INSIDE | PLACE_UNKNOWN | DOWNLOAD | GONE_ONCE | STALE_HANDLE | GONE_UNPROVED | NO_LEASE | NETWORK | LOCAL_IO | STORE
        | FAILED => Group::Waiting,
        _ => return None,
    })
}

/// How many unknown reasons are remembered as logged; past it, none is
/// logged any more (a backoff's reason can be an error's own text).
const UNKNOWN_LOGGED: usize = 64;

/// The group of `key`; an unknown one is [`Group::Waiting`], logged once.
pub fn group_of(key: &str) -> Group {
    if let Some(group) = known_group(key) {
        return group;
    }
    static LOGGED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let mut logged = LOGGED.lock().unwrap_or_else(|p| p.into_inner());
    if logged.len() < UNKNOWN_LOGGED && logged.insert(key.to_owned()) {
        tracing::warn!("a change is kept back for a reason not in the table: {key:?}; shown as waiting");
    }
    Group::Waiting
}

/// The reason rows of `kind`, `state` and `reason` are kept back for, if
/// they are: blocked, or waiting (or in backoff) with a reason; ready but
/// waiting for space (`waiting-for-space`, `too-big:…`); and, while OneDrive
/// is `full`, a change that sends content and says nothing else waits for
/// space, as `Changes()` shows it. Held removals have their own question (the
/// mass-delete guard), and a row running is not kept back.
pub fn kept_reason(kind: OutboxKind, state: OutboxState, reason: Option<&str>, full: bool) -> Option<String> {
    let said = reason.filter(|r| !r.is_empty()).map(str::to_owned);
    let for_space = || (full && kind.sends_content()).then(|| space::WAITING.to_owned());
    match state {
        OutboxState::Blocked => Some(said.unwrap_or_else(|| "blocked".into())),
        OutboxState::Waiting | OutboxState::Retry => said.or_else(for_space),
        OutboxState::Ready => match said {
            Some(r) => space::waits(Some(&r)).then_some(r),
            None => for_space(),
        },
        OutboxState::Running | OutboxState::Held => None,
    }
}

fn kept_group(group: &OutboxGroup, full: bool) -> Option<String> {
    kept_reason(group.kind(), group.state(), group.reason().as_deref(), full)
}

/// One row of `NotUploadedSummary()`: (group, reason, count, bytes).
pub type SummaryRow = (String, String, u32, u64);

/// `NotUploadedSummary()`: one row per reason, in the groups' order, then by
/// reason; `full` while OneDrive is full ([`kept_reason`]). From the store's
/// sums: nothing read from the disk.
pub fn summary(skipped: &[SkippedGroup], groups: &[OutboxGroup], full: bool) -> Vec<SummaryRow> {
    let mut by: BTreeMap<(Group, String), (u64, u64)> = BTreeMap::new();
    let mut add = |reason: &str, count: u64, bytes: u64| {
        let key = reason_key(reason).to_owned();
        let entry = by.entry((group_of(&key), key)).or_default();
        entry.0 = entry.0.saturating_add(count);
        entry.1 = entry.1.saturating_add(bytes);
    };
    for s in skipped {
        add(&s.reason, s.count, s.bytes);
    }
    for g in groups {
        if let Some(reason) = kept_group(g, full) {
            add(&reason, g.count, g.bytes);
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
pub fn files(store: &TreeStore, root: &Path, full: bool, reason: &str, limit: u32) -> Result<(Vec<(String, String)>, u32), TreeError> {
    let groups = store.outbox_groups()?;
    let kept: Vec<(&OutboxGroup, String)> =
        groups.iter().filter_map(|g| kept_group(g, full).map(|why| (g, why))).filter(|(_, why)| reason_key(why) == reason).collect();
    let skipped: Vec<SkippedGroup> = store.skipped_groups()?.into_iter().filter(|s| reason_key(&s.reason) == reason).collect();
    let total = kept.iter().map(|(g, _)| g.count).chain(skipped.iter().map(|s| s.count)).sum::<u64>();
    let of: Vec<&OutboxGroup> = kept.iter().map(|(g, _)| *g).collect();
    let mut all: Vec<(String, String)> = store
        .outbox_places_of(&of, limit)?
        .into_iter()
        .map(|(rel, n)| (root.join(rel).display().to_string(), kept[n].1.clone()))
        .collect();
    let reasons: Vec<&str> = skipped.iter().map(|s| s.reason.as_str()).collect();
    all.extend(store.skipped_places_of(&reasons, limit)?.into_iter().map(|(rel, why)| (root.join(rel).display().to_string(), why)));
    all.sort();
    if limit > 0 {
        all.truncate(limit as usize);
    }
    Ok((all, u32::try_from(total).unwrap_or(u32::MAX)))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::tree::outbox::{Detection, OutboxKind, OutboxOp};
    use crate::tree::TreeStore;

    fn create(rel: &str, state: OutboxState, reason: Option<&str>) -> OutboxOp {
        OutboxOp::Record(Detection {
            kind: OutboxKind::Create,
            item_id: None,
            inode: None,
            rel: rel.into(),
            base: None,
            target_parent: None,
            target_name: Path::new(rel).file_name().map(|n| n.to_string_lossy().into_owned()),
            same_content: false,
            state,
            reason: reason.map(str::to_owned),
            next_try: None,
            size: None,
        })
    }

    /// 5000 files waiting for space in OneDrive, two too big for the space
    /// left, a few names it refuses, a 400, a symlink, a file open for
    /// writing and a reason no code writes: one summary row per reason in the
    /// table's groups, and the files of one reason capped with their total.
    /// While OneDrive is full, a change never tried waits for space too.
    #[test]
    fn a_full_onedrive_is_one_line_and_names_are_listed_per_file() {
        let mut store = TreeStore::in_memory().unwrap();
        let mut ops: Vec<OutboxOp> = (0..5000).map(|i| create(&format!("big/{i:05}.bin"), OutboxState::Ready, Some(space::WAITING))).collect();
        ops.push(create("huge1.iso", OutboxState::Ready, Some(&space::too_big(300, 20))));
        ops.push(create("huge2.iso", OutboxState::Ready, Some(&space::too_big(400, 20))));
        ops.push(create("a:b.txt", OutboxState::Blocked, Some("name-characters")));
        ops.push(create("c?d.txt", OutboxState::Blocked, Some("name-characters")));
        ops.push(create("CON", OutboxState::Blocked, Some("name-reserved")));
        ops.push(create("odd.txt", OutboxState::Blocked, Some("refused: The name is not allowed")));
        ops.push(create("open.odt", OutboxState::Waiting, Some("open-for-writing")));
        ops.push(create("queued.txt", OutboxState::Ready, None));
        ops.push(create("strange.txt", OutboxState::Retry, Some("something-new")));
        ops.push(OutboxOp::Skip { rel: PathBuf::from("link"), reason: "symlink".into(), size: 0 });
        store.outbox_apply(&ops, 1).unwrap();
        let (skipped, rows) = (store.skipped_groups().unwrap(), store.outbox_groups().unwrap());
        let root = Path::new("/nowhere/OneDrive");

        let got = summary(&skipped, &rows, false);
        let shown: Vec<(&str, &str, u32)> = got.iter().map(|(g, r, n, _)| (g.as_str(), r.as_str(), *n)).collect();
        assert_eq!(
            shown,
            vec![
                ("one-action", "too-big", 2),
                ("one-action", "waiting-for-space", 5000),
                ("per-file", "name-characters", 2),
                ("per-file", "name-reserved", 1),
                ("per-file", "refused", 1),
                ("never", "symlink", 1),
                ("waiting", "open-for-writing", 1),
                ("waiting", "something-new", 1),
            ]
        );

        let (items, total) = files(&store, root, false, space::WAITING, 20).unwrap();
        assert_eq!((items.len(), total), (20, 5000));
        assert_eq!(items[0], ("/nowhere/OneDrive/big/00000.bin".to_owned(), space::WAITING.to_owned()));
        let (items, _) = files(&store, root, false, "too-big", 0).unwrap();
        assert_eq!(items[0], ("/nowhere/OneDrive/huge1.iso".to_owned(), "too-big:300:20".to_owned()));
        let waiting = |full| summary(&skipped, &rows, full).into_iter().find(|(_, r, _, _)| r == space::WAITING).map(|(_, _, n, _)| n);
        assert_eq!((waiting(false), waiting(true)), (Some(5000), Some(5001)), "queued.txt waits for space while full");
        let (items, total) = files(&store, root, false, "name-characters", 0).unwrap();
        assert_eq!(total, 2);
        assert_eq!(items.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), vec!["/nowhere/OneDrive/a:b.txt", "/nowhere/OneDrive/c?d.txt"]);
        let (items, _) = files(&store, root, false, "refused", 20).unwrap();
        assert_eq!(items, vec![("/nowhere/OneDrive/odd.txt".to_owned(), "refused: The name is not allowed".to_owned())]);
        assert_eq!(files(&store, root, false, "no-such", 20).unwrap(), (vec![], 0));
    }

    /// Issue #87: the four keys a failure is stored under all wait.
    #[test]
    fn failure_keys_wait() {
        for key in [reason::NETWORK, reason::LOCAL_IO, reason::STORE, reason::FAILED] {
            assert_eq!(known_group(key), Some(Group::Waiting), "{key}");
        }
    }
}
