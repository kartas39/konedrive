//! The reconcile of one cycle, for both modes: the folder made to match
//! `staging`, the commit, and what follows it.
//!
//! [`Listing::reconcile`] waits for the folder's lease and runs
//! [`Reconcile::run`] on a blocking thread. `run` is three steps:
//!
//! 1. [`Reconcile::apply_with_handover`]: the materializer over the scope,
//!    and a Full pass after a Changed one that found the folder not matching
//!    the stored tree;
//! 2. [`commit_cycle`]: `staging` swapped in with its link, or a first
//!    listing's page put into `items`. In read-write mode it decides what
//!    waits: the changes the disk did not take keep their base row;
//! 3. [`Reconcile::after_commit`]: removals of items already gone dropped
//!    from the outbox, the activity and the conflicts recorded, the watcher
//!    told what to examine and rows let wait for a folder made again.
//!
//! What a reconcile did on disk stands whether or not its cycle goes
//! through: when step 1 or step 2 fails, [`Reconcile::failed`] still records
//! the rescues and copies and hands the places over to the watcher.
//!
//! The mode is [`Mode`] with a read-write cycle's load ([`RwCycle`]): the
//! read phase's reconcile, or a read-write folder's (`docs/design/writes.md`
//! §9), which holds the tree lock until the swap.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

use super::{applying, cancellable, CycleError, DriveRecord, Listing, Turn, Writes};
use crate::folder::disk::{rescue_base, rescue_stamp, Disk};
use crate::folder::locks::InodeLocks;
use crate::folder::root::SyncRoot;
use crate::helper::HelperLink;
use crate::local::{Batch, IgnoreList};
use crate::remote::mode::Mode;
use crate::remote::materialize::{Applied, Changed, Claimed, Failed, Kept, Materializer, OnDisk, Rw, Scope};
use crate::status::activity::{self, Kind};
use crate::status::report::Report;
use konedrive_tree::outbox::OutboxRow;
use konedrive_tree::reconcile::Deferrals;
use konedrive_tree::{Change, ConflictKind, ConflictRow, Store};

/// What a reconcile did.
#[derive(Debug, Default)]
pub(crate) struct Reconciled {
    pub applied: Applied,
    /// A Full reconcile ran — asked for, or handed over to.
    pub full: bool,
}

impl Reconciled {
    /// What one page of a first listing did, added to what the pages before
    /// it did.
    pub(super) fn add(&mut self, page: Reconciled) {
        self.applied.add_page(page.applied);
        self.full |= page.full;
    }
}

/// What a reconcile commits once the folder matches `staging`.
pub(crate) enum Commit {
    /// `staging` swapped in with the link to ask from next time.
    /// `listing`: the last page of a first listing placed page by page,
    /// which the one `listed` event stands for, however it was reconciled.
    Swap { link: String, listing: bool },
    /// One page of a first listing: its entries into `items`, with
    /// `next`, the link to the page after it.
    Page { changes: Vec<Change>, next: String },
}

/// What the activity log says of a reconcile, beside its conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Said {
    /// One `listed` event for the whole folder: a Full reconcile, or the end
    /// of a first listing.
    Listed,
    /// What a Changed reconcile did, item by item.
    EachChange,
    /// Nothing: a page of a first listing, which the `listed` event at its
    /// end stands for.
    Nothing,
}

/// What a read-write reconcile carries from its staging to its swap.
pub(crate) struct RwCycle<'a> {
    /// What the cycle shares with the folder's outbox worker and watcher.
    pub writes: &'a Writes,
    /// The tree lock, taken before `staging` was begun.
    pub tree: OwnedMutexGuard<()>,
    /// `resyncChangesUploadDifferences`: what the listing left out and was
    /// downloaded here is uploaded again.
    pub upload_differences: bool,
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

/// What a reconcile reads before it touches the folder ([`Reconcile::prepare`]).
pub(crate) struct Prepared {
    root_item_id: String,
    plan: Mode<Plan>,
}

