use std::collections::{BTreeSet, HashMap, HashSet};

use crate::local::entry::Type;
use crate::local::{MASS_DELETE_FLOOR, MASS_DELETE_ITEMS, MASS_DELETE_PERCENT};
use std::path::{Path, PathBuf};

use konedrive_tree::outbox::{Detection, LocalSkip, OutboxKind, OutboxOp, OutboxRow, OutboxState, Reason};
use konedrive_tree::{Kind, TreeError};

use super::{depth, Examined, ExamineError, Run};
use crate::folder::disk::daemon_owned;

impl Run<'_, '_, '_> {
    /// `local_skipped` lists what is there now: a line for a place this run
    /// examined ([`examined`](Self::examined)) that no longer qualifies goes.
    /// A line at or inside a place that was not examined stays: what could
    /// not be looked at is not known to be gone. Unless the nearest place
    /// above it that was examined holds nothing: what is below a directory
    /// that is not there (deleted, moved out, taken away by a change from
    /// OneDrive) is not there either.
    ///
    /// A line below a directory this run found renamed follows it
    /// ([`moved`](Self::moved)): it is judged at its new place, and one that
    /// says "cannot be read" is asked for again there.
    pub(super) fn tidy_skipped(&mut self) -> Result<(), ExamineError> {
        let mut follow = Vec::new();
        for s in self.facts.skipped()? {
            let new = self.moved(&s.rel);
            let rel = new.clone().unwrap_or_else(|| s.rel.clone());
            if self.outcome.skipped.contains_key(&rel) {
                // Listed again by this run, at the place it is now.
                self.outcome.ops.extend(new.map(|_| OutboxOp::Unskip(s.rel)));
                continue;
            }
            let above_is_gone = || rel.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()).find(|a| self.examined(a)).is_some_and(|a| self.listing.at(a).is_none());
            if self.examined(&rel) || above_is_gone() {
                self.outcome.ops.push(OutboxOp::Unskip(s.rel));
                continue;
            }
            if new.is_some() {
                if s.reason == LocalSkip::Unreadable {
                    if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
                        self.outcome.out.passed.name(parent, name);
                    }
                    self.outcome.out.passed.tree(&rel);
                }
                follow.push((s.rel, rel));
            }
        }
        // After the lines taken off above, before the lines written below.
        if !follow.is_empty() {
            self.outcome.ops.push(OutboxOp::MoveSkipped(follow));
        }
        let listing = self.listing;
        let skipped = std::mem::take(&mut self.outcome.skipped);
        self.outcome.ops.extend(skipped.into_iter().map(|(rel, reason)| {
            // A file's size, for the sums of what is kept back; nothing for the rest.
            let size = listing.at(&rel).map(|ix| &listing[ix]).filter(|e| e.ty == Type::File).map_or(0, |e| e.size);
            OutboxOp::Skip { rel, reason, size }
        }));
        Ok(())
    }

    /// Where `rel` is now, if it is below a directory this run found
    /// renamed (the run's own `Rebase`, by the deepest such directory):
    /// `None` when it is below none, or when the new place is the daemon's
    /// own.
    fn moved(&self, rel: &Path) -> Option<PathBuf> {
        let below = |from: &Path| rel.strip_prefix(from).ok().filter(|rest| !rest.as_os_str().is_empty());
        let moves = self.outcome.ops.iter().filter_map(|op| match op {
            OutboxOp::Rebase { from, to } => below(from).map(|rest| (depth(from), to.join(rest))),
            _ => None,
        });
        let (_, new) = moves.max_by_key(|(deep, _)| *deep)?;
        (!new.components().any(|c| daemon_owned(c.as_os_str()))).then_some(new)
    }

    /// Adds item `id` and, for a folder, everything the base has inside it.
    fn count_removed(&mut self, id: &str, removed: &mut HashSet<String>) -> Result<(), TreeError> {
        if !removed.insert(id.to_owned()) {
            return Ok(());
        }
        if self.facts.row(id)?.is_some_and(|base| base.kind == Kind::Folder) {
            removed.extend(self.facts.descendants(id)?);
        }
        Ok(())
    }

    /// The mass-delete guard, then everything in one transaction.
    ///
    /// The guard counts the items removed from OneDrive — by deletes and
    /// moves out alike, the Trash included (`docs/design/writes.md` §4.5) — by this batch and by the
    /// removals still waiting in the outbox, so that a trickle adds up. Each
    /// item counts once, and removals the user confirmed count no more.
    /// When it trips on something new, every removal not confirmed is
    /// held.
    ///
    /// Rows are written freers first: a row that frees a name in OneDrive
    /// before the row that takes it, otherwise shallowest first, in the order
    /// found. What is said in Activity (an empty copy removed) is written
    /// right after the rows, in a call of its own, also when the rows could
    /// not be written: the removal happened.
    pub(super) fn finish(mut self) -> Result<Examined, ExamineError> {
        let confirmed: HashSet<String> = self.facts.rows.iter().filter(|r| r.kind.removes() && r.confirmed).filter_map(|r| r.item_id.clone()).collect();
        let waiting: Vec<OutboxRow> = self
            .facts
            .rows
            .iter()
            .filter(|r| r.kind.removes() && !r.confirmed && r.state != OutboxState::Running)
            .filter(|r| !r.item_id.as_ref().is_some_and(|i| self.decisions.settled(i).is_some()))
            .cloned()
            .collect();
        let new: Vec<String> = self
            .outcome
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
        // when the share decides.
        let trips = fresh
            && (n > MASS_DELETE_ITEMS || (n >= MASS_DELETE_FLOOR && n * 100 > self.facts.placed()? * MASS_DELETE_PERCENT));
        if trips {
            tracing::warn!("{n} items would be removed from OneDrive; held until confirmed");
            for d in self.outcome.detections.iter_mut().filter(|d| d.kind.removes() && d.item_id.as_ref().is_some_and(|id| !confirmed.contains(id))) {
                d.state = OutboxState::Held;
                d.reason = Some(Reason::MassDelete);
                d.next_try = None;
            }
            for row in waiting.iter().filter(|r| r.state != OutboxState::Held) {
                self.outcome.ops.push(OutboxOp::Hold { seq: row.seq, reason: Reason::MassDelete });
            }
            self.outcome.out.held = n;
        }
        let detections = ordered(std::mem::take(&mut self.outcome.detections));
        let mut ops: Vec<OutboxOp> = Vec::new();
        let (first, rest): (Vec<OutboxOp>, Vec<OutboxOp>) =
            std::mem::take(&mut self.outcome.ops).into_iter().partition(|op| matches!(op, OutboxOp::Rebase { .. } | OutboxOp::Remove(_)));
        ops.extend(first);
        ops.extend(detections.into_iter().map(OutboxOp::Record));
        ops.extend(rest);
        let now = self.ex.now;
        let applied = self.ex.store.call_blocking(move |s| s.outbox_apply(&ops, now));
        let said = std::mem::take(&mut self.outcome.activity);
        if !said.is_empty() {
            if let Err(err) = self.ex.store.call_blocking(move |s| s.add_activity(&said)) {
                tracing::warn!("cannot record an activity event: {err}");
            }
        }
        self.outcome.out.applied = applied?;
        Ok(self.outcome.out)
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
