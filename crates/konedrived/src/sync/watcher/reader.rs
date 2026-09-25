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

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use konedrive_fs::handle::FileHandle;
use nix::fcntl::{openat2, OFlag, OpenHow, ResolveFlag};

use super::dirt::Dirt;
use super::fan::{self, Event, Fid, Fsid, Group};
use super::map::DirMap;
use super::{Shared, Timing, ToExaminer, WalkState};
use crate::sync::disk::{open_subdir, HOLDING, NEW_PREFIX};
use crate::sync::helper::HelperError;
use crate::sync::listing::LinkCell;

/// The shortest time between two walks for a directory the map lost (an
/// event from a handle it does not know, a directory it could not list).
pub const UNKNOWN_WALK: Duration = Duration::from_secs(60);
/// Directories that left the folder, remembered so that their events are
/// passed over. Past this many the memory starts again: an event from a
/// forgotten one costs a walk at most.
const LEFT_CAP: usize = 65_536;

/// How a walk treats what it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
    /// The bring-up: every directory is sent to the helper (the helper's
    /// walk may have passed its parent before it was made) and marked; the
    /// Full local scan follows, so nothing is dirtied.
    BringUp,
    /// A directory someone else brought into the folder, or one the map knew
    /// without its mark: `MarkDir`, mark, and the tree is dirty. A directory
    /// the map knows marked only moved.
    Foreign,
    /// One the daemon made or moved (a reconcile). The top one is marked
    /// without `MarkDir` (the reconcile sent it); anything below it may be
    /// someone else's, so it is sent, and the tree is dirty.
    Own,
    /// The map rebuilt from the disk: every directory is visited, and what
    /// the map did not know is treated as [`Walk::Foreign`].
    Again,
}

/// A directory to visit: `name` in `parent`, which is open as `parent_dir`.
struct Item {
    parent_dir: Rc<File>,
    parent: Fid,
    name: OsString,
    depth: usize,
    /// The first new directory of its branch: the tree dirtied is its.
    top: bool,
    /// The directory the walk was started for.
    first: bool,
}

/// An `ONDIR` record whose directory could not be opened where the map
/// says: settled again once the queue is drained and the map has caught up.
struct Pending {
    parent: Fid,
    name: OsString,
    foreign: bool,
    deleted: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Flow {
    Go,
    RootGone,
}

pub(super) struct Reader {
    root: File,
    root_dev: u64,
    map: DirMap,
    groups: Vec<Group>,
    /// Filesystem ids no group could be made for: scan-only.
    no_group: Vec<Fsid>,
    /// The uid's mark budget is spent: no more marks are tried.
    marks_spent: bool,
    /// Directories that left the folder still carrying this group's mark
    /// (only the kernel takes it off, when they go).
    left: HashSet<Fid>,
    /// Directories whose `MarkDir` failed with the helper connected.
    unmarked: HashSet<Fid>,
    /// Directories on another device than the folder's, which the helper
    /// refuses to mark.
    other_dev: HashSet<Fid>,
    warned_devs: HashSet<u64>,
    /// A `MarkDir` timed out in this walk: the rest wait for the retry.
    helper_stuck: bool,
    dirt: Dirt,
    deferred: Vec<Pending>,
    /// Walk again as soon as the queue is drained (a lost event).
    rewalk: bool,
    /// Walk again at this time (a directory the map lost).
    walk_at: Option<Instant>,
    last_walk: Option<Instant>,
    retry_at: Option<Instant>,
    own_pid: Option<i32>,
    timing: Timing,
    link: LinkCell,
    runtime: tokio::runtime::Handle,
    shared: Arc<Shared>,
    #[cfg(test)]
    mark_limit: Option<usize>,
    marks: usize,
}

fn daemon_owned(name: &OsStr) -> bool {
    name == OsStr::new(HOLDING) || name.as_bytes().starts_with(NEW_PREFIX.as_bytes())
}

fn gone(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP))
}

fn beneath() -> ResolveFlag {
    ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS
}

fn dev_of(file: &File) -> Option<u64> {
    nix::sys::stat::fstat(file).ok().map(|st| st.st_dev)
}

