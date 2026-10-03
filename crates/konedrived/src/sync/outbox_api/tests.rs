use std::path::{Path, PathBuf};

use super::entries;
use crate::tree::outbox::{OutboxKind, OutboxRow, OutboxState};

fn row(seq: i64, state: OutboxState, reason: &str) -> OutboxRow {
    OutboxRow {
        seq,
        kind: OutboxKind::Create,
        item_id: None,
        inode: None,
        rel: PathBuf::from(format!("{seq}.bin")),
        base: None,
        target_parent: Some("R".into()),
        target_name: None,
        state,
        reason: Some(reason.into()).filter(|r: &String| !r.is_empty()),
        attempts: 0,
        next_try: Some(1_700_000_000),
        snapshot: Some("100 1".into()),
        session_url: None,
        session_expires: None,
        session_next: None,
        confirmed: false,
        size: None,
    }
}

/// While paused, every row that waits reads `paused` — a session stopped
/// by the pause, one never started, one in backoff — with no reason and no
/// next try; a blocked or held row keeps its own state.
#[test]
fn rows_waiting_while_paused_read_paused() {
    let rows = vec![
        row(1, OutboxState::Waiting, "paused"),
        row(2, OutboxState::Ready, ""),
        row(3, OutboxState::Retry, "error sending request"),
        row(4, OutboxState::Blocked, "quota-exceeded"),
        row(5, OutboxState::Held, "mass-delete"),
    ];
    let root = Path::new("/nowhere");
    let seen = |paused| entries(rows.clone(), root, &[], paused, false).into_iter().map(|e| (e.3, e.6, e.7)).collect::<Vec<_>>();
    let t = 1_700_000_000;
    assert_eq!(
        seen(true),
        vec![
            ("paused".into(), String::new(), 0),
            ("paused".into(), String::new(), 0),
            ("paused".into(), String::new(), 0),
            ("blocked".into(), "quota-exceeded".into(), t),
            ("held".into(), "mass-delete".into(), t),
        ]
    );
    assert_eq!(seen(false)[2], ("retry".into(), "error sending request".into(), t), "resumed, each row reads as it stands");
}
