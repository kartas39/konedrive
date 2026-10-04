//! The detections an examination records: each shape is made in one place, and "a
//! writer has it open" is said one way.

use std::path::Path;

use konedrive_tree::outbox::{Base, Detection, Inode, OutboxKind, OutboxState, Reason};
use konedrive_tree::Row;

use crate::local::entry::{Entry, Type};
use crate::local::RECHECK;

use super::{lossy, Run};

/// Whether a file can be read for an upload now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Readiness {
    Ready,
    /// A program has it open for writing: looked at again at `next_try`.
    Waiting { next_try: i64 },
}

impl Readiness {
    /// Says it on `d`: a row that waits for its writer, or the row as it is.
    pub(super) fn onto(self, d: &mut Detection) {
        if let Readiness::Waiting { next_try } = self {
            d.state = OutboxState::Waiting;
            d.reason = Some(Reason::OpenForWriting);
            d.next_try = Some(next_try);
        }
    }
}

impl Run<'_, '_> {
    /// A file a writer holds, as this run says it.
    pub(super) fn waiting(&self) -> Readiness {
        Readiness::Waiting { next_try: self.ex.now + RECHECK.as_secs() as i64 }
    }

    /// Item `id`, found as `e`: against `base`, at the place `e` stands at.
    pub(super) fn of_item(&self, kind: OutboxKind, id: &str, base: &Row, e: &Entry, local_ctag: Option<&str>) -> Detection {
        // The version the local content derives from: the file's own cTag
        // when it names another than the base's (a download not yet
        // replaced); the eTag guards only the base's own version.
        let same_version = local_ctag.is_none_or(|c| Some(c) == base.ctag.as_deref());
        Detection {
            kind,
            item_id: Some(id.to_owned()),
            base: Some(Base {
                etag: if same_version { base.etag.clone() } else { None },
                ctag: if same_version { base.ctag.clone() } else { local_ctag.map(str::to_owned) },
                parent: base.parent_id.clone(),
                name: Some(base.name.clone()),
            }),
            ..self.standing(kind, e)
        }
    }

    /// An object OneDrive has no item for yet, as `e` stands.
    pub(super) fn new_object(&self, kind: OutboxKind, e: &Entry) -> Detection {
        self.standing(kind, e)
    }

    /// What every detection of an entry that is there says: its object, its
    /// place, and a file's size.
    fn standing(&self, kind: OutboxKind, e: &Entry) -> Detection {
        Detection {
            kind,
            item_id: None,
            inode: Some(e.inode()),
            rel: e.rel.clone(),
            base: None,
            target_parent: self.dir_id(e.dir_rel()),
            target_name: Some(lossy(&e.name)),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: (e.ty == Type::File).then_some(e.size),
        }
    }
}

/// What was at `rel` leaves OneDrive: a `delete`, or a `move-out` to `went_to`. `id`
/// and `base` are the item's, and `None` for an object that is still being created
/// there (its delete learns the id when the create is committed).
pub(super) fn leaves(kind: OutboxKind, id: Option<&str>, base: Option<Base>, inode: Option<Inode>, rel: &Path, went_to: Option<String>) -> Detection {
    Detection {
        kind,
        item_id: id.map(str::to_owned),
        inode,
        rel: rel.to_path_buf(),
        base,
        target_parent: None,
        target_name: went_to,
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    }
}