/// The subdirectories of `dir`, by name. The type comes from the directory
/// entry, or from `lstat` where the filesystem gives none; a symlink is never
/// a directory here.
fn subdirs(dir: &File) -> io::Result<Vec<OsString>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
        let entry = entry?;
        if entry.file_type().is_ok_and(|t| t.is_dir()) && !daemon_owned(&entry.file_name()) {
            out.push(entry.file_name());
        }
    }
    Ok(out)
}

/// What stands at a name, as far as the watcher can tell.
enum Found {
    /// A directory, open.
    Dir(File, Fid),
    /// A directory this daemon may not open: known by its handle (taken by
    /// name), not marked, and nothing in it examined either.
    Closed(Fid),
    /// Something the watcher cannot look at now (the parent lost its search
    /// bit meanwhile, say): what the map has there stays.
    Unknown,
    Nothing,
}

impl Reader {
    pub(super) fn new(
        root: File,
        own_pid: Option<i32>,
        timing: Timing,
        link: LinkCell,
        runtime: tokio::runtime::Handle,
        shared: Arc<Shared>,
        #[cfg(test)] mark_limit: Option<usize>,
    ) -> io::Result<Self> {
        let key = Fid::of(&root)?;
        let root_dev = dev_of(&root).ok_or_else(|| io::Error::other("cannot stat the folder"))?;
        let mut reader = Self {
            map: DirMap::new(key.clone(), false),
            root,
            root_dev,
            groups: Vec::new(),
            no_group: Vec::new(),
            marks_spent: false,
            left: HashSet::new(),
            unmarked: HashSet::new(),
            other_dev: HashSet::new(),
            warned_devs: HashSet::new(),
            helper_stuck: false,
            dirt: Dirt::default(),
            deferred: Vec::new(),
            rewalk: false,
            walk_at: None,
            last_walk: None,
            retry_at: None,
            own_pid,
            timing,
            link,
            runtime,
            shared,
            #[cfg(test)]
            mark_limit,
            marks: 0,
        };
        let root = reader.root.try_clone()?;
        let marked = reader.watch(&root, &key, fan::ROOT_MASK);
        reader.map.set_marked(&key, marked);
        Ok(reader)
    }

