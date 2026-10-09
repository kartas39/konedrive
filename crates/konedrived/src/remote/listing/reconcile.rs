//! The reconcile of one cycle, for both modes: the folder made to follow
//! `staging`, the commit, and what follows it.
//!
//! [`Listing::reconcile`] waits for the folder's lease and runs
//! [`Reconcile::run`] on a blocking thread. `run` is three steps:
//!
//! 1. [`Reconcile::follow_mount`]: nothing is placed, since the folder's mount
//!    shows the store; the folder under the mount follows what changed
//!    ([`CycleStep::apply_staged_cycle`], the mount's);
//! 2. [`commit_cycle`]: `staging` swapped in with its link, or a first
//!    listing's page put into `items`. In read-write mode it decides what
//!    waits: the changes of what the outbox holds keep their base row;
//! 3. [`Reconcile::after_commit`]: the mount told ([`CycleStep::announce_committed_cycle`]),
//!    removals of items already gone dropped from the outbox, the activity
//!    and the conflicts recorded, and rows let wait for a folder made again.
//!
//! What a reconcile did in the folder stands whether or not its cycle goes
//! through: when step 2 fails, [`Reconcile::failed`] still records the copies
//! it made.
//!
//! The mode is [`Mode`] with a read-write cycle's load ([`RwCycle`]): a
//! read-only folder's reconcile, or a read-write folder's
//! (`docs/design/writes.md` §9), which holds the tree lock until the swap.
//!
//! [`CycleStep::apply_staged_cycle`]: crate::fuse::CycleStep::apply_staged_cycle
//! [`CycleStep::announce_committed_cycle`]: crate::fuse::CycleStep::announce_committed_cycle
//! [`Reconcile::run`]: crate::remote::listing::reconcile::Reconcile::run
//! [`Reconcile::follow_mount`]: crate::remote::listing::reconcile::Reconcile::follow_mount
//! [`Reconcile::after_commit`]: crate::remote::listing::reconcile::Reconcile::after_commit
//! [`Reconcile::failed`]: crate::remote::listing::reconcile::Reconcile::failed
//! [`commit_cycle`]: crate::remote::listing::reconcile::commit_cycle
//! [`RwCycle`]: crate::remote::listing::reconcile::RwCycle

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

use super::{cancellable, CycleError, DriveRecord, Listing, Turn, Writes};
use crate::folder::root::SyncRoot;
use crate::fuse::{Mount, OneDriveChange};
use crate::local::held::Rw;
use crate::remote::applied::{Applied, Changed, Copied, OnDisk};
use crate::remote::mode::Mode;
use crate::status::activity::{self, Kind};
use crate::status::report::Report;
use konedrive_tree::outbox::OutboxRow;
use konedrive_tree::reconcile::Deferrals;
use konedrive_tree::{Change, ConflictKind, ConflictRow, Store};

/// What a reconcile did.
#[derive(Debug, Default)]
pub(crate) struct Reconciled {
    pub applied: Applied,
}

impl Reconciled {
    /// What one page of a first listing did, added to what the pages before
    /// it did.
    pub(super) fn add(&mut self, page: Reconciled) {
        self.applied.add_page(page.applied);
    }
}

/// What a reconcile commits once the folder follows `staging`.
pub(crate) enum Commit {
    /// `staging` swapped in with the link to ask from next time.
    /// `listing`: the last page of a first listing shown page by page,
    /// which the one `listed` event stands for.
    Swap { link: String, listing: bool },
    /// One page of a first listing: its entries into `items`, with
    /// `next`, the link to the page after it.
    Page { changes: Vec<Change>, next: String },
}

/// What the activity log says of a reconcile, beside its conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Said {
    /// One `listed` event for the whole folder: the end of a first listing.
    Listed,
    /// What the reconcile did, item by item.
    EachChange,
    /// Nothing: a page of a first listing, which the `listed` event at its
    /// end stands for.
    Nothing,
}

/// What a read-write reconcile carries from its staging to its swap.
pub(crate) struct RwCycle<'a> {
    /// What the cycle shares with the folder's outbox worker.
    pub writes: &'a Writes,
    /// The tree lock, taken before `staging` was begun.
    pub tree: OwnedMutexGuard<()>,
    pub waiting: Waiting,
}

/// What a read-write commit goes by, besides what the reconcile did.
#[derive(Debug, Default, Clone)]
pub(crate) struct Waiting {
    /// `outbox_seq` as the fetch started: what is deferred is dated by it.
    pub fetch_seq: i64,
    /// The deferred changes staged again at the start: done with at the swap,
    /// or deferred anew.
    pub consumed: Vec<String>,
}