/// The locks a reconcile holds until the folder is no longer being changed:
/// the cycle's turn, the folder's lease, and a read-write folder's tree lock.
pub(crate) struct Held {
    _turn: Turn,
    _lease: super::lease::Held,
    _tree: Option<OwnedMutexGuard<()>>,
}

/// A read-write reconcile's part: what it reads its plan with, and whom it
/// tells.
struct Writing {
    machine: String,
    ignore: IgnoreList,
    upload_differences: bool,
    waiting: Waiting,
    /// Hands the watcher places to examine.
    examine: Arc<dyn Fn(Batch) + Send + Sync>,
    /// Told the removals dropped because their item is already gone.
    dropped_removed: Arc<dyn Fn(Vec<OutboxRow>) + Send + Sync>,
}

/// One reconcile, ready to run on a blocking thread.
pub(crate) struct Reconcile {
    root: SyncRoot,
    store: Store,
    link: Option<HelperLink>,
    runtime: tokio::runtime::Handle,
    locks: InodeLocks,
    rescue_dir: PathBuf,
    cancel: CancellationToken,
    report: Report,
    /// Whether another account claims an item id. A read-only folder's
    /// only: a read-write one sets nothing aside.
    claimed: Option<Claimed>,
    /// The drive to write into `config.toml` and onto the folder first.
    drive: Option<(DriveRecord, String)>,
    /// The mode, with a read-write reconcile's part. A read-only folder is
    /// reconciled under its lock.
    mode: Mode<Writing>,
}

impl Listing {
    /// Makes the folder match `staging` and commits it: swaps
    /// `staging` in with its link, or puts a first listing's page into
    /// `items` with the link to the next one. Under the folder's
    /// lease, on a blocking thread. A Changed scope that
    /// finds the folder not matching the stored tree hands over to a Full
    /// reconcile in the same run. Everything before this wrote only
    /// `staging` and `meta`, and needed no lock but, in read-write mode,
    /// the tree lock `mode` brings.
    ///
    /// The locks and the turn go into the blocking task: however this future
    /// ends, they are held until the folder is no longer being changed. The
    /// materializer sees `cancel` itself between steps; the commit follows
    /// the change to the folder in the same task, so a page placed is a page
    /// committed unless the daemon dies in between.
    pub(super) async fn reconcile(&self, turn: &Turn, mode: Mode<RwCycle<'_>>, scope: Scope, commit: Commit, cancel: &CancellationToken) -> Result<Reconciled, CycleError> {
        let (reconcile, held) = self.begin_reconcile(turn, mode, cancel).await?;
        tokio::task::spawn_blocking(move || {
            let _held = held;
            reconcile.run(scope, commit)
        })
        .await
        .map_err(|e| CycleError::Apply(format!("the reconcile task failed: {e}")))?
    }

    /// A reconcile in `mode`, with the locks it holds while it runs. Waits
    /// for the folder's lease; a folder with no helper is not reconciled
    /// (HS2).
    pub(crate) async fn begin_reconcile(&self, turn: &Turn, mode: Mode<RwCycle<'_>>, cancel: &CancellationToken) -> Result<(Reconcile, Held), CycleError> {
        let lease = cancellable(cancel, self.ctx.lease.hold()).await?;
        // The link as it is now. A helper's reconnect sets it before `resume`
        // takes the lock to re-register the root, so this may be a new link
        // whose helper has no marks yet: at worst a `MarkDir` fails, this
        // cycle fails (the next is Full), and `resume` re-marks the tree.
        let link = self.ctx.link.get().filter(|_| self.ctx.intercepted);
        if link.is_none() {
            return Err(CycleError::NoHelper);
        }
        let (tree, claimed, mode) = match mode {
            Mode::ReadOnly => (None, self.ctx.neighbours.as_ref().map(|n| Arc::clone(&n.claimed)), Mode::ReadOnly),
            Mode::ReadWrite(RwCycle { writes, tree, upload_differences, waiting }) => {
                let writing = Writing {
                    machine: writes.machine_name.clone(),
                    ignore: writes.ignore.read().unwrap_or_else(|p| p.into_inner()).clone(),
                    upload_differences,
                    waiting,
                    examine: Arc::clone(&writes.examine),
                    dropped_removed: Arc::clone(&writes.dropped_removed),
                };
                // Read-write mode sets nothing aside: an object whose id the base does not
                // know is left alone (F115), or goes with what OneDrive removed (F116).
                (Some(tree), None, Mode::ReadWrite(writing))
            }
        };
        let reconcile = Reconcile {
            root: self.ctx.root.clone(),
            store: self.ctx.store.clone(),
            link,
            runtime: tokio::runtime::Handle::current(),
            locks: self.ctx.locks.clone(),
            rescue_dir: self.ctx.rescue_dir.clone(),
            cancel: cancel.clone(),
            report: self.ctx.report.clone(),
            claimed,
            drive: self.pending_drive.lock().unwrap().take(),
            mode,
        };
        Ok((reconcile, Held { _turn: Arc::clone(turn), _lease: lease, _tree: tree }))
    }
}

