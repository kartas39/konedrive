//! The reader: one thread that walks the folder once, then drains the
//! notification groups, keeps the directory map and the marks current, and
//! gathers dirt until a quiet spell hands it over (`docs/design/writes.md` §3, amended
//! by §17).
//!
//! **Events are hints.** A directory's events are settled against the disk,
//! never read as a sequence: for each `(directory, name)` a `FAN_ONDIR`
//! event names, whatever the map has at that name and the disk does not has
//! gone from there, and whatever the disk has there is placed (it moved) or
//! adopted (it is new). The new side of a rename is settled before the old
//! one, so a move within the folder is one move. A rename with only an old
//! side went somewhere unwatched: out of the folder, or into a directory not
//! marked yet, whose own adoption finds it (§3.1, §8.1).
//!
//! **Permission marks.** Every directory the bring-up walk visits is
//! sent to the helper (`MarkDir`): one made after the helper's own walk passed
//! its parent has no permission mark otherwise, for as long as that helper
//! runs. After that, every directory new to the map is, before this group's
//! mark and before it is listed. A `MarkDir` that fails while the helper is
//! connected is asked again every [`Timing::mark_retry`] and when the helper
//! is back. The helper refuses a directory on another device than the
//! folder's (a nested Btrfs subvolume, a mount), and nothing there is
//! uploaded (F72): such a directory is neither asked for, nor marked, nor
//! walked into; it is counted, and `LastError` says so.
//!
//! **Own events** (§3.2) carry the daemon's pid and are dropped, except that
//! a directory the daemon made, moved or removed still changes the map and
//! the marks. The kernel reports a pid only for the listener's own events, so
//! the check cannot drop anyone else's.
//!
//! **A directory handle the map does not know** (§3.3): a directory that left
//! the folder keeps this group's mark, and its events are passed over. Any
//! other unknown handle means the map lost a directory: the folder is walked
//! again, at most once every [`UNKNOWN_WALK`]. So is a directory the walk
//! could not look into; what the map has there is kept meanwhile.
//!
//! **The parts.** What the reader knows is kept in three: the folder's
//! directories (`tree`: the root, the map, what left, what waits to be
//! settled), the marks on them (`marks`: the groups and their budget, the
//! helper's `MarkDir` and what it did not mark), and when to walk or ask
//! again (`timers`). How a walk treats what it finds is `walk`'s.

mod marks;
mod timers;
mod tree;
mod walk;

use std::ffi::OsStr;
use std::fs::File;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use timers::UNKNOWN_WALK;

use self::marks::Marks;
use self::timers::Timers;
use self::tree::{look, By, Found, Gone, Pending, Settled, Tree};
use self::walk::{Item, Walk};
use super::dirt::Dirt;
use super::fan::{self, Event, Fid};
use super::{Shared, Timing, ToExaminer, WalkState};
use crate::helper::LinkCell;
use crate::local::ScanReason;

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Go,
    RootGone,
}

pub(super) struct Reader {
    tree: Tree,
    marks: Marks,
    timers: Timers,
    /// What the events since the last hand-over made dirty.
    dirt: Dirt,
    /// Events with this pid are the daemon's own.
    own_pid: Option<i32>,
    timing: Timing,
    shared: Arc<Shared>,
}

impl Reader {
    /// The reader of the folder open as `root`, with the root marked. No
    /// more than `mark_budget` marks are placed, if given.
    pub(super) fn new(
        root: File,
        own_pid: Option<i32>,
        timing: Timing,
        mark_budget: Option<usize>,
        link: LinkCell,
        runtime: tokio::runtime::Handle,
        shared: Arc<Shared>,
    ) -> std::io::Result<Self> {
        let mut reader = Self {
            marks: Marks::new(mark_budget, link, runtime, Arc::clone(&shared)),
            timers: Timers::new(&timing),
            dirt: Dirt::default(),
            own_pid,
            timing,
            shared,
            tree: Tree::new(root.try_clone()?)?,
        };
        let key = reader.tree.map.root().clone();
        let marked = reader.marks.watch(&root, &key, fan::ROOT_MASK);
        reader.tree.map.set_marked(&key, marked);
        Ok(reader)
    }

