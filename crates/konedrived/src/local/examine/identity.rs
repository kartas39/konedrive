//! Which of the objects carrying an item's id is the item (invariant I2,
//! `docs/limitations/F53.md`). [`identify`] is the whole rule and touches nothing: what
//! the store says of the id and what was listed go in, the decision comes out.
//! [`Run::identities`] asks the store, and [`Run::settle_identity`] carries a decision out.

use std::path::Path;
use std::rc::Rc;

use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::LocalSkip;
use konedrive_tree::{Kind, Row};

use super::facts::Expect;
use super::hands::Opened;
use super::listing::EntryIx;
use super::{ExamineError, Run};
use crate::local::entry::{Entry, Type};

/// What the store says of one id.
#[derive(Debug, Clone, Copy)]
pub(super) enum Known<'a> {
    /// The base has no such item. `placing`: the new tree a reconcile is
    /// placing right now has it.
    Unknown { placing: bool },
    /// The base has it and does not place it in the folder. `placing`: where
    /// that new tree places it, if it does.
    Unplaced { kind: Kind, placing: Option<&'a Path> },
    /// The base places it. `recorded` is the object it records for it;
    /// `expected` where the item should stand: where a waiting row last saw
    /// it, or its base place (also when a row says it was removed: an object
    /// there takes the removal back).
    Placed { kind: Kind, recorded: Option<&'a FileHandle>, expected: Option<&'a Path> },
}

/// One listed entry, as the rule sees it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Seen<'a> {
    pub(super) ix: EntryIx,
    pub(super) entry: &'a Entry,
    /// Its name is on the ignore list.
    pub(super) ignored: bool,
    /// A row without an item id waits for its object (a create or a mkdir
    /// between its two commit steps)...
    pub(super) pending: bool,
    /// ... and the worker is sending it right now.
    pub(super) creating: bool,
}

/// Who is who among the entries carrying one id.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Identity {
    /// The entry that is the item.
    pub(super) item: Option<EntryIx>,
    /// Other names of the item's object: hard links OneDrive cannot hold.
    pub(super) links: Vec<EntryIx>,
    /// Entries that carry the id and are not the item: the user's own.
    pub(super) copies: Vec<EntryIx>,
    /// The item is the recorded object itself, so the copies are certainly
    /// not the item.
    pub(super) certain: bool,
    /// Entries a reconcile is placing right now: looked at again.
    pub(super) waits: Vec<EntryIx>,
    /// A save by rename with a backup.
    pub(super) backup: Option<Backup>,
}

/// Rename to a backup, write new (vim's `file~`): the item's recorded object
/// sits under an ignored name beside where the item is expected (`old`, its
/// names), and another file stands there (`new`): the item's new content.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Backup {
    pub(super) old: Vec<EntryIx>,
    pub(super) new: EntryIx,
}