impl Reconcile {
    /// The whole reconcile: the folder, the commit, and what follows it.
    pub(crate) fn run(mut self, scope: Scope, commit: Commit) -> Result<Reconciled, CycleError> {
        self.record_drive();
        let Some(prepared) = self.prepare()? else { return before_the_root(&self.store, commit) };
        let done = self.apply_with_handover(&prepared, scope)?;
        self.commit(&prepared, done, commit)
    }

    /// Steps 2 and 3: the commit of what step 1 did, and what follows it. A
    /// commit that fails leaves what was done on disk recorded and handed
    /// over all the same ([`Self::failed`]).
    pub(crate) fn commit(&self, prepared: &Prepared, done: Reconciled, commit: Commit) -> Result<Reconciled, CycleError> {
        match commit_cycle(&self.store, prepared.plan.as_ref(), &done, commit) {
            Ok(said) => {
                self.after_commit(&done.applied, said);
                Ok(done)
            }
            Err(e) => {
                self.failed(done.applied.on_disk);
                Err(e)
            }
        }
    }

    /// The account's drive written into `config.toml` (A-M5) and onto the
    /// folder (design §8.3), when this is the reconcile that learnt it.
    fn record_drive(&mut self) {
        let Some((record, id)) = self.drive.take() else { return };
        let recorded = crate::config::DriveId::new(id.as_str()).map(|drive| record.store.record_drive(&record.account, &drive));
        if let Some(Err(e)) = recorded {
            tracing::warn!("cannot record the account's drive in config.toml: {e}");
        }
        if let Err(e) = crate::folder::root::mark_drive(&self.root, &id) {
            tracing::warn!("cannot record the drive on {}: {e}", self.root.path.display());
        }
    }

    /// What the reconcile goes by, read once `staging` holds the new tree:
    /// the drive's root and, in read-write mode, the outbox's rows and what
    /// the new tree changes. `None` while the drive's root has not come:
    /// nothing can be placed yet.
    pub(crate) fn prepare(&self) -> Result<Option<Prepared>, CycleError> {
        let Some(root_item_id) = self.store.call_blocking(move |s| s.root_item_id()).map_err(|e| applying(e.into()))? else { return Ok(None) };
        let plan = match &self.mode {
            Mode::ReadOnly => Mode::ReadOnly,
            Mode::ReadWrite(writing) => {
                let (machine, differences, ignore) = (writing.machine.clone(), writing.upload_differences, writing.ignore.clone());
                let rw = self.store.call_blocking(move |s| Rw::read(s, machine, differences, ignore))?;
                Mode::ReadWrite(Plan { rw: Arc::new(rw), waiting: writing.waiting.clone() })
            }
        };
        Ok(Some(Prepared { root_item_id, plan }))
    }