    /// The bring-up walk: every directory `MarkDir`ed and marked before
    /// anything in it is looked at, then a Full local scan (§3.3's bring-up
    /// order). Runs on the reader's thread; `Watcher::walked` says when it is
    /// done, or that it was cut short (a stop, a folder it could not list).
    fn bring_up(&mut self) {
        let walked = self.walk_all(Walk::BringUp);
        self.dirt.full(self.shared.first_scan);
        self.dirt.touch(Instant::now());
        self.publish();
        let state = if walked && !self.shared.stopping() { WalkState::Done } else { WalkState::Cut };
        self.shared.walked.send_replace(state);
    }

    /// Walks the folder, then reads until stopped or the root goes.
    ///
    /// While part of the folder cannot be watched, the map is also walked
    /// again every [`Timing::degraded_scan`]: a directory made inside an
    /// unwatched one raised no event, and gets its `MarkDir` then.
    pub(super) fn run(mut self, tx: mpsc::Sender<ToExaminer>) {
        // However this thread ends: whoever waits for the walk is let go (a
        // walk not finished is cut), a flush waiting for it is answered, and
        // an end nobody asked for is said (the watcher).
        struct Ending(Arc<Shared>);
        impl Drop for Ending {
            fn drop(&mut self) {
                let shared = &self.0;
                shared.walked.send_if_modified(|state| {
                    let walking = *state == WalkState::Walking;
                    if walking {
                        *state = WalkState::Cut;
                    }
                    walking
                });
                shared.reader_done.store(true, Ordering::SeqCst);
                crate::panic::lock(&shared.flushes).clear();
                if !shared.stopping() && !shared.status().root_gone {
                    tracing::error!("the watcher of local changes stopped unexpectedly");
                    shared.update(|s| s.stopped = true);
                }
            }
        }
        let _ending = Ending(Arc::clone(&self.shared));
        self.bring_up();
        let mut buf = vec![0u8; 256 * 1024];
        let mut events = Vec::new();
        while !self.shared.stopping() {
            let flushes = self.shared.take_flushes();
            if !flushes.is_empty() {
                // Everything the kernel holds now, handed over at once.
                if self.drain(&mut buf, &mut events) == Flow::RootGone {
                    return self.root_gone();
                }
                self.settle_deferred();
                if self.timers.walk_wanted() {
                    self.walk_all(Walk::Again);
                }
                self.hand_over(&tx);
                for ack in flushes {
                    let _ = tx.send(ToExaminer::Flush(ack));
                }
                continue;
            }
            let now = Instant::now();
            if self.dirt.due(&self.timing).is_some_and(|due| due <= now) {
                self.hand_over(&tx);
                continue;
            }
            if self.shared.take_helper_back() {
                // Its registration walk marked every directory there is.
                self.retry_unmarked();
                self.publish();
            }
            if self.shared.degraded() {
                self.timers.degrade(now);
            }
            self.timers.uncovered(now, self.marks.uncovered());
            let wake = [self.dirt.due(&self.timing), self.timers.next_wake()].into_iter().flatten().min();
            self.wait(wake.map(|at| at.saturating_duration_since(now)));
            if self.drain(&mut buf, &mut events) == Flow::RootGone {
                return self.root_gone();
            }
            self.settle_deferred();
            let due = self.timers.due(Instant::now());
            if due.walk {
                self.walk_all(Walk::Again);
                self.publish();
            }
            if due.retry {
                self.retry_unmarked();
            }
            self.publish_counts();
        }
    }

    fn root_gone(&self) {
        tracing::warn!("the OneDrive folder was moved or deleted; its watcher stops");
        self.shared.root_gone();
    }

