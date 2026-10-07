use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use konedrive_helper::roots::{Accepted, Refused, Root, Roots};

use super::lock;

pub(crate) const ROOTS_FILE: &str = "/var/lib/konedrive/roots.json";

/// The registered roots, and the one lock order the helper has.
///
/// Two locks. `roots` is taken for a question or for putting a new set in
/// place, by every connection thread and some workers, and never held across
/// anything slow. `saving` is held by a registration or an unregistration
/// from its decision until the registrations it decided on are saved and in
/// `roots` ([`Change`]), so that the save itself — two `fsync`s — runs
/// without `roots`. `saving` is always taken before `roots`, never while
/// holding it, and by nothing else: the one place in the helper where a
/// second lock is taken under a first. Neither lock leaves this file.
pub(crate) struct Registrations {
    roots: Mutex<Roots>,
    saving: Mutex<()>,
}

impl Registrations {
    pub(crate) fn new(roots: Roots) -> Self {
        Self { roots: Mutex::new(roots), saving: Mutex::new(()) }
    }

    /// Every registered root, as they are now.
    pub(crate) fn all(&self) -> Vec<Root> {
        lock(&self.roots).iter().cloned().collect()
    }

    /// The root registered under `root_id`, if any.
    pub(crate) fn get(&self, root_id: &str) -> Option<Root> {
        lock(&self.roots).get(root_id).cloned()
    }

    /// Who holds `root_id`, and how many roots `uid` holds, asked at one
    /// moment.
    pub(crate) fn owner_and_held(&self, root_id: &str, uid: u32) -> (Option<u32>, usize) {
        let roots = lock(&self.roots);
        (roots.owner_of(root_id), roots.held_by(uid))
    }

    /// The entries under other ids that stand in `new`'s way, as they are
    /// now (`Roots::conflicting`).
    pub(crate) fn conflicting(&self, new: &Root) -> Vec<Root> {
        lock(&self.roots).conflicting(new)
    }

    pub(crate) fn has_root_for(&self, uid: u32) -> bool {
        lock(&self.roots).has_root_for(uid)
    }

    pub(crate) fn may_act_on(&self, peer_uid: u32, object_dev: u64, object_uid: u32) -> bool {
        lock(&self.roots).may_act_on(peer_uid, object_dev, object_uid)
    }

    /// Begins a registration or an unregistration: nothing else is
    /// registered or unregistered until the guard is dropped.
    pub(crate) fn change(&self) -> Change<'_> {
        Change { roots: &self.roots, _saving: lock(&self.saving) }
    }
}

/// A registration or an unregistration under way: decided on the roots as
/// they are, saved, and only then put in place. Dropped without
/// [`commit`](Self::commit), it changes nothing.
pub(crate) struct Change<'a> {
    roots: &'a Mutex<Roots>,
    _saving: MutexGuard<'a, ()>,
}

impl Change<'_> {
    /// The roots with `root` added and the entries of `gone` dropped, or why
    /// it is refused (`Roots::with_gone`).
    pub(crate) fn with_gone(&self, root: Root, gone: &[Root]) -> Result<Accepted, Refused> {
        lock(self.roots).with_gone(root, gone)
    }

    /// The roots as they are now, to change and [`commit`](Self::commit).
    pub(crate) fn current(&self) -> Roots {
        lock(self.roots).clone()
    }

    /// The roots without `uid`'s `root_id`, and the root that left; `None`
    /// if it is not theirs (`Roots::without`).
    pub(crate) fn without(&self, uid: u32, root_id: &str) -> Option<(Roots, Root)> {
        lock(self.roots).without(uid, root_id)
    }

    /// Saves `next` to [`ROOTS_FILE`] and then puts it in place. Saved
    /// before it is in place, and not under the roots lock: nobody sees a
    /// registration that is not on disk, and a save that fails leaves
    /// nothing to put back.
    pub(crate) fn commit(self, next: Roots) -> io::Result<()> {
        next.save(Path::new(ROOTS_FILE))?;
        *lock(self.roots) = next;
        Ok(())
    }
}

/// How many walk boundaries [`Unregistrations`] remembers, two per
/// unregistration. Anything older than that is assumed to concern everyone.
const UNREGISTRATIONS_REMEMBERED: usize = 4096;

/// A sequence number bumped as each root's unregistration walk begins and as
/// it ends, and which uid's root each bump was for.
///
/// Per uid, and not one count for the machine, because the guard withholds an
/// ignore mark, and a withheld mark costs a permission event on every later
/// open of that file until one is placed. With one count, any local user
/// could register and unregister a root of their own in a loop — with a tree
/// as large as they like, which is as long as each walk takes — and keep
/// every other user's hydrated files from ever being marked. A file is
/// matched to an unregistration by its owner's uid, which is the uid of the
/// root it is in whenever the daemon can act on it at all (`docs/design/hydration.md` §11); a file in
/// someone else's root that this misses is still cleared by the registration
/// walk before that tree is intercepted again.
pub(crate) struct Unregistrations {
    seq: AtomicU64,
    /// `(the sequence number the bump produced, the root's uid)`, oldest
    /// first. Written and read under the lock; `seq` is only ever advanced
    /// under it too, so an entry is there by the time its number is seen.
    recent: Mutex<VecDeque<(u64, u32)>>,
}

impl Unregistrations {
    pub(crate) fn new() -> Self {
        Self { seq: AtomicU64::new(0), recent: Mutex::new(VecDeque::new()) }
    }

    /// The sequence number now: what an event read now is stamped with.
    pub(crate) fn now(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    /// One boundary of `uid`'s unregistration walk.
    pub(crate) fn bump(&self, uid: u32) {
        let mut recent = lock(&self.recent);
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        recent.push_back((seq, uid));
        while recent.len() > UNREGISTRATIONS_REMEMBERED {
            recent.pop_front();
        }
    }

    /// Whether one of `uid`'s roots has been unregistered — a walk begun or
    /// ended — since `since`; with no uid, whether anybody's has. `true`
    /// also when bumps after `since` have been forgotten, since then nobody
    /// can say whose they were.
    pub(crate) fn since(&self, since: u64, uid: Option<u32>) -> bool {
        let recent = lock(&self.recent);
        if self.seq.load(Ordering::SeqCst) == since {
            return false;
        }
        match recent.front() {
            Some(&(oldest, _)) if oldest > since + 1 => true,
            None => true,
            Some(_) => recent.iter().any(|&(seq, who)| seq > since && uid.is_none_or(|u| u == who)),
        }
    }
}

#[cfg(test)]
mod tests;
