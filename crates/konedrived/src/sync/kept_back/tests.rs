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
    use crate::sync::local::examine::{MOUNTED_INSIDE, UNKNOWN_STATE};
    for key in [UNKNOWN_STATE, MOUNTED_INSIDE] {
        assert_eq!(group_of(reason_key(key)), Group::PerFile, "{key}");
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