/// A read-write commit's rules: what the outbox holds, and what the cycle
/// carries to its swap.
pub(crate) struct Plan {
    rw: Arc<Rw>,
    waiting: Waiting,
}

/// The locks a reconcile holds until the folder is no longer being changed:
/// the cycle's turn, the folder's lease, and a read-write folder's tree lock.
pub(crate) struct Held {
    _turn: Turn,
    _lease: super::lease::Held,
    _tree: Option<OwnedMutexGuard<()>>,
}

/// A read-write reconcile's part: what it carries to its swap, and whom it
/// tells.
struct Writing {
    waiting: Waiting,
    /// Told the removals dropped because their item is already gone.
    dropped_removed: Arc<dyn Fn(Vec<OutboxRow>) + Send + Sync>,
}

/// One reconcile, ready to run on a blocking thread.
pub(crate) struct Reconcile {
    root: SyncRoot,
    store: Store,
    mount: Arc<Mount>,
    report: Report,
    /// The drive to write into `config.toml` and onto the folder first.
    drive: Option<(DriveRecord, String)>,
    /// The mode, with a read-write reconcile's part.
    mode: Mode<Writing>,
}

impl Listing {
    /// Makes the folder follow `staging` and commits it: swaps
    /// `staging` in with its link, or puts a first listing's page into
    /// `items` with the link to the next one. Under the folder's
    /// lease, on a blocking thread. Everything before this wrote only
    /// `staging` and `meta`, and needed no lock but, in read-write mode,
    /// the tree lock `mode` brings.
    ///
    /// The locks and the turn go into the blocking task: however this future
    /// ends, they are held until the folder is no longer being changed. The
    /// commit follows the change to the folder in the same task, so a page
    /// followed is a page committed unless the daemon dies in between.
    pub(super) async fn reconcile(&self, turn: &Turn, mode: Mode<RwCycle<'_>>, commit: Commit, cancel: &CancellationToken) -> Result<Reconciled, CycleError> {
        let lease = cancellable(cancel, self.ctx.lease.hold()).await?;
        let (tree, mode) = match mode {
            Mode::ReadOnly => (None, Mode::ReadOnly),
            Mode::ReadWrite(RwCycle { writes, tree, waiting }) => (Some(tree), Mode::ReadWrite(Writing { waiting, dropped_removed: Arc::clone(&writes.dropped_removed) })),
        };
        let reconcile = Reconcile {
            root: self.ctx.root.clone(),
            store: self.ctx.store.clone(),
            mount: Arc::clone(&self.ctx.mount),
            report: self.ctx.report.clone(),
            drive: crate::panic::lock(&self.pending_drive).take(),
            mode,
        };
        let held = Held { _turn: Arc::clone(turn), _lease: lease, _tree: tree };
        tokio::task::spawn_blocking(move || {
            let _held = held;
            reconcile.run(commit)
        })
        .await
        .map_err(|e| CycleError::Apply(format!("the reconcile task failed: {e}")))?
    }
}

impl Reconcile {
    /// The whole reconcile: the folder, the commit, and what follows it.
    fn run(mut self, commit: Commit) -> Result<Reconciled, CycleError> {
        self.record_drive();
        // While the drive's root has not come, nothing has a place yet.
        if self.store.call_blocking(move |s| s.root_item_id())?.is_none() {
            return before_the_root(&self.store, commit);
        }
        let plan = match &self.mode {
            Mode::ReadOnly => Mode::ReadOnly,
            Mode::ReadWrite(writing) => Mode::ReadWrite(Plan { rw: Arc::new(self.store.call_blocking(|s| Rw::read(s))?), waiting: writing.waiting.clone() }),
        };
        let done = self.follow_mount(plan.as_ref().map(|plan| Arc::clone(&plan.rw)))?;
        match commit_cycle(&self.store, plan.as_ref(), commit) {
            Ok(said) => {
                self.mount.cycle_step().announce_committed_cycle();
                self.after_commit(&done.applied, said);
                Ok(done)
            }
            // A commit that fails leaves what was done in the folder recorded and handed
            // over all the same.
            Err(e) => {
                self.failed(done.applied.on_disk);
                Err(e)
            }
        }
    }

