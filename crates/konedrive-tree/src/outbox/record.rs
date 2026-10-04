//! A detection recorded: merged into the item's row, or a row of its own.

use rusqlite::Connection;

use super::{insert, remove, rewrite, rows_for, Base, Detection, OutboxKind, OutboxRow, OutboxState, Recorded};
use crate::TreeError;

fn new_row(d: &Detection, kind: OutboxKind, base: Option<Base>) -> OutboxRow {
    let removes = kind.removes();
    OutboxRow {
        seq: 0,
        kind,
        item_id: d.item_id.clone(),
        inode: d.inode.clone(),
        rel: d.rel.clone(),
        base,
        target_parent: if removes { None } else { d.target_parent.clone() },
        // A `move-out` keeps where the object was last proved to be: what a
        // later `ESTALE` is checked against.
        target_name: if removes && kind != OutboxKind::MoveOut { None } else { d.target_name.clone() },
        state: d.state,
        reason: d.reason.clone(),
        attempts: 0,
        next_try: d.next_try,
        snapshot: None,
        session_url: None,
        session_expires: None,
        session_next: None,
        confirmed: false,
        size: d.size,
    }
}

/// The kind a row of kind `row` becomes with a detection of kind `d`, or
/// `None` when the two cancel out (§3.5's table).
fn merged_kind(row: OutboxKind, d: &Detection) -> Option<OutboxKind> {
    use OutboxKind::*;
    Some(match (row, d.kind) {
        // Never sent: nothing to take back.
        (Create | Mkdir, Delete | MoveOut) => return None,
        // The newest content, at the newest place.
        (Create, _) => Create,
        (Mkdir, _) => Mkdir,
        (Update, Move) if d.same_content => Move,
        (Update, Move | Update | Create | Mkdir) => Update,
        // The base of the update is kept.
        (Update | Move | Delete | MoveOut, Delete) => Delete,
        (Update | Move | Delete | MoveOut, MoveOut) => MoveOut,
        (Move, Update | Create) => Update,
        (Move, Move | Mkdir) => Move,
        // Save-by-rename over a deleted name, or the item back again.
        (Delete | MoveOut, Update | Create) => Update,
        (Delete | MoveOut, Move | Mkdir) => Move,
    })
}

/// The row `existing` with detection `d` merged in; `None` when it goes.
fn merge(existing: &OutboxRow, d: &Detection) -> Option<OutboxRow> {
    let kind = merged_kind(existing.kind, d)?;
    let base = existing.base.clone().or_else(|| d.base.clone());
    let mut row = new_row(d, kind, base);
    row.seq = existing.seq;
    row.item_id = existing.item_id.clone().or_else(|| d.item_id.clone());
    row.inode = d.inode.clone().or_else(|| existing.inode.clone());
    row.attempts = existing.attempts;
    row.size = d.size.or(existing.size);
    // A confirmed removal stays confirmed while it is still one.
    row.confirmed = existing.confirmed && kind.removes();
    if row.confirmed && d.state == OutboxState::Held {
        row.state = existing.state;
        row.reason = existing.reason.clone();
    }
    if kind == OutboxKind::Move && d.at_base(row.base.as_ref()) {
        return None;
    }
    // On its way through a temporary name: the row keeps it while the
    // detection still sees the object where the row was taking it, so that
    // a replay looks for the item there (F55 (7) (b)).
    let swapping = existing.swap_name().is_some();
    if swapping && kind == existing.kind && d.rel == existing.rel && d.target_parent.as_ref().is_none_or(|p| existing.target_parent.as_ref() == Some(p)) {
        row.target_parent = existing.target_parent.clone();
        row.target_name = existing.target_name.clone();
    }
    // A failed row keeps its backoff, and a held delete stays held, unless the
    // detection itself waits, is blocked or is held.
    match d.state {
        OutboxState::Waiting | OutboxState::Blocked | OutboxState::Held => {}
        _ if existing.state == OutboxState::Retry => {
            row.state = OutboxState::Retry;
            row.reason = existing.reason.clone();
            row.next_try = existing.next_try;
        }
        _ if existing.state == OutboxState::Held && kind.removes() => {
            row.state = OutboxState::Held;
            row.reason = existing.reason.clone();
        }
        _ => {}
    }
    // An upload session belongs to one object, one place and one kind of
    // request; the snapshot is checked against the file before a resume, so
    // new content alone does not cost the progress.
    let same_object = match (&row.inode, &existing.inode) {
        (Some(a), Some(b)) => a.same_object(b),
        (None, None) => true,
        _ => false,
    };
    if kind == existing.kind && same_object && row.target() == existing.target() {
        row.snapshot = existing.snapshot;
        row.session_url = existing.session_url.clone();
        row.session_expires = existing.session_expires;
        row.session_next = existing.session_next;
    }
    Some(row)
}

/// The row a detection makes behind a running one of the same item, or
/// `None` when the running row already does what it says.
fn follow_up(running: &OutboxRow, d: &Detection) -> Option<OutboxRow> {
    use OutboxKind::*;
    let pending_create = matches!(running.kind, Create | Mkdir);
    let kind = match d.kind {
        // Content again: a running row's snapshot is compared by the
        // examination, which only says so when it moved on since.
        Create | Update => Update,
        Mkdir | Move if d.target() == running.target() => return None,
        Mkdir | Move => Move,
        Delete | MoveOut if running.kind.removes() => return None,
        other => other,
    };
    // Behind a create the item has no id or base yet: the commit fills them.
    let base = if pending_create { None } else { running.base.clone() };
    Some(new_row(d, kind, base))
}

pub(super) fn record(conn: &Connection, d: &Detection) -> Result<Recorded, TreeError> {
    let live = rows_for(conn, d.item_id.as_deref(), d.inode.as_ref())?;
    let running = live.iter().find(|row| row.state == OutboxState::Running);
    let pending = live.iter().rev().find(|row| row.state != OutboxState::Running);
    if let Some(existing) = pending {
        return Ok(match merge(existing, d) {
            Some(row) => {
                rewrite(conn, &row)?;
                Recorded::Merged(row.seq)
            }
            None => {
                remove(conn, existing.seq)?;
                Recorded::Removed(existing.seq)
            }
        });
    }
    if let Some(running) = running {
        return Ok(match follow_up(running, d) {
            Some(row) => Recorded::Inserted(insert(conn, &row)?),
            None => Recorded::Nothing,
        });
    }
    if d.kind == OutboxKind::Move && d.at_base(d.base.as_ref()) {
        return Ok(Recorded::Nothing);
    }
    Ok(Recorded::Inserted(insert(conn, &new_row(d, d.kind, d.base.clone()))?))
}
