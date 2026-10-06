//! The marks on the folder's directories: this daemon's notification marks
//! (a group for each filesystem id, within the user's budget) and the
//! helper's permission marks (`MarkDir`), with what could not be marked.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::Arc;

use super::super::fan::{Event, Fid, Fsid, Group};
use super::super::Shared;
use super::tree::Tree;
use crate::helper::{HelperError, LinkCell};

pub(super) struct Marks {
    groups: Vec<Group>,
    /// Filesystem ids no group could be made for: scan-only.
    no_group: Vec<Fsid>,
    /// Marks placed by this watcher.
    placed: usize,
    /// No more than this many are placed (`WatchConfig::mark_budget`).
    budget: Option<usize>,
    /// The budget, the watcher's or the uid's, is spent: no more marks are tried.
    spent: bool,
    /// Directories whose `MarkDir` failed with the helper connected.
    unmarked: HashSet<Fid>,
    /// Directories on another device than the folder's, which the helper
    /// refuses to mark.
    other_dev: HashSet<Fid>,
    warned_devs: HashSet<u64>,
    /// A `MarkDir` timed out in this walk: the rest wait for the retry.
    helper_stuck: bool,
    link: LinkCell,
    runtime: tokio::runtime::Handle,
    shared: Arc<Shared>,
}

impl Marks {
    pub(super) fn new(budget: Option<usize>, link: LinkCell, runtime: tokio::runtime::Handle, shared: Arc<Shared>) -> Self {
        Self {
            groups: Vec::new(),
            no_group: Vec::new(),
            placed: 0,
            budget,
            spent: false,
            unmarked: HashSet::new(),
            other_dev: HashSet::new(),
            warned_devs: HashSet::new(),
            helper_stuck: false,
            link,
            runtime,
            shared,
        }
    }

    /// This watcher's mark on `dir`. `false` when it could not be placed; the
    /// periodic scan covers what that leaves unwatched.
    pub(super) fn watch(&mut self, dir: &File, key: &Fid, mask: u64) -> bool {
        if self.spent {
            return false;
        }
        if self.budget.is_some_and(|budget| self.placed >= budget) {
            self.spend();
            return false;
        }
        let Some(group) = self.group_for(key.fsid) else { return false };
        match self.groups[group].mark(dir, mask) {
            Ok(()) => {
                self.placed += 1;
                true
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
                self.spend();
                false
            }
            Err(e) => {
                self.shared.degrade(format!("a directory could not be watched ({e})"));
                false
            }
        }
    }

    /// A directory without its mark can still be given one: the budget is
    /// not spent, and it is on the folder's device.
    pub(super) fn can_watch(&self, key: &Fid) -> bool {
        !self.spent && !self.other_dev.contains(key)
    }

    fn spend(&mut self) {
        self.spent = true;
        self.shared.degrade(
            "the folder has more directories than this user may watch (fs.fanotify.max_user_marks, shared by every \
             account and subvolume)"
                .into(),
        );
    }

    /// The group for directories of `fsid`, made on first need: one per
    /// filesystem id, since a nested Btrfs subvolume cannot share a group
    /// with its parent (`docs/kernel-behavior-7.2/notification.md` §14.6). `None` when none can be made (the 128 groups
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

    /// Everything the groups hold now, added to `events`. `true` when a
    /// group may hold more.
    pub(super) fn read(&self, buf: &mut [u8], events: &mut Vec<Event>) -> bool {
        let mut more = false;
        for group in &self.groups {
            match group.read(buf, events) {
                Ok(read) => more |= read,
                Err(e) => tracing::warn!("cannot read the notification group: {e}"),
            }
        }
        more
    }

    /// The groups' descriptors, to wait on.
    pub(super) fn fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.groups.iter().map(Group::raw)
    }

    /// A walk begins: the helper is asked again, even if it did not answer
    /// in the walk before.
    pub(super) fn begin_walk(&mut self) {
        self.helper_stuck = false;
    }

    /// `MarkDir` through the helper: the permission mark a directory needs
    /// before content lands in it. With no helper, its own walk marks
    /// everything when it is back. A failure is asked again later.
    pub(super) fn intercept(&mut self, dir: &File, key: &Fid) {
        let Some(link) = self.link.get() else { return };
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

    /// Asks the helper again for every directory it did not mark, where
    /// `tree` has it now.
    pub(super) fn retry(&mut self, tree: &Tree) {
        self.helper_stuck = false;
        for key in std::mem::take(&mut self.unmarked) {
            if self.shared.stopping() {
                return;
            }
            match tree.open_known(&key) {
                Some(dir) => self.intercept(&dir, &key),
                // Moved since: the next retry finds it where the map says.
                None if tree.map.contains(&key) => {
                    self.unmarked.insert(key);
                }
                None => {}
            }
        }
    }

    /// `key`, named `name`, is on the device `dev`, another than the
    /// folder's (a nested Btrfs subvolume, a filesystem mounted inside it).
    /// Nothing there is uploaded (the examination lists it as
    /// `other-device`), and the helper refuses to mark it, so it is neither
    /// marked nor walked: it is counted, and `LastError` says so.
    pub(super) fn elsewhere(&mut self, key: &Fid, name: &OsStr, dev: u64) {
        if self.warned_devs.insert(dev) {
            tracing::warn!(
                "{} is on another device than the OneDrive folder (a nested Btrfs subvolume, or a mount): nothing in it \
                 is uploaded",
                Path::new(name).display()
            );
        }
        self.other_dev.insert(key.clone());
    }

    /// `keys` are not in the folder any more: nothing is kept for them.
    pub(super) fn forget(&mut self, keys: &[Fid]) {
        for key in keys {
            self.unmarked.remove(key);
            self.other_dev.remove(key);
        }
    }

    /// Notification groups.
    pub(super) fn groups(&self) -> usize {
        self.groups.len()
    }

    /// Directories waiting for the helper's mark.
    pub(super) fn uncovered(&self) -> usize {
        self.unmarked.len()
    }

    /// Directories on another device than the folder's.
    pub(super) fn other_device(&self) -> usize {
        self.other_dev.len()
    }
}