    /// The account's drive written into `config.toml` and onto the
    /// folder (`docs/design/accounts.md` §6.1, §6.3), when this is the reconcile that learnt it.
    fn record_drive(&mut self) {
        let Some((record, id)) = self.drive.take() else { return };
        let recorded = crate::config::DriveId::new(id.as_str()).map(|drive| record.store.record_drive(&record.account, &drive));
        if let Some(Err(e)) = recorded {
            tracing::warn!("cannot record the account's drive in config.toml: {e}");
        }
        // Through the mount the folder takes no attribute: its own directory does.
        let root = SyncRoot { path: self.mount.backing_directory().resolve_root_path(), root_id: self.root.root_id.clone() };
        if let Err(e) = crate::folder::root::mark_drive(&root, &id) {
            tracing::warn!("cannot record the drive on {}: {e}", self.root.path.display());
        }
    }

    /// Step 1: nothing is placed, since the mount shows the store. The folder under the mount
    /// follows what changed ([`CycleStep::apply_staged_cycle`](crate::fuse::CycleStep::apply_staged_cycle)), and the changes are counted and named for
    /// the activity log.
    fn follow_mount(&self, rw: Mode<Arc<Rw>>) -> Result<Reconciled, CycleError> {
        let following = Arc::clone(self.mount.cycle_step());
        let outcome = self.store.call_blocking(move |s| following.apply_staged_cycle(s, rw.read_write().map(|rw| &**rw)))?;
        let mut applied = Applied::default();
        applied.on_disk.copies = outcome.conflicts.moved_aside.into_iter().map(|moved_aside| Copied { original: moved_aside.original, copy: moved_aside.copy }).collect();
        applied.on_disk.recreated = outcome.conflicts.kept_directories;
        for step in &outcome.changes {
            let (kind, rel, from) = match step {
                OneDriveChange::Appeared { now, .. } => (Kind::Added, now, None),
                OneDriveChange::Vanished { was, .. } => (Kind::Removed, was, None),
                OneDriveChange::Moved { was, now, .. } => (Kind::Moved, now, Some(was.clone())),
                OneDriveChange::Stayed { at, .. } if step.has_new_content() => (Kind::Updated, at, None),
                OneDriveChange::Stayed { .. } => continue,
            };
            match kind {
                Kind::Added => applied.counts.created += 1,
                Kind::Removed => applied.counts.deleted += 1,
                Kind::Moved => applied.counts.moved += 1,
                _ => applied.counts.updated += 1,
            }
            applied.changes.push(Changed { kind, rel: rel.clone(), from });
        }
        Ok(Reconciled { applied })
    }

    /// Step 3, once the tree the folder was made to follow is committed.
    fn after_commit(&self, applied: &Applied, said: Said) {
        if let Mode::ReadWrite(writing) = &self.mode {
            self.drop_removed(writing);
        }
        // Where each of the user's own files went is a conflict: a row
        // in `Conflicts.List()`, `Conflicts.Count` and a `conflict` event. It
        // is not a problem, so `LastError` does not say it. A page's are
        // written with the page, not at the end of the listing: a listing
        // that never ends must still say where the files went.
        record(&self.report, &self.store, &self.root.path, applied, said);
        self.hand_over(&applied.on_disk);
    }

