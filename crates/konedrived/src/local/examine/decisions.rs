//! Who is who, as one examination decides it: which entry is which item, which items
//! are settled without one, which entries are spoken for. Only through the named
//! transitions here; the listing itself is never edited.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::listing::{EntryIx, Listing};

/// How an item that left its place was decided. Ordered: a folder waits
/// as long as the least settled item inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Settle {
    /// Its row is written.
    Done,
    /// It is in the folder, where this run did not look: found there by the
    /// next one, so nothing is reported for it. A folder it was in waits.
    Elsewhere,
    /// Held back until it can be placed: examined again (undecided).
    Wait,
    /// Held back until the reconcile places it again: no recorded handle,
    /// so nothing can prove it gone (WR4, unproven).
    Unproven,
}

#[derive(Default)]
pub(super) struct Decisions {
    /// Item id → the entry that is the item.
    item: HashMap<String, EntryIx>,
    /// Items decided without an entry: removed (`Done`), or left for later.
    settled: HashMap<String, Settle>,
    /// Entries spoken for: an item's own, its other names, a save-by-rename's
    /// new file, a copy that is listed or was given up.
    taken: HashSet<EntryIx>,
    /// Entries whose id was taken off: new objects now, in the order stripped.
    new: Vec<EntryIx>,
    stripped: HashSet<EntryIx>,
    /// Places the run gave up on while acting: an entry it was refused to
    /// open, strip or read, a copy that could not be stripped or whose name
    /// holds another object by now.
    gave_up: HashSet<PathBuf>,
}

impl Decisions {
    /// Entry `ix` is item `id`.
    pub(super) fn is_item(&mut self, id: &str, ix: EntryIx) {
        self.item.insert(id.to_owned(), ix);
        self.taken.insert(ix);
    }

    /// Entry `ix` is spoken for, and gets no row of its own: another name
    /// of an item's object, a copy that stays as it is.
    pub(super) fn is_link(&mut self, ix: EntryIx) {
        self.taken.insert(ix);
    }

    /// Entry `ix` lost an id that was not its own: a new object.
    pub(super) fn is_new(&mut self, ix: EntryIx) {
        if self.stripped.insert(ix) {
            self.new.push(ix);
        }
    }

    /// Item `id` is decided without an entry.
    ///
    /// The rule that ends the recursion of the removals (`missing_item`,
    /// `removal` and `left_before` ask after one another's items): **an id
    /// is settled before anything is asked on its behalf, and every nested
    /// question is about another id.** `missing_item` and `removal` go on
    /// only for an id that is [`open`](Self::open), and `left_before` asks
    /// only after such ones; an id that stopped being open is never open
    /// again. So a question that comes back to an id stops there, and at
    /// most one question for each id goes on.
    pub(super) fn settle(&mut self, id: &str, how: Settle) {
        self.settled.insert(id.to_owned(), how);
    }

    /// Whether item `id` is still to be decided: no entry is it, and nothing
    /// settled it.
    pub(super) fn open(&self, id: &str) -> bool {
        !self.item.contains_key(id) && !self.settled.contains_key(id)
    }

    /// The entry that is item `id`.
    pub(super) fn item(&self, id: &str) -> Option<EntryIx> {
        self.item.get(id).copied()
    }

    /// How item `id` was settled, if it was.
    pub(super) fn settled(&self, id: &str) -> Option<Settle> {
        self.settled.get(id).copied()
    }

    /// How item `id` stands when it is not [`open`](Self::open): as it was
    /// settled, and `Done` for one an entry is (nothing waits for it).
    pub(super) fn standing(&self, id: &str) -> Settle {
        self.settled(id).unwrap_or(Settle::Done)
    }

    pub(super) fn taken(&self, ix: EntryIx) -> bool {
        self.taken.contains(&ix)
    }

    /// The entries that lost an id, in the order they did.
    pub(super) fn new_objects(&self) -> &[EntryIx] {
        &self.new
    }

    /// The item id entry `ix` carries, as the run sees it: none once it was
    /// taken off.
    pub(super) fn id_of<'l>(&self, listing: &'l Listing, ix: EntryIx) -> Option<&'l str> {
        if self.stripped.contains(&ix) {
            return None;
        }
        listing[ix].id.as_deref()
    }

    /// The place `rel` is not examined in this run after all.
    pub(super) fn give_up(&mut self, rel: &Path) -> bool {
        self.gave_up.insert(rel.to_path_buf())
    }

    /// Whether the run gave up on `rel`, or on a directory above it.
    pub(super) fn gave_up(&self, rel: &Path) -> bool {
        !self.gave_up.is_empty() && rel.ancestors().any(|above| self.gave_up.contains(above))
    }
}