    /// The bring-up walk: every directory `MarkDir`ed and marked before
    /// anything in it is looked at, then a Full local scan (§3.3's bring-up
    /// order). Runs on the reader's thread; `Watcher::walked` says when it is
    /// done, or that it was cut short (a stop, a folder it could not list).
    fn bring_up(&mut self) {
        let walked = self.walk_all(Walk::BringUp);
        self.dirt.full();
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
                shared.flushes.lock().unwrap_or_else(|p| p.into_inner()).clear();
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
        let mut degraded_walk: Option<Instant> = None;
        while !self.shared.stopping() {
            let flushes = self.shared.take_flushes();
            if !flushes.is_empty() {
                // Everything the kernel holds now, handed over at once.
                if self.drain(&mut buf, &mut events) == Flow::RootGone {
                    return self.root_gone();
                }
                self.settle_deferred();
                if self.rewalk {
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
            #[cfg(test)]
            if self.shared.paused() {
                self.wait(Some(Duration::from_millis(20)), true);
                continue;
            }
            if self.shared.take_helper_back() {
                // Its registration walk marked every directory there is.
                self.retry_unmarked();
                self.publish();
            }
            if self.shared.degraded() {
                degraded_walk.get_or_insert(now + self.timing.degraded_scan);
            }
            if self.unmarked.is_empty() {
                self.retry_at = None;
            } else {
                self.retry_at.get_or_insert(now + self.timing.mark_retry);
            }
            let wake = [self.dirt.due(&self.timing), degraded_walk, self.walk_at, self.retry_at].into_iter().flatten().min();
            self.wait(wake.map(|at| at.saturating_duration_since(now)), false);
            if self.drain(&mut buf, &mut events) == Flow::RootGone {
                return self.root_gone();
            }
            self.settle_deferred();
            let now = Instant::now();
            if degraded_walk.is_some_and(|at| at <= now) {
                degraded_walk = None;
                self.rewalk = true;
            }
            if self.walk_at.is_some_and(|at| at <= now) {
                self.walk_at = None;
                self.rewalk = true;
            }
            if self.rewalk {
                self.walk_all(Walk::Again);
                self.publish();
            }
            if self.retry_at.is_some_and(|at| at <= now) {
                self.retry_at = None;
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
            let mut more = false;
            for group in &self.groups {
                match group.read(buf, events) {
                    Ok(read) => more |= read,
                    Err(e) => tracing::warn!("cannot read the notification group: {e}"),
                }
            }
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
    fn wait(&self, timeout: Option<Duration>, stop_only: bool) {
        let mut fds = vec![libc::pollfd { fd: self.shared.wake_fd(), events: libc::POLLIN, revents: 0 }];
        if !stop_only {
            fds.extend(self.groups.iter().map(|g| libc::pollfd { fd: g.raw(), events: libc::POLLIN, revents: 0 }));
        }
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
        let batch = self.dirt.take(&self.map);
        self.publish();
        if !batch.is_empty() {
            self.shared.update(|s| s.handed_over += 1);
            let _ = tx.send(ToExaminer::Batch(batch));
        }
    }

    /// The counts, for `status()`.
    fn publish(&self) {
        let unwatched = self.map.keys().filter(|k| !self.map.get(k).is_some_and(|n| n.marked)).count();
        self.shared.update(|s| s.unwatched = unwatched);
        self.publish_counts();
    }

    /// The counts that cost nothing to take.
    fn publish_counts(&self) {
        let (directories, groups, uncovered, other_device) = (self.map.len(), self.groups.len(), self.unmarked.len(), self.other_dev.len());
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
            self.dirt.full();
            self.dirt.touch(now);
            self.rewalk = true;
            return Flow::Go;
        }
        if event.has(fan::DELETE_SELF | fan::MOVE_SELF) && event.at.as_ref().is_some_and(|at| at.dir == *self.map.root()) {
            return Flow::RootGone;
        }
        let foreign = self.own_pid != Some(event.pid);
        if event.has(fan::ONDIR) {
            // The new side first: a move within the folder is then a move.
            let deleted = event.has(fan::DELETE) && !event.has(fan::RENAME);
            for (record, deleted) in [(&event.new, false), (&event.at, deleted), (&event.old, false)] {
                let Some(record) = record else { continue };
                if record.name == OsStr::new(".") {
                    self.retry_mark(&record.dir);
                } else {
                    self.settle(&record.dir, &record.name, foreign, deleted);
                }
            }
        }
        if !foreign {
            return Flow::Go;
        }
        let (mut dirty, mut unknown) = (false, false);
        for (record, at) in [(&event.at, true), (&event.old, false), (&event.new, false)] {
            let Some(record) = record else { continue };
            if !self.map.contains(&record.dir) {
                unknown |= !self.left.contains(&record.dir);
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
            self.walk_soon();
        }
        Flow::Go
    }

    /// A walk for a directory the map lost, at most once every
    /// [`UNKNOWN_WALK`].
    fn walk_soon(&mut self) {
        let earliest = self.last_walk.map_or_else(Instant::now, |at| at + UNKNOWN_WALK);
        self.walk_at.get_or_insert(earliest.max(Instant::now()));
    }

    /// An event on a directory itself (`"."`), such as a `chmod`. A known
    /// directory without its mark (it was unreadable) is adopted again now,
    /// with everything below it: `MarkDir`, mark, and since nothing in it
    /// raised events meanwhile, the whole tree is dirty.
    fn retry_mark(&mut self, dir: &Fid) {
        let Some(node) = self.map.get(dir) else { return };
        if node.marked || self.marks_spent || self.other_dev.contains(dir) {
            return;
        }
        let (Some(parent), name) = (node.parent.clone(), node.name.clone()) else { return };
        let Some(parent_dir) = self.open_known(&parent) else { return };
        let depth = self.map.path(dir).map_or(1, |p| p.components().count());
        let item = Item { parent_dir: Rc::new(parent_dir), parent, name, depth, top: true, first: true };
        self.visit(vec![item], Walk::Foreign, None);
    }

    /// `name` in `parent`, as the disk has it now.
    fn settle(&mut self, parent: &Fid, name: &OsStr, foreign: bool, deleted: bool) {
        if daemon_owned(name) || !self.map.contains(parent) {
            return;
        }
        if !self.try_settle(parent, name, foreign, deleted) {
            self.deferred.push(Pending { parent: parent.clone(), name: name.to_owned(), foreign, deleted });
        }
    }

    /// Whatever the map has as `name` in `parent` and the disk does not is
    /// gone from there, with everything below it; whatever the disk has
    /// there is placed, or adopted when new. `false` when `parent` could not
    /// be opened where the map says (it moved again, and a later event says
    /// where).
    fn try_settle(&mut self, parent: &Fid, name: &OsStr, foreign: bool, deleted: bool) -> bool {
        let Some(parent_dir) = self.open_known(parent) else { return false };
        let found = look(&parent_dir, parent, name);
        let here = match &found {
            Found::Dir(_, key) | Found::Closed(key) => Some(key.clone()),
            Found::Unknown => {
                self.walk_soon();
                return true;
            }
            Found::Nothing => None,
        };
        if let Some(was) = self.map.child(parent, name).filter(|was| Some(was) != here.as_ref()) {
            self.forget(&was, deleted);
        }
        match found {
            Found::Dir(dir, _) => {
                drop(dir);
                let depth = self.map.path(parent).map_or(0, |p| p.components().count()) + 1;
                let how = if foreign { Walk::Foreign } else { Walk::Own };
                let item = Item { parent_dir: Rc::new(parent_dir), parent: parent.clone(), name: name.to_owned(), depth, top: true, first: true };
                self.visit(vec![item], how, None);
            }
            Found::Closed(key) => match self.map.place(key, parent, name, false) {
                Ok(displaced) => self.leave(displaced),
                Err(_) => self.rewalk = true,
            },
            Found::Unknown | Found::Nothing => {}
        }
        true
    }

    fn settle_deferred(&mut self) {
        for pending in std::mem::take(&mut self.deferred) {
            if !self.map.contains(&pending.parent) {
                continue;
            }
            if !self.try_settle(&pending.parent, &pending.name, pending.foreign, pending.deleted) {
                tracing::debug!("a directory event the map cannot place; the map is walked again");
                self.rewalk = true;
            }
        }
    }

    /// `key` and everything below it are not in the folder any more:
    /// deleted, or gone somewhere unwatched (still carrying the mark).
    fn forget(&mut self, key: &Fid, deleted: bool) {
        let gone = self.map.remove(key);
        for key in &gone {
            self.unmarked.remove(key);
            self.other_dev.remove(key);
        }
        if !deleted {
            self.leave(gone);
        }
    }

    fn leave(&mut self, keys: Vec<Fid>) {
        if self.left.len() + keys.len() > LEFT_CAP {
            self.left.clear();
        }
        self.left.extend(keys);
    }

    /// The directory the map knows as `key`, opened beneath the root where
    /// the map says it is, and proved to be `key` by its handle.
    fn open_known(&self, key: &Fid) -> Option<File> {
        let path = self.map.path(key)?;
        let dir = if path.as_os_str().is_empty() {
            self.root.try_clone().ok()?
        } else {
            let how = OpenHow::new().flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).resolve(beneath());
            File::from(openat2(self.root.as_fd(), path.as_path(), how).ok()?)
        };
        (Fid::of(&dir).ok()? == *key).then_some(dir)
    }

    /// Every directory beneath the root: the bring-up, or the map rebuilt.
    /// What the map knew and the walk did not find is gone; what the walk
    /// could not look into is kept, and walked again later.
    /// `false` when it could not finish: the folder could not be listed, or
    /// the watcher is stopping.
    fn walk_all(&mut self, how: Walk) -> bool {
        self.rewalk = false;
        self.deferred.clear();
        self.last_walk = Some(Instant::now());
        let root_key = self.map.root().clone();
        let Ok(root) = self.root.try_clone() else { return false };
        let root = Rc::new(root);
        let names = match subdirs(&root) {
            Ok(names) => names,
            Err(e) => {
                tracing::warn!("cannot list the folder: {e}");
                self.walk_soon();
                return false;
            }
        };
        let items = names
            .into_iter()
            .map(|name| Item { parent_dir: Rc::clone(&root), parent: root_key.clone(), name, depth: 1, top: true, first: false })
            .collect();
        let mut seen = HashSet::from([root_key]);
        self.visit(items, how, Some(&mut seen));
        if self.shared.stopping() {
            return false;
        }
        let lost: Vec<Fid> = self.map.keys().filter(|k| !seen.contains(*k)).cloned().collect();
        for key in lost {
            self.forget(&key, false);
        }
        true
    }

    /// Visits `stack` and, below each directory new to the map or not yet
    /// marked (below every one, for a walk of the whole folder), everything
    /// inside it: each directory is `MarkDir`ed and marked before it is
    /// listed.
    fn visit(&mut self, mut stack: Vec<Item>, how: Walk, mut seen: Option<&mut HashSet<Fid>>) {
        self.helper_stuck = false;
        while let Some(item) = stack.pop() {
            if self.shared.stopping() {
                return;
            }
            let (dir, key) = match look(&item.parent_dir, &item.parent, &item.name) {
                Found::Dir(dir, key) => (dir, key),
                Found::Closed(key) => {
                    if let Some(seen) = seen.as_deref_mut() {
                        seen.extend(self.map.subtree(&key));
                        seen.insert(key.clone());
                    }
                    match self.map.place(key, &item.parent, &item.name, false) {
                        Ok(displaced) => self.leave(displaced),
                        Err(_) => self.rewalk = true,
                    }
                    continue;
                }
                Found::Unknown => {
                    if let (Some(seen), Some(known)) = (seen.as_deref_mut(), self.map.child(&item.parent, &item.name)) {
                        seen.extend(self.map.subtree(&known));
                    }
                    self.walk_soon();
                    continue;
                }
                Found::Nothing => continue,
            };
            if let Some(seen) = seen.as_deref_mut() {
                seen.insert(key.clone());
            }
            if dev_of(&dir).is_some_and(|dev| dev != self.root_dev) {
                self.elsewhere(&dir, key, &item);
                continue;
            }
            let fresh = self.map.get(&key).is_none_or(|n| !n.marked);
            if how == Walk::BringUp || (fresh && !(how == Walk::Own && item.first)) {
                self.intercept(&dir, &key);
            }
            let marked = !fresh || self.watch(&dir, &key, fan::DIR_MASK);
            match self.map.place(key.clone(), &item.parent, &item.name, marked) {
                Ok(displaced) => self.leave(displaced),
                Err(_) => {
                    tracing::debug!("the directory map is out of step; it is walked again");
                    self.rewalk = true;
                    continue;
                }
            }
            self.left.remove(&key);
            if fresh && item.top && how != Walk::BringUp {
                self.dirt.tree(&key);
                self.dirt.touch(Instant::now());
            }
            if !fresh && !matches!(how, Walk::BringUp | Walk::Again) {
                continue;
            }
            if item.depth >= konedrive_fs::MAX_DEPTH {
                tracing::warn!("a directory deeper than {} levels is not watched", konedrive_fs::MAX_DEPTH);
                continue;
            }
            let names = match subdirs(&dir) {
                Ok(names) => names,
                Err(e) => {
                    tracing::debug!("cannot list a directory to watch: {e}");
                    if let Some(seen) = seen.as_deref_mut() {
                        seen.extend(self.map.subtree(&key));
                    }
                    self.walk_soon();
                    continue;
                }
            };
            let dir = Rc::new(dir);
            for name in names {
                stack.push(Item { parent_dir: Rc::clone(&dir), parent: key.clone(), name, depth: item.depth + 1, top: !fresh, first: false });
            }
        }
    }

    /// A directory on another device than the folder's (a nested Btrfs
    /// subvolume, a filesystem mounted inside it). Nothing there is uploaded
    /// (the examination lists it as `other-device`), and the helper refuses to
    /// mark it, so it is neither marked nor walked: it is kept in the map,
    /// unwatched, and counted, and `LastError` says so.
    fn elsewhere(&mut self, dir: &File, key: Fid, item: &Item) {
        if let Some(dev) = dev_of(dir) {
            if self.warned_devs.insert(dev) {
                tracing::warn!(
                    "{} is on another device than the OneDrive folder (a nested Btrfs subvolume, or a mount): nothing \
                     in it is uploaded",
                    Path::new(&item.name).display()
                );
            }
        }
        self.other_dev.insert(key.clone());
        match self.map.place(key, &item.parent, &item.name, false) {
            Ok(displaced) => self.leave(displaced),
            Err(_) => self.rewalk = true,
        }
    }

    /// `MarkDir` through the helper: the permission mark a directory needs
    /// before content lands in it. With no helper, its own walk marks
    /// everything when it is back. A failure is asked again later.
    fn intercept(&mut self, dir: &File, key: &Fid) {
        let Some(link) = self.link.lock().unwrap().clone() else { return };
        if self.helper_stuck {
            self.unmarked.insert(key.clone());
            return;
        }
        match self.runtime.block_on(link.mark_dir(dir)) {
            Ok(()) => {
                self.unmarked.remove(key);
            }
            Err(e) => {
                tracing::warn!("the helper did not mark a directory of the folder ({e}); it is asked again");
                self.helper_stuck = matches!(e, HelperError::Timeout);
                self.unmarked.insert(key.clone());
            }
        }
    }

    /// Asks the helper again for every directory it did not mark.
    fn retry_unmarked(&mut self) {
        self.helper_stuck = false;
        for key in std::mem::take(&mut self.unmarked) {
            if self.shared.stopping() {
                return;
            }
            match self.open_known(&key) {
                Some(dir) => self.intercept(&dir, &key),
                // Moved since: the next retry finds it where the map says.
                None if self.map.contains(&key) => {
                    self.unmarked.insert(key);
                }
                None => {}
            }
        }
        self.publish_counts();
    }

    /// This group's mark on `dir`. `false` when it could not be placed; the
    /// periodic scan covers what that leaves unwatched.
    fn watch(&mut self, dir: &File, key: &Fid, mask: u64) -> bool {
        if self.marks_spent {
            return false;
        }
        #[cfg(test)]
        if self.mark_limit.is_some_and(|limit| self.marks >= limit) {
            self.spent();
            return false;
        }
        let Some(group) = self.group_for(key.fsid) else { return false };
        match self.groups[group].mark(dir, mask) {
            Ok(()) => {
                self.marks += 1;
                true
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
                self.spent();
                false
            }
            Err(e) => {
                self.shared.degrade(format!("a directory could not be watched ({e})"));
                false
            }
        }
    }

    fn spent(&mut self) {
        self.marks_spent = true;
        self.shared.degrade(
            "the folder has more directories than this user may watch (fs.fanotify.max_user_marks, shared by every \
             account and subvolume)"
                .into(),
        );
    }

    /// The group for directories of `fsid`, made on first need: one per
    /// filesystem id, since a nested Btrfs subvolume cannot share a group
    /// with its parent (§3.6). `None` when none can be made (the 128 groups
    /// a uid may hold): that subvolume is scan-only.
    fn group_for(&mut self, fsid: Fsid) -> Option<usize> {
        if let Some(at) = self.groups.iter().position(|g| g.fsid == fsid) {
            return Some(at);
        }
        if self.no_group.contains(&fsid) {
            return None;
        }
        match Group::new(fsid) {
            Ok(group) => {
                self.groups.push(group);
                Some(self.groups.len() - 1)
            }
            Err(e) => {
                self.no_group.push(fsid);
                let why = if e.raw_os_error() == Some(libc::EMFILE) {
                    "this user holds all the notification groups it may (fs.fanotify.max_user_groups)".to_owned()
                } else {
                    format!("no notification group could be made ({e})")
                };
                self.shared.degrade(why);
                None
            }
        }
    }
}

fn look(parent_dir: &File, parent: &Fid, name: &OsStr) -> Found {
    match open_subdir(parent_dir, name) {
        Ok(dir) => match Fid::of(&dir) {
            Ok(key) => Found::Dir(dir, key),
            Err(e) => {
                tracing::warn!("{} gives no file handle, so it cannot be watched: {e}", Path::new(name).display());
                Found::Nothing
            }
        },
        Err(e) if gone(&e) => Found::Nothing,
        Err(e) => match FileHandle::at(parent_dir, name) {
            Ok(handle) => {
                tracing::debug!("{} cannot be opened to be watched: {e}", Path::new(name).display());
                Found::Closed(Fid { fsid: parent.fsid, handle })
            }
            Err(e) if gone(&e) => Found::Nothing,
            Err(_) => Found::Unknown,
        },
    }
}