    /// Reads and handles everything queued, until the groups are empty.
    fn drain(&mut self, buf: &mut [u8], events: &mut Vec<Event>) -> Flow {
        loop {
            let more = self.marks.read(buf, events);
            let now = Instant::now();
            for event in events.drain(..) {
                if self.handle(event, now) == Flow::RootGone {
                    return Flow::RootGone;
                }
            }
            if !more {
                return Flow::Go;
            }
        }
    }

    /// Waits for an event, a wake-up (a stop, a flush, the helper back), or
    /// `timeout`.
    fn wait(&self, timeout: Option<Duration>) {
        let mut fds = vec![libc::pollfd { fd: self.shared.wake_fd(), events: libc::POLLIN, revents: 0 }];
        fds.extend(self.marks.fds().map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }));
        let ms = timeout.map_or(-1, |t| t.as_millis().clamp(1, i32::MAX as u128) as i32);
        // SAFETY: `fds` is a live array of `fds.len()` pollfds.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if fds[0].revents & libc::POLLIN != 0 && !self.shared.stopping() {
            let mut count = 0u64;
            // SAFETY: reads 8 bytes into a live u64 from our own eventfd.
            unsafe { libc::read(self.shared.wake_fd(), (&mut count as *mut u64).cast(), 8) };
        }
    }

    fn hand_over(&mut self, tx: &mpsc::Sender<ToExaminer>) {
        let batch = self.dirt.take(&self.tree.map);
        self.publish();
        if !batch.is_empty() {
            self.shared.update(|s| s.handed_over += 1);
            let _ = tx.send(ToExaminer::Batch(batch));
        }
    }

    /// The counts, for `status()`.
    fn publish(&self) {
        let map = &self.tree.map;
        let unwatched = map.keys().filter(|k| !map.get(k).is_some_and(|n| n.marked)).count();
        self.shared.update(|s| s.unwatched = unwatched);
        self.publish_counts();
    }

    /// The counts that cost nothing to take.
    fn publish_counts(&self) {
        let (directories, groups, uncovered, other_device) =
            (self.tree.map.len(), self.marks.groups(), self.marks.uncovered(), self.marks.other_device());
        self.shared.update(|s| {
            s.directories = directories;
            s.groups = groups;
            s.uncovered = uncovered;
            s.other_device = other_device;
        });
    }

    /// One event. `Flow::RootGone` when the root itself moved or went.
    fn handle(&mut self, event: Event, now: Instant) -> Flow {
        if event.has(fan::Q_OVERFLOW) {
            tracing::warn!("the notification queue overflowed: the folder is scanned in full");
            self.shared.update(|s| s.overflows += 1);
            self.dirt.full(ScanReason::Overflow);
            self.dirt.touch(now);
            self.timers.lost();
            return Flow::Go;
        }
        if event.has(fan::DELETE_SELF | fan::MOVE_SELF) && event.at.as_ref().is_some_and(|at| at.dir == *self.tree.map.root()) {
            return Flow::RootGone;
        }
        let by = if self.own_pid == Some(event.pid) { By::Daemon } else { By::Other };
        if event.has(fan::ONDIR) {
            // A rename tells nothing of where the old side went; a delete does.
            let at_gone = if event.has(fan::DELETE) && !event.has(fan::RENAME) { Gone::Deleted } else { Gone::Left };
            // The new side first: a move within the folder is then a move.
            for (record, gone) in [(&event.new, Gone::Left), (&event.at, at_gone), (&event.old, Gone::Left)] {
                let Some(record) = record else { continue };
                if record.name == OsStr::new(".") {
                    self.retry_mark(&record.dir);
                } else {
                    self.settle(&record.dir, &record.name, Settled { by, gone });
                }
            }
        }
        if by == By::Daemon {
            return Flow::Go;
        }
        let (mut dirty, mut unknown) = (false, false);
        for (record, at) in [(&event.at, true), (&event.old, false), (&event.new, false)] {
            let Some(record) = record else { continue };
            if !self.tree.map.contains(&record.dir) {
                unknown |= !self.tree.has_left(&record.dir);
                continue;
            }
            if at && event.has(fan::CLOSE_WRITE) && !event.has(fan::ONDIR) {
                self.dirt.written(&record.dir, &record.name, event.object.as_ref().map(|o| &o.handle));
            }
            self.dirt.name(&record.dir, &record.name);
            dirty = true;
        }
        if dirty {
            if let Some(object) = &event.object {
                self.dirt.object(&object.handle);
            }
            self.dirt.touch(now);
        } else if unknown {
            tracing::debug!("an event from a directory the map does not know; the folder is walked again");
            self.timers.walk_soon(now);
        }
        Flow::Go
    }

    /// An event on a directory itself (`"."`), such as a `chmod`. A known
    /// directory without its mark (it was unreadable) is adopted again now,
    /// with everything below it: `MarkDir`, mark, and since nothing in it
    /// raised events meanwhile, the whole tree is dirty.
    fn retry_mark(&mut self, dir: &Fid) {
        let Some(node) = self.tree.map.get(dir) else { return };
        if node.marked || !self.marks.can_watch(dir) {
            return;
        }
        let (Some(parent), name) = (node.parent.clone(), node.name.clone()) else { return };
        let Some(parent_dir) = self.tree.open_known(&parent) else { return };
        let depth = self.tree.depth(dir).max(1);
        self.visit(vec![Item::first(parent_dir, parent, name, depth)], Walk::Foreign, None);
    }

    /// `name` in `parent`, as the disk has it now.
    fn settle(&mut self, parent: &Fid, name: &OsStr, how: Settled) {
        if !self.tree.follows(parent, name) {
            return;
        }
        if !self.try_settle(parent, name, how) {
            self.tree.defer(Pending { parent: parent.clone(), name: name.to_owned(), how });
        }
    }

    /// Whatever the map has as `name` in `parent` and the disk does not is
    /// gone from there, with everything below it; whatever the disk has
    /// there is placed, or adopted when new. `false` when `parent` could not
    /// be opened where the map says (it moved again, and a later event says
    /// where).
    fn try_settle(&mut self, parent: &Fid, name: &OsStr, how: Settled) -> bool {
        let Some(parent_dir) = self.tree.open_known(parent) else { return false };
        let found = look(&parent_dir, parent, name);
        let here = match &found {
            Found::Dir(_, key) | Found::Closed(key) => Some(key),
            Found::Unknown => {
                self.timers.walk_soon(Instant::now());
                return true;
            }
            Found::Nothing => None,
        };
        if let Some(was) = self.tree.map.child(parent, name).filter(|was| Some(was) != here) {
            self.forget(&was, how.gone);
        }
        match found {
            Found::Dir(dir, _) => {
                drop(dir);
                let depth = self.tree.depth(parent) + 1;
                let walk = match how.by {
                    By::Other => Walk::Foreign,
                    By::Daemon => Walk::Own,
                };
                self.visit(vec![Item::first(parent_dir, parent.clone(), name.to_owned(), depth)], walk, None);
            }
            Found::Closed(key) => {
                self.tree.place_or_rewalk(&mut self.timers, key, parent, name, false);
            }
            Found::Unknown | Found::Nothing => {}
        }
        true
    }

    fn settle_deferred(&mut self) {
        for pending in self.tree.take_deferred() {
            // Asked at each one: settling the one before may have forgotten its directory.
            if !self.tree.map.contains(&pending.parent) {
                continue;
            }
            if !self.try_settle(&pending.parent, &pending.name, pending.how) {
                tracing::debug!("a directory event the map cannot place; the map is walked again");
                self.timers.lost();
            }
        }
    }

    /// `key` and everything below it are not in the folder any more:
    /// deleted, or gone somewhere unwatched (still carrying the mark).
    fn forget(&mut self, key: &Fid, how: Gone) {
        let gone = self.tree.forget(key, how);
        self.marks.forget(&gone);
    }

    /// Asks the helper again for every directory it did not mark.
    fn retry_unmarked(&mut self) {
        self.marks.retry(&self.tree);
        self.publish_counts();
    }
}
