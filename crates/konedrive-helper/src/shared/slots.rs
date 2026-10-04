use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::lock;

/// A bounded number of places per uid, and optionally a bound across all
/// uids: the connections one uid may hold, and the workers that may wait for
/// one uid's daemon.
///
/// A uid that holds no place has no entry, so the map is the size of the set
/// of uids holding one and no larger.
pub(crate) struct UidSlots {
    held: Mutex<HashMap<u32, usize>>,
    per_uid: usize,
    in_all: Option<usize>,
}

impl UidSlots {
    /// `per_uid` places for each uid; with `in_all`, no more than that many
    /// across every uid together.
    pub(crate) fn new(per_uid: usize, in_all: Option<usize>) -> Arc<Self> {
        Arc::new(Self { held: Mutex::new(HashMap::new()), per_uid, in_all })
    }

    /// One of `uid`'s places, held for as long as the guard lives; `None`
    /// when the uid, or everybody together, holds as many as there are.
    pub(crate) fn take(self: &Arc<Self>, uid: u32) -> Option<UidSlot> {
        let mut held = lock(&self.held);
        // The bound across uids is checked first, and before any per-uid
        // entry is touched: the map is the size of the set of uids holding a
        // place, so this sum is bounded by `in_all` itself and never a hot
        // loop over unrelated state.
        if let Some(in_all) = self.in_all {
            let total: usize = held.values().sum();
            if total >= in_all {
                return None;
            }
        }
        let places = held.entry(uid).or_insert(0);
        if *places >= self.per_uid {
            // No entry is created by this arm: `or_insert(0)` only inserted a
            // zero if there was nothing there, and a zero cannot reach the cap.
            return None;
        }
        *places += 1;
        drop(held);
        Some(UidSlot { slots: Arc::clone(self), uid })
    }
}

/// Holds one of a uid's places in a [`UidSlots`] for as long as it lives,
/// and gives it back however its holder leaves, a panic included.
pub(crate) struct UidSlot {
    slots: Arc<UidSlots>,
    uid: u32,
}

impl Drop for UidSlot {
    fn drop(&mut self) {
        let mut held = lock(&self.slots.held);
        if let Some(places) = held.get_mut(&self.uid) {
            *places -= 1;
            if *places == 0 {
                held.remove(&self.uid);
            }
        }
    }
}

#[cfg(test)]
mod tests;
