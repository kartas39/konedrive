use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::local::entry::Type;
use crate::local::{MASS_DELETE_FLOOR, MASS_DELETE_ITEMS, MASS_DELETE_PERCENT};
use konedrive_tree::outbox::{Detection, OutboxKind, OutboxOp, OutboxRow, OutboxState};
use konedrive_tree::{Kind, Table, TreeError};

use super::{depth, Examined, ExamineError, MASS_DELETE, Run};

impl Run<'_, '_> {
    /// `local_skipped` lists what is there now: rows for examined places
    /// that no longer qualify go — after a Full scan, every such row.
    pub(super) fn tidy_skipped(&mut self, full: bool) -> Result<(), ExamineError> {
        for s in self.store(|s| s.local_skipped())? {
            let dir = s.rel.parent().unwrap_or(Path::new(""));
            let examined = !self.unreadable.contains(&s.rel)
                && (full
                    || self.whole.contains(dir)
                    || self.named.get(dir).is_some_and(|names| s.rel.file_name().is_some_and(|n| names.contains(n))));
            if examined && !self.skipped.contains_key(&s.rel) {
                self.ops.push(OutboxOp::Unskip(s.rel));
            }
        }
        let skipped = std::mem::take(&mut self.skipped);
        let ops: Vec<OutboxOp> = skipped
            .into_iter()
            .map(|(rel, reason)| {
                // A file's size, for the sums of what is kept back; nothing for the rest.
                let size = self.at.get(&rel).map(|&i| &self.entries[i]).filter(|e| e.ty == Type::File).map_or(0, |e| e.size);
                OutboxOp::Skip { rel, reason, size }
            })
            .collect();
        self.ops.extend(ops);
        Ok(())
    }

    /// Adds item `id` and, for a folder, everything the base has inside it.
    fn count_removed(&mut self, id: &str, removed: &mut HashSet<String>) -> Result<(), TreeError> {
        if !removed.insert(id.to_owned()) {
            return Ok(());
        }
        if self.base_row(id)?.is_some_and(|base| base.kind == Kind::Folder) {
            removed.extend(self.store({ let id = id.to_owned(); move |s| s.descendants(Table::Items, &id) })?);
        }
        Ok(())
    }

    /// The mass-delete guard, then everything in one transaction.
    ///
    /// The guard counts the items removed from OneDrive — by deletes and
    /// moves out alike, the Trash included (§4.6) — by this batch and by the
    /// removals still waiting in the outbox, so that a trickle adds up. Each
    /// item counts once, and removals the user confirmed count no more.
    /// When it trips on something new, every removal not confirmed is
    /// held.
    ///
    /// Rows are written freers first: a row that frees a name in OneDrive
    /// before the row that takes it, otherwise shallowest first, in the order
    /// found.
    pub(super) fn finish(mut self) -> Result<Examined, ExamineError> {
        let confirmed: HashSet<String> = self.rows.iter().filter(|r| r.kind.removes() && r.confirmed).filter_map(|r| r.item_id.clone()).collect();
        let waiting: Vec<OutboxRow> = self
            .rows
            .iter()
            .filter(|r| r.kind.removes() && !r.confirmed && r.state != OutboxState::Running)
            .filter(|r| !r.item_id.as_ref().is_some_and(|i| self.decided.contains(i)))
            .cloned()
            .collect();
        let new: Vec<String> = self
            .detections
            .iter()
            .filter(|d| d.kind.removes())
            .filter_map(|d| d.item_id.clone())
            .filter(|id| !confirmed.contains(id))
            .collect();
        let fresh = !new.is_empty() || waiting.iter().any(|r| r.state != OutboxState::Held);
        let mut removed: HashSet<String> = HashSet::new();
        for id in new.iter().chain(waiting.iter().filter_map(|r| r.item_id.as_ref())) {
            self.count_removed(id, &mut removed)?;
        }
        let n = removed.len() as u64;
        // The items in the folder — a walk of the whole tree — counted only
        // when the share decides (issue #39).
        let trips = fresh
            && (n > MASS_DELETE_ITEMS || (n >= MASS_DELETE_FLOOR && n * 100 > self.store(|s| s.counts())?.placed * MASS_DELETE_PERCENT));
        if trips {
            tracing::warn!("{n} items would be removed from OneDrive; held until confirmed");
            for d in self.detections.iter_mut().filter(|d| d.kind.removes() && d.item_id.as_ref().is_some_and(|id| !confirmed.contains(id))) {
                d.state = OutboxState::Held;
                d.reason = Some(MASS_DELETE.into());
                d.next_try = None;
            }
            for row in waiting.iter().filter(|r| r.state != OutboxState::Held) {
                self.ops.push(OutboxOp::Hold { seq: row.seq, reason: MASS_DELETE.into() });
            }
            self.out.held = n;
        }
        let detections = ordered(std::mem::take(&mut self.detections));
        let mut ops: Vec<OutboxOp> = Vec::new();
        let (first, rest): (Vec<OutboxOp>, Vec<OutboxOp>) =
            std::mem::take(&mut self.ops).into_iter().partition(|op| matches!(op, OutboxOp::Rebase { .. } | OutboxOp::Remove(_)));
        ops.extend(first);
        ops.extend(detections.into_iter().map(OutboxOp::Record));
        ops.extend(rest);
        let now = self.ex.now;
        self.out.applied = self.ex.store.call_blocking(move |s| s.outbox_apply(&ops, now))?;
        Ok(self.out)
    }
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
fn ordered(detections: Vec<Detection>) -> Vec<Detection> {
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
    let priority = |i: usize| (depth(&detections[i].rel), i);
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
