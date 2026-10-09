//! What a change a program made means for the outbox: a new file or directory, new content,
//! a move, a removal. The mount saw the change itself, so nothing is worked out from the
//! folder afterwards; the rules for what is sent at all are the account's own
//! ([`crate::local::names`], the ignore list).

use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxOp, OutboxState, Reason};
use konedrive_tree::{Row, Table, TreeError, TreeStore};

use super::view::Dir;
use super::under::Object;
use super::shared::Shared;
use crate::local::names;

/// The item as the base has it: what a change to it was made against.
fn base(row: &Row) -> Base {
    Base { etag: row.etag.clone(), ctag: row.ctag.clone(), parent: row.parent_id.clone(), name: Some(row.name.clone()) }
}

/// A place in the folder: the directory, and the name in it.
pub(super) struct Place<'a> {
    pub(super) dir: &'a Dir,
    pub(super) name: &'a str,
}

impl Place<'_> {
    pub(super) fn rel(&self) -> PathBuf {
        self.dir.rel.join(self.name)
    }

    fn standing(&self, kind: OutboxKind, object: Option<FileHandle>, size: Option<u64>) -> Detection {
        Detection {
            kind,
            item_id: None,
            object,
            rel: self.rel(),
            base: None,
            target_parent: self.dir.id.clone(),
            target_name: Some(self.name.to_owned()),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size,
        }
    }

    /// The file or directory `object`, which OneDrive has no item for yet, stands here. A name
    /// OneDrive refuses, or a file too large for it, is said on the row and waits for the
    /// user; a file a program still writes waits for its last writer.
    pub(super) fn new(&self, object: &Object, open: bool) -> Detection {
        let meta = &object.meta;
        let kind = if meta.is_dir() { OutboxKind::Mkdir } else { OutboxKind::Create };
        let mut d = self.standing(kind, Some(object.handle.clone()), meta.is_file().then(|| meta.len()));
        let refused = names::refused(self.name.as_ref()).or((meta.is_file() && meta.len() > names::MAX_FILE_SIZE).then_some(names::Refused::TooLarge));
        if let Some(refused) = refused {
            d.state = OutboxState::Blocked;
            d.reason = Some(refused.reason());
        } else if open {
            waits(&mut d);
        } else if meta.is_file() {
            settles(&mut d);
        }
        d
    }

    /// The item `base` stands here: moved (`Move`) or with new content (`Update`), in
    /// `object`, when the folder holds one for it.
    pub(super) fn of_item(&self, kind: OutboxKind, row: &Row, object: Option<&Object>) -> Detection {
        let size = object.filter(|o| o.meta.is_file() && kind == OutboxKind::Update).map(|o| o.meta.len());
        Detection { item_id: Some(row.id.clone()), base: Some(base(row)), ..self.standing(kind, object.map(|o| o.handle.clone()), size) }
    }
}

/// A row that waits for the last program writing its file: recorded ready when that one
/// closes it, and looked at again by the worker itself in case that is never heard.
pub(super) fn waits(d: &mut Detection) {
    d.state = OutboxState::Waiting;
    d.reason = Some(Reason::OpenForWriting);
    d.next_try = Some(crate::clock::unix_now() + crate::local::RECHECK.as_secs() as i64);
}

/// A row of content just closed, which goes a moment later: a program that saves through a
/// file of another name renames it right after closing it, and that name is never sent.
pub(super) fn settles(d: &mut Detection) {
    d.state = OutboxState::Waiting;
    d.reason = Some(Reason::Changed);
    d.next_try = Some(crate::clock::unix_now() + crate::local::QUIET.as_secs() as i64);
}

/// What was at `rel` is removed: the item `row`, or an object without one.
pub(super) fn leaves(row: Option<&Row>, object: Option<FileHandle>, rel: &Path) -> Detection {
    Detection {
        kind: OutboxKind::Delete,
        item_id: row.map(|r| r.id.clone()),
        object,
        rel: rel.to_path_buf(),
        base: row.map(base),
        target_parent: None,
        target_name: None,
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    }
}

/// The item `id` as the base has it now.
pub(super) fn based(s: &TreeStore, id: &str) -> Result<Option<Row>, TreeError> {
    s.get(Table::Items, id)
}

/// An object without an item is gone, or stays on this computer from now on: what the outbox
/// held of it goes. Nothing for an object the outbox never heard of.
pub(super) fn forgotten(s: &TreeStore, object: &FileHandle, rel: &Path) -> Result<Vec<Detection>, TreeError> {
    Ok(if s.outbox_for_object(object)?.is_empty() { Vec::new() } else { vec![leaves(None, Some(object.clone()), rel)] })
}

/// A directory moved: the rows of what is below it follow.
pub(super) fn rebase(from: &Path, to: &Path) -> OutboxOp {
    OutboxOp::Rebase { from: from.to_path_buf(), to: to.to_path_buf() }
}

impl Shared {
    /// Whether what stands as `name` in `dir` and has no item stays on this computer
    /// ([`super::policy::Policy::stays_local`]).
    pub(super) fn stays_local(&self, dir: &Dir, name: &str) -> bool {
        self.policy.stays_local(&dir.rel, name, |at| self.under.carried(at).is_some())
    }

    /// The detections of everything without an item at and below the directory `rel`, which
    /// is sent from now on: itself, and what it holds, by the same rules as anything new.
    pub(super) fn below(&self, dir: &Dir, name: &str, into: &mut Vec<Detection>) {
        let rel = dir.rel.join(name);
        let Some(object) = self.under.object(&rel) else { return };
        if self.stays_local(dir, name) || self.under.carried(&rel).is_some() || !(object.meta.is_dir() || object.meta.is_file()) {
            return;
        }
        into.push(Place { dir, name }.new(&object, false));
        if object.meta.is_dir() && into.len() < MOST_BELOW {
            let inside = Dir { id: None, rel: rel.clone() };
            for entry in self.under.list(&rel) {
                if let Some(name) = entry.file_name().to_str() {
                    self.below(&inside, name, into);
                }
            }
        }
    }
}

/// How many objects one move records at most: a tree larger than this is recorded as far as
/// that, and the rest as it is next changed.
const MOST_BELOW: usize = 50_000;