    /// `items` just took this cycle's answer: a held or pending
    /// `delete` row whose item is not in it any more has
    /// nothing left to send.
    fn drop_removed(&self, writing: &Writing) {
        match self.store.call_blocking(move |s| s.outbox_drop_removed()) {
            Ok(dropped) if !dropped.is_empty() => {
                let events = dropped
                    .iter()
                    .map(|row| activity::event(Kind::Removed, self.root.path.join(&row.rel).display().to_string(), "already removed in OneDrive"))
                    .collect();
                self.report.activity.record_blocking(events);
                (writing.dropped_removed)(dropped);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("cannot drop held or pending removals of items already gone from OneDrive: {e}"),
        }
    }

    /// Read-write mode: rows wait for the folders the reconcile made the
    /// user's own (their `mkdir`).
    fn hand_over(&self, done: &OnDisk) {
        if self.mode.is_read_only() || done.recreated.is_empty() {
            return;
        }
        let recreated = done.recreated.clone();
        if let Err(e) = self.store.call_blocking(move |s| s.outbox_detach_parents(&recreated)) {
            tracing::warn!("cannot let the outbox wait for folders made again: {e}");
        }
    }

    /// What a reconcile whose commit failed did in the folder before it did, handed over
    /// and recorded all the same: its copies as conflicts.
    fn failed(&self, done: OnDisk) {
        self.hand_over(&done);
        record(&self.report, &self.store, &self.root.path, &Applied { on_disk: done, ..Applied::default() }, Said::Nothing);
    }
}

/// A commit before the drive's root has come. Nothing on a page has a place
/// yet: it waits in `items` like any entry whose folder has not come.
fn before_the_root(store: &Store, commit: Commit) -> Result<Reconciled, CycleError> {
    match commit {
        Commit::Page { changes, next } => {
            store.call_blocking(move |s| s.commit_page(&changes, &next))?;
            Ok(Reconciled::default())
        }
        Commit::Swap { .. } => Err(CycleError::Apply("the drive's listing has no root".into())),
    }
}

/// Step 2: the commit. `staging` is swapped in with its link, or a page
/// goes into `items` with the link to the next one.
///
/// With a plan (read-write mode) the swap keeps the base row of what the
/// outbox holds, and its change waits (the read-write reconcile
/// must, items 3 and 4). Never what is being removed.
pub(crate) fn commit_cycle(store: &Store, plan: Mode<&Plan>, commit: Commit) -> Result<Said, CycleError> {
    let (link, listing) = match commit {
        Commit::Page { changes, next } => {
            store.call_blocking(move |s| s.commit_page(&changes, &next))?;
            return Ok(Said::Nothing);
        }
        Commit::Swap { link, listing } => (link, listing),
    };
    match plan {
        Mode::ReadOnly => store.call_blocking(move |s| s.commit_staging(&link))?,
        Mode::ReadWrite(Plan { rw, waiting }) => {
            let changed = store.call_blocking(move |s| s.changed_ids())?;
            let defer: Vec<String> = changed.into_iter().filter(|id| !rw.removing.contains(id) && rw.held.contains(id)).collect();
            if !defer.is_empty() {
                tracing::debug!("{} change(s) wait for the folder to take them", defer.len());
            }
            let (consumed, fetched_at) = (waiting.consumed.clone(), waiting.fetch_seq);
            store.call_blocking(move |s| s.commit_staging_deferring(&link, &Deferrals { consumed: &consumed, whole: &defer, content: &[], fetched_at, waits: &[] }))?;
        }
    }
    Ok(if listing { Said::Listed } else { Said::EachChange })
}

/// What a reconcile records, on its blocking thread, right after the tree
/// it made the folder follow is committed: each
/// copy as a conflict — the row first, so that whoever hears of it can
/// already find it — and the activity `said`: one `listed` event for the end
/// of a first listing, what any other did item by
/// item, at most [`activity::PER_KIND`] of each kind plus one "and N more",
/// or nothing but the conflicts for a page of a first listing.
fn record(report: &Report, store: &Store, root: &Path, applied: &Applied, said: Said) {
    if said == Said::Nothing && applied.on_disk.copies.is_empty() {
        return;
    }
    let shown = |rel: &Path| root.join(rel).display().to_string();
    let folder = root.display().to_string();
    let mut events = match said {
        Said::Listed => {
            let listed = match store.call_blocking(move |s| s.listed_count()) {
                Ok(listed) => listed,
                Err(e) => {
                    tracing::warn!("cannot count what was listed: {e}");
                    0
                }
            };
            vec![activity::event(Kind::Listed, folder.clone(), activity::items(listed))]
        }
        Said::EachChange => {
            // A folder removed with everything in it is one removal: the
            // delta names every item inside it too.
            let removed: HashSet<&Path> = applied.changes.iter().filter(|c| c.kind == Kind::Removed).map(|c| c.rel.as_path()).collect();
            let went_with_its_folder = |c: &Changed| c.kind == Kind::Removed && c.rel.ancestors().skip(1).any(|above| removed.contains(above));
            let each = applied
                .changes
                .iter()
                .filter(|c| !went_with_its_folder(c))
                .map(|c| {
                    let from = c.from.as_deref().map(|from| format!("from {}", shown(from))).unwrap_or_default();
                    activity::event(c.kind, shown(&c.rel), from)
                })
                .collect();
            activity::capped(each, activity::PER_KIND, &folder)
        }
        Said::Nothing => Vec::new(),
    };
    let at = crate::clock::unix_now();
    // The copies (`docs/design/writes.md` §7): conflicts of kind `copy`, both versions in
    // the folder.
    let conflicts: Vec<ConflictRow> =
        applied.on_disk.copies.iter().map(|c| ConflictRow { at, original: shown(&c.original), rescued: shown(&c.copy), kind: ConflictKind::Copy }).collect();
    // Capped like every other kind; every conflict is
    // still a row.
    let each = conflicts.iter().map(|c| activity::event(Kind::Conflict, c.original.clone(), c.rescued.clone())).collect();
    events.extend(activity::capped(each, activity::PER_KIND, &folder));
    report.activity.add_conflicts(conflicts);
    report.activity.record_blocking(events);
}