    /// Step 1: the folder made to match `staging`. A Changed pass that
    /// finds the folder not matching the stored tree is followed by a Full
    /// one; what the first rescued is rescued all the same. A pass that
    /// fails has its work on disk recorded and handed over ([`Self::failed`]).
    pub(crate) fn apply_with_handover(&self, prepared: &Prepared, scope: Scope) -> Result<Reconciled, CycleError> {
        let materializer = Materializer {
            disk: Disk::open(&self.root, self.mode.is_read_only()).map_err(|e| applying(e.into()))?,
            store: self.store.clone(),
            link: self.link.clone(),
            runtime: self.runtime.clone(),
            locks: self.locks.clone(),
            root_item_id: prepared.root_item_id.clone(),
            // One directory for the whole cycle, on the folder's own
            // filesystem: a rescue is one rename, never a copy.
            rescue_into: rescue_base(&self.root.path, &self.rescue_dir).join(rescue_stamp(SystemTime::now())),
            cancel: self.cancel.clone(),
            mode: prepared.plan.as_ref().map(|plan| Arc::clone(&plan.rw)),
            claimed: self.claimed.clone(),
        };
        match materializer.apply_with_handover(scope) {
            Ok((applied, full)) => Ok(Reconciled { applied, full }),
            Err(failed) => {
                let Failed { error, done } = *failed;
                self.failed(done);
                Err(applying(error))
            }
        }
    }

    /// Step 3, once the tree the folder was made to match is committed.
    pub(crate) fn after_commit(&self, applied: &Applied, said: Said) {
        if let Mode::ReadWrite(writing) = &self.mode {
            self.drop_removed(writing);
        }
        // Where each rescued file went is a conflict: a row
        // in `Conflicts.List()`, `Conflicts.Count` and a `conflict` event. It
        // is not a problem, so `LastError` does not say it. A page's are
        // written with the page, not at the end of the listing: a listing
        // that never ends must still say where the files went.
        record(&self.report, &self.store, &self.root.path, applied, said);
        self.hand_over(&applied.on_disk);
    }

    /// `items` just took this cycle's answer: a held or pending
    /// `delete`/`move-out` row whose item is not in it any more has
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
    /// user's own (their `mkdir`), and the watcher is told what to examine —
    /// the attributes are off already, and nothing else says so.
    fn hand_over(&self, done: &OnDisk) {
        let Mode::ReadWrite(writing) = &self.mode else { return };
        if !done.recreated.is_empty() {
            let recreated = done.recreated.clone();
            if let Err(e) = self.store.call_blocking(move |s| s.outbox_detach_parents(&recreated)) {
                tracing::warn!("cannot let the outbox wait for folders made again: {e}");
            }
        }
        if !done.examine.is_empty() {
            let mut batch = Batch::new();
            for (rel, below) in &done.examine {
                match (below, rel.parent(), rel.file_name()) {
                    (true, _, _) => batch.tree(rel),
                    (false, Some(parent), Some(name)) => batch.name(parent, name),
                    _ => {}
                }
            }
            (writing.examine)(batch);
        }
    }

    /// What a reconcile that failed did on disk before it did, handed over
    /// and recorded all the same: its rescues and copies as conflicts, and
    /// what it kept of something removed in OneDrive — only where it took
    /// konedrive's attributes off now, so that a cycle failing again and
    /// again says it once.
    pub(crate) fn failed(&self, mut done: OnDisk) {
        self.hand_over(&done);
        done.kept.retain(|(_, kept)| kept.stripped > 0);
        record(&self.report, &self.store, &self.root.path, &Applied { on_disk: done, ..Applied::default() }, Said::Nothing);
    }
}