/// Which of `carriers`, the entries carrying item `id`'s id, is the item.
///
/// An object is the item only if the base places the item and the object is
/// the one the base records. When the recorded object is not among those
/// seen, or none is recorded (a rebuilt store, a placement that never
/// reached its record), it is the object standing where the item is
/// expected. Every other object carrying the id is a copy, the user's own,
/// wherever the item's object is and whatever became of it: nothing is
/// asked, and no row of the item is made from a copy. An item with no entry
/// is left to what is missing, if its place was looked at.
///
/// `at_place` is the entry standing where the item is expected, whatever id
/// it carries.
pub(super) fn identify(id: &str, known: Known<'_>, carriers: &[Seen<'_>], at_place: Option<Seen<'_>>) -> Identity {
    let mut out = Identity::default();
    let (kind, recorded, expected) = match known {
        Known::Unknown { placing } => {
            for seen in carriers {
                if placing {
                    // Its swap follows.
                    out.waits.push(seen.ix);
                } else if !seen.pending {
                    // Not surely nobody's: another account's folder may
                    // have it, and wait to download it where it went. (One
                    // with a pending row is adopted by its replay, §5.)
                    out.copies.push(seen.ix);
                }
            }
            return out;
        }
        Known::Unplaced { kind, .. } => (kind, None, None),
        Known::Placed { kind, recorded, expected } => (kind, recorded, expected),
    };
    let want = if kind == Kind::Folder { Type::Dir } else { Type::File };
    // Its own id with the other kind: not the item.
    out.copies.extend(carriers.iter().filter(|seen| seen.entry.ty != want).map(|seen| seen.ix));
    // By object: one inode may have several names (hard links).
    let mut groups: Vec<Vec<Seen<'_>>> = Vec::new();
    for seen in carriers.iter().filter(|seen| seen.entry.ty == want) {
        match groups.iter_mut().find(|group| group[0].entry.same_object(seen.entry)) {
            Some(group) => group.push(*seen),
            None => groups.push(vec![*seen]),
        }
    }
    for group in &mut groups {
        group.sort_by(|a, b| a.entry.rel.cmp(&b.entry.rel));
    }
    let is_at = |group: &[Seen<'_>], rel: Option<&Path>| rel.is_some_and(|rel| group.iter().any(|seen| seen.entry.rel == rel));
    let names = |group: &[Seen<'_>]| group.iter().map(|seen| seen.ix).collect::<Vec<_>>();
    if let Known::Unplaced { placing, .. } = known {
        // Nothing on disk is the item. Placed right here by the new tree, a
        // reconcile is placing it now, and its swap follows.
        for group in &groups {
            if is_at(group, placing) {
                out.waits.extend(names(group));
            } else {
                out.copies.extend(names(group));
            }
        }
        return out;
    }
    let original = recorded.and_then(|handle| groups.iter().position(|group| group[0].entry.handle.as_ref() == Some(handle)));
    if let (Kind::File, Some(o), Some(rel)) = (kind, original, expected) {
        let group = &groups[o];
        let beside = group.iter().all(|seen| seen.entry.dir_rel() == rel.parent().unwrap_or(Path::new(""))) && group.iter().any(|seen| seen.ignored);
        // A new file, or one that copied the old one's attributes — not one
        // the worker is creating right now.
        let newcomer = at_place.filter(|seen| {
            let e = seen.entry;
            !group.iter().any(|old| old.ix == seen.ix) && e.ty == Type::File && (e.id.is_none() || e.id.as_deref() == Some(id)) && !seen.creating
        });
        if let (true, Some(new)) = (beside && !is_at(group, expected), newcomer) {
            out.backup = Some(Backup { old: names(group), new: new.ix });
            for (n, other) in groups.iter().enumerate() {
                if n != o && !other.iter().any(|seen| seen.ix == new.ix) {
                    out.copies.extend(names(other));
                }
            }
            return out;
        }
    }
    // The recorded object; not seen, the one where the item is expected: the
    // object a placement or a replacement left before its record, an
    // editor's new inode that copied the attributes.
    let Some(pick) = original.or_else(|| groups.iter().position(|group| is_at(group, expected))) else {
        out.copies.extend(groups.iter().flat_map(|group| names(group)));
        return out;
    };
    let group = &groups[pick];
    let item = group.iter().find(|seen| Some(seen.entry.rel.as_path()) == expected).unwrap_or(&group[0]).ix;
    out.item = Some(item);
    out.links = names(group).into_iter().filter(|&ix| ix != item).collect();
    out.certain = original == Some(pick);
    for (n, other) in groups.iter().enumerate() {
        if n != pick {
            out.copies.extend(names(other));
        }
    }
    out
}

impl<'l> Run<'_, '_, 'l> {
    /// [`identify`] for the entries `carriers` of `id`, with what the store
    /// says of it; and the item's row, when the base has one.
    pub(super) fn identity(&mut self, id: &str, carriers: &[EntryIx]) -> Result<(Identity, Option<Rc<Row>>), ExamineError> {
        let listing = self.listing;
        let seen = |run: &Self, ix: EntryIx| {
            let entry = &listing[ix];
            let pending = run.facts.rows.pending(entry).is_some();
            Seen { ix, entry, ignored: run.ex.ignore.matches(&entry.name), pending, creating: pending && run.facts.rows.being_created(entry) }
        };
        let carriers: Vec<Seen<'_>> = carriers.iter().map(|&ix| seen(self, ix)).collect();
        let Some(base) = self.facts.row(id)? else {
            let placing = self.facts.being_placed(id)?;
            return Ok((identify(id, Known::Unknown { placing }, &carriers, None), None));
        };
        if !self.facts.located(id)?.is_some_and(|l| l.placed) {
            let placing = self.facts.placed_anew(id)?;
            return Ok((identify(id, Known::Unplaced { kind: base.kind, placing: placing.as_deref() }, &carriers, None), Some(base)));
        }
        let recorded = self.facts.recorded(id)?;
        let expected = match self.facts.expected(id)? {
            Expect::At(rel) => Some(rel),
            _ => self.facts.located(id)?.map(|l| l.rel),
        };
        let at_place = expected.as_deref().and_then(|rel| listing.at(rel)).map(|ix| seen(self, ix));
        let known = Known::Placed { kind: base.kind, recorded: recorded.as_ref(), expected: expected.as_deref() };
        Ok((identify(id, known, &carriers, at_place), Some(base)))
    }

    /// Carries the decision about `id` out: what waits is looked at again,
    /// the item's entry and its other names are marked, the copies handled
    /// ([`copy`](Self::copy)), a save by rename recorded.
    pub(super) fn settle_identity(&mut self, id: &str, identity: &Identity, base: Option<&Row>) -> Result<(), ExamineError> {
        let listing = self.listing;
        for &ix in &identity.waits {
            self.recheck(&listing[ix]);
        }
        if let Some(item) = identity.item {
            self.decisions.is_item(id, item);
        }
        for &ix in &identity.links {
            self.decisions.is_link(ix);
            let e = &listing[ix];
            if !self.ex.ignore.matches(&e.name) {
                self.skip(&e.rel, LocalSkip::HardLink);
            }
        }
        for &ix in &identity.copies {
            self.copy(ix, identity.certain)?;
        }
        if let (Some(backup), Some(base)) = (&identity.backup, base) {
            // The backup stays the user's, stripped if it was downloaded (a
            // placeholder stays managed and fills on open).
            for &ix in &backup.old {
                self.decisions.is_link(ix);
                let e = &listing[ix];
                if e.hydrated() && self.strip(e)? {
                    self.outcome.out.stripped.push(e.rel.clone());
                }
            }
            self.decisions.is_item(id, backup.new);
            self.save_by_rename(id, base, backup.new)?;
        }
        Ok(())
    }

    /// Takes konedrive's marks off the object `e` was listed as
    /// ([`Hands::strip`](super::hands::Hands::strip)), and says whether it
    /// did. A name that holds another object by now is left as it is, and
    /// looked at again; a refusal passes the entry over.
    pub(super) fn strip(&mut self, e: &Entry) -> Result<bool, ExamineError> {
        let stripped = self.hands.strip(e);
        match self.entry_io(e, stripped)? {
            Some(Opened::Same(_)) => Ok(true),
            Some(Opened::Gone) => {
                self.recheck(e);
                Ok(false)
            }
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests;
