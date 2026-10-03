use std::path::PathBuf;

use super::*;
use konedrive_tree::outbox::{Detection, OutboxKind, OutboxOp};
use konedrive_tree::TreeStore;

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

/// Issue #104: what keeps a folder no longer synced here on disk needs
/// the user, and is shown where blocked rows are.
#[test]
fn what_keeps_a_leaving_folder_is_blocked() {
    use crate::local::examine::{MOUNTED_INSIDE, UNKNOWN_STATE};
    for key in [UNKNOWN_STATE, MOUNTED_INSIDE] {
        assert_eq!(group_of(reason_key(key), false), Group::PerFile, "{key}");
    }
    let mut store = TreeStore::in_memory().unwrap();
    store.outbox_apply(&[OutboxOp::Skip { rel: PathBuf::from("docs/u.txt"), reason: UNKNOWN_STATE.into(), size: 0 }], 1).unwrap();
    let got = summary(&store.skipped_groups().unwrap(), &store.outbox_groups().unwrap(), false);
    assert_eq!(got.iter().map(|(g, r, n, _)| (g.as_str(), r.as_str(), *n)).collect::<Vec<_>>(), vec![("per-file", UNKNOWN_STATE, 1)]);
}

/// Issue #87: the four keys a failure is stored under all wait.
#[test]
fn failure_keys_wait() {
    for key in [reason::NETWORK, reason::LOCAL_IO, reason::STORE, reason::FAILED] {
        assert_eq!(known_group(key), Some(Group::Waiting), "{key}");
    }
}

/// UP3. A blocked row needs the user (`BlockedCount`): whatever the worker
/// blocked it with, it is never listed among the changes that "go up by
/// themselves".
#[test]
fn a_blocked_row_is_never_shown_as_going_up_by_itself() {
    // What the worker blocks a row with, beside the names OneDrive refuses, `refused: …`,
    // `forbidden` and what keeps a leaving folder: `steps.rs`, `content.rs`, `move_out.rs` —
    // the last one an error's own text (`content.rs`, a state that cannot be read);
    // and `blocked`, which `kept_reason` gives a blocked row that has no reason.
    let reasons = ["no-name", "no-item", "no-guard", "no-handle", "bad-handle", "another-item", "f6.txt carries a state konedrive cannot read"];
    let mut store = TreeStore::in_memory().unwrap();
    let mut ops: Vec<OutboxOp> = reasons.iter().enumerate().map(|(i, why)| create(&format!("f{i}.txt"), OutboxState::Blocked, Some(why))).collect();
    ops.push(create("bare.txt", OutboxState::Blocked, None));
    store.outbox_apply(&ops, 1).unwrap();
    let got = summary(&store.skipped_groups().unwrap(), &store.outbox_groups().unwrap(), false);
    let waiting: Vec<&str> = got.iter().filter(|(group, ..)| group == Group::Waiting.as_str()).map(|(_, reason, ..)| reason.as_str()).collect();
    assert!(waiting.is_empty(), "blocked rows listed as waiting, by reason: {waiting:?}");
    assert_eq!(got.iter().map(|(.., n, _)| *n).sum::<u32>(), 8, "{got:?}");
}

/// UP3. Every reason the worker itself writes has its group in the table:
/// none is "a reason not in the table", logged as unknown.
#[test]
fn every_reason_the_worker_writes_is_in_the_table() {
    let written = [
        // Constants of `upload::reason` the worker writes.
        reason::PAUSED.to_owned(),
        reason::SESSION_OPEN.to_owned(),
        reason::NAME_HELD.to_owned(),
        // Sentences (`steps.rs`, `engine/drain.rs`, `content.rs`).
        "changed in OneDrive again and again".to_owned(),
        "changing in OneDrive again and again".to_owned(),
        "the upload session ended twice".to_owned(),
        "not allowed now: the folder is read-only".to_owned(),
        // A key with the error behind it (`move_out.rs`).
        format!("{}: errno 5", reason::DOWNLOAD),
        format!("{}: errno 5", reason::UNREACHABLE),
        format!("{}: Resource temporarily unavailable", reason::NOT_LOCAL),
        format!("{}: Function not implemented", reason::NO_LEASE),
    ];
    let unknown: Vec<&str> = written.iter().map(String::as_str).filter(|why| known_group(reason_key(why)).is_none()).collect();
    assert!(unknown.is_empty(), "not in the table: {unknown:?}");
}
