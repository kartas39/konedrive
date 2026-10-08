//! A local change recorded in the outbox: the mass-delete guard, the order of the rows, and
//! the one transaction that writes them.
//!
//! Whoever saw the change — the mount, as a program's request is answered — says what it is
//! ([`Detection`]); what is done about it is decided here, from the store alone.

use std::collections::{BTreeSet, HashMap, HashSet};

use konedrive_tree::outbox::{Detection, OutboxApplied, OutboxKind, OutboxOp, OutboxState, Reason};
use konedrive_tree::{Kind, Table, TreeError, TreeStore};

use super::{MASS_DELETE_FLOOR, MASS_DELETE_ITEMS, MASS_DELETE_PERCENT};

/// What recording did.
#[derive(Debug, Default)]
pub struct Recorded {
    pub applied: OutboxApplied,
    /// How many items the mass-delete guard holds back, when this change tripped it.
    pub held: u64,
}

/// Records `detections`, with `ops` beside them, in one transaction.
///
/// The mass-delete guard counts the items the detections remove from OneDrive together with
/// the removals still waiting in the outbox, so that a trickle adds up: each item once, a
/// folder with everything the base has inside it, and nothing the user confirmed. When it
/// trips on something new, every removal not confirmed is held.
///
/// Rows are written freers first ([`ordered`]).
pub fn record(s: &mut TreeStore, mut detections: Vec<Detection>, ops: Vec<OutboxOp>, now: i64) -> Result<Recorded, TreeError> {
    let mut held = 0;
    let mut holds = Vec::new();
    if detections.iter().any(|d| d.kind.removes() && d.item_id.is_some()) {
        let rows = s.outbox_rows()?;
        let confirmed: HashSet<&str> = rows.iter().filter(|r| r.kind.removes() && r.confirmed).filter_map(|r| r.item_id.as_deref()).collect();
        let waiting = || rows.iter().filter(|r| r.kind.removes() && !r.confirmed && r.state != OutboxState::Running);
        let new = || detections.iter().filter(|d| d.kind.removes()).filter_map(|d| d.item_id.as_deref()).filter(|id| !confirmed.contains(id));
        let mut removed: HashSet<String> = HashSet::new();
        for id in new().chain(waiting().filter_map(|r| r.item_id.as_deref())) {
            if removed.insert(id.to_owned()) && s.get(Table::Items, id)?.is_some_and(|base| base.kind == Kind::Folder) {
                removed.extend(s.descendants(Table::Items, id)?);
            }
        }
        let n = removed.len() as u64;
        // The items in the folder are counted only when the share decides.
        let trips = new().next().is_some() && (n > MASS_DELETE_ITEMS || (n >= MASS_DELETE_FLOOR && n * 100 > s.counts()?.placed * MASS_DELETE_PERCENT));
        if trips {
            tracing::warn!("{n} items would be removed from OneDrive; held until confirmed");
            holds = waiting().filter(|r| r.state != OutboxState::Held).map(|r| OutboxOp::Hold { seq: r.seq, reason: Reason::MassDelete }).collect();
            let unconfirmed: HashSet<String> = new().map(str::to_owned).collect();
            for d in detections.iter_mut().filter(|d| d.kind.removes() && d.item_id.as_ref().is_some_and(|id| unconfirmed.contains(id))) {
                d.state = OutboxState::Held;
                d.reason = Some(Reason::MassDelete);
                d.next_try = None;
            }
            held = n;
        }
    }
    let (first, rest): (Vec<OutboxOp>, Vec<OutboxOp>) = ops.into_iter().partition(|op| matches!(op, OutboxOp::Rebase { .. } | OutboxOp::Remove(_)));
    let mut all = first;
    all.extend(ordered(detections).into_iter().map(OutboxOp::Record));
    all.extend(rest);
    all.extend(holds);
    Ok(Recorded { applied: s.outbox_apply(&all, now)?, held })
}

/// Whether a detection takes its item away from its base place.
fn moves_away(d: &Detection) -> bool {
    matches!(d.kind, OutboxKind::Move | OutboxKind::Update)
        && d.base.as_ref().is_some_and(|b| (b.parent.as_deref(), b.name.as_deref()) != (d.target_parent.as_deref(), d.target_name.as_deref()))
}

/// The OneDrive (parent, name) a detection frees and takes, compared
/// without case as OneDrive does.
fn frees(d: &Detection) -> Option<(String, String)> {
    if !(d.kind.removes() || moves_away(d)) {
        return None;
    }
    let base = d.base.as_ref()?;
    Some((base.parent.clone()?, base.name.as_ref()?.to_lowercase()))
}

fn takes(d: &Detection) -> Option<(String, String)> {
    if !(matches!(d.kind, OutboxKind::Mkdir | OutboxKind::Create) || moves_away(d)) {
        return None;
    }
    Some((d.target_parent.clone()?, d.target_name.as_ref()?.to_lowercase()))
}

/// A batch's detections in the order their rows are written: every row that
/// frees a name before a row that takes it, otherwise shallowest first and
/// in the order found. A cycle (a swap) is broken at its first row.
pub(crate) fn ordered(detections: Vec<Detection>) -> Vec<Detection> {
    let n = detections.len();
    let mut freeing: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, d) in detections.iter().enumerate() {
        if let Some(key) = frees(d) {
            freeing.entry(key).or_default().push(i);
        }
    }
    let mut after: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut before = vec![0usize; n];
    for (t, d) in detections.iter().enumerate() {
        for &f in takes(d).and_then(|key| freeing.get(&key)).map(Vec::as_slice).unwrap_or_default() {
            if f != t {
                after[f].push(t);
                before[t] += 1;
            }
        }
    }
    let priority = |i: usize| (detections[i].rel.components().count(), i);
    let mut ready: BTreeSet<(usize, usize)> = (0..n).filter(|&i| before[i] == 0).map(priority).collect();
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        let next = match ready.pop_first() {
            Some((_, i)) if done[i] => continue,
            Some((_, i)) => i,
            None => (0..n).filter(|&i| !done[i]).min_by_key(|&i| priority(i)).expect("a row is left"),
        };
        done[next] = true;
        order.push(next);
        for &t in &after[next] {
            before[t] = before[t].saturating_sub(1);
            if before[t] == 0 && !done[t] {
                ready.insert(priority(t));
            }
        }
    }
    let mut slots: Vec<Option<Detection>> = detections.into_iter().map(Some).collect();
    order.into_iter().map(|i| slots[i].take().expect("each row once")).collect()
}

#[cfg(test)]
mod tests;