/// A commit before the drive's root has come. Nothing on a page can be
/// placed yet: it waits in `items` like any entry whose folder has not come.
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
/// disk does not show yet, and its change waits (the read-write reconcile
/// must, items 3 and 4): an item the outbox holds or the reconcile left
/// unsettled waits whole; one whose new content is still to land waits for
/// its content only. Never what is being removed, and never what the
/// reconcile took off the disk.
pub(crate) fn commit_cycle(store: &Store, plan: Mode<&Plan>, done: &Reconciled, commit: Commit) -> Result<Said, CycleError> {
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
            let applied = &done.applied;
            let changed = store.call_blocking(move |s| s.changed_ids())?;
            let goes = |id: &String| rw.removing.contains(id) || applied.on_disk.taken.contains(id);
            let whole = |id: &String| rw.held.contains(id) || applied.pending.unsettled.contains(id);
            let defer: Vec<String> = changed.iter().filter(|id| !goes(id) && whole(id)).cloned().collect();
            // Only the content waits where the disk took the rest.
            let content: Vec<String> = changed.iter().filter(|id| !goes(id) && !whole(id) && applied.pending.content_waits.contains(*id)).cloned().collect();
            if !defer.is_empty() || !content.is_empty() {
                tracing::debug!("{} change(s) wait for the folder to take them", defer.len() + content.len());
            }
            let (consumed, fetched_at) = (waiting.consumed.clone(), waiting.fetch_seq);
            // What waits to leave the folder, with what it waits for.
            let waits: Vec<(String, String)> = applied.pending.waits.iter().filter(|(id, _)| defer.contains(id)).cloned().collect();
            store.call_blocking(move |s| s.commit_staging_deferring(&link, &Deferrals { consumed: &consumed, whole: &defer, content: &content, fetched_at, waits: &waits }))?;
        }
    }
    Ok(if listing || done.full { Said::Listed } else { Said::EachChange })
}

/// What a reconcile records, on its blocking thread, right after the tree
/// it made the folder match is committed: each
/// rescue as a conflict — the row first, so that whoever hears of it can
/// already find it — and the activity `said`: one `listed` event for a Full
/// reconcile or the end of a first listing, what a Changed one did item by
/// item, at most [`activity::PER_KIND`] of each kind plus one "and N more",
/// or nothing but the conflicts for a page of a first listing.
pub(crate) fn record(report: &Report, store: &Store, root: &Path, applied: &Applied, said: Said) {
    if said == Said::Nothing && applied.on_disk.rescued.is_empty() && applied.on_disk.copies.is_empty() && applied.on_disk.kept.is_empty() {
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
    // What was removed in OneDrive and stays here in part, whatever is said
    // of the rest: it is gone there, and what stays is the user's own.
    let kept = applied.on_disk.kept.iter().map(|(rel, kept)| activity::event(Kind::Removed, shown(rel), kept_detail(*kept))).collect();
    events.extend(activity::capped(kept, activity::PER_KIND, &folder));
    let at = activity::unix_now();
    let conflicts: Vec<ConflictRow> = applied
        .on_disk
        .rescued
        .iter()
        .map(|r| ConflictRow { at, original: shown(&r.original), rescued: r.rescued.display().to_string(), kind: ConflictKind::Rescued })
        // Read-write mode's copies (`docs/design/writes.md` §7): conflicts of kind `copy`, both
        // versions in the folder.
        .chain(applied.on_disk.copies.iter().map(|c| ConflictRow { at, original: shown(&c.original), rescued: shown(&c.copy), kind: ConflictKind::Copy }))
        .collect();
    // Capped like every other kind; every conflict is
    // still a row.
    let each = conflicts.iter().map(|c| activity::event(Kind::Conflict, c.original.clone(), c.rescued.clone())).collect();
    events.extend(activity::capped(each, activity::PER_KIND, &folder));
    report.activity.add_conflicts(conflicts);
    report.activity.record_blocking(events);
}

/// The detail of a `removed` event for something that stays here in part:
/// what goes up as new, and what stays on this computer only.
fn kept_detail(kept: Kept) -> String {
    let uploaded = match kept.uploaded {
        0 => None,
        1 => Some("1 file changed or new on this computer was kept and is uploaded as new".to_owned()),
        n => Some(format!("{n} files changed or new on this computer were kept and are uploaded as new")),
    };
    let local = match kept.local {
        0 => None,
        1 => Some("1 item with an ignored or refused name was kept on this computer only".to_owned()),
        n => Some(format!("{n} items with ignored or refused names were kept on this computer only")),
    };
    [uploaded, local].into_iter().flatten().collect::<Vec<_>>().join("; ")
}
