//! What the outbox's rows say of the folder: where an item the user moved stands until its
//! move is sent, which items the user removed, and which hold content of the user's own.
//!
//! The store has every item where OneDrive has it; a change made in the folder is a row of
//! the outbox until it is sent. The mount shows the folder as the user left it: the store,
//! with what these rows say laid over it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_tree::outbox::OutboxKind;

use super::shared::Shared;

/// The directory a moved item stands in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Above {
    Item(String),
    /// A directory OneDrive has no item for yet, by its place.
    Local(PathBuf),
}

/// What the rows say, as of one version of the store.
#[derive(Default)]
pub(super) struct Waiting {
    version: i64,
    /// Items that stand elsewhere than the store has them: the directory, and the name there.
    pub(super) moved: HashMap<String, (Above, String)>,
    /// Items removed here and not yet in OneDrive.
    pub(super) gone: HashSet<String>,
    /// Items whose content was changed here and is not sent yet.
    pub(super) changed: HashSet<String>,
}

impl Shared {
    /// What the outbox says now. Read again only when the store changed since.
    pub(super) fn waiting(&self) -> Arc<Waiting> {
        let store = crate::panic::lock(&self.reader);
        let mut kept = crate::panic::lock(&self.waiting);
        let version = store.data_version().unwrap_or(kept.version.wrapping_add(1));
        if version == kept.version {
            return Arc::clone(&kept);
        }
        let mut now = Waiting { version, ..Waiting::default() };
        for row in store.outbox_rows().unwrap_or_default() {
            let Some(id) = row.item_id.clone() else { continue };
            if row.kind.removes() {
                now.gone.insert(id);
                continue;
            }
            if row.kind == OutboxKind::Update {
                now.changed.insert(id.clone());
            }
            let base = row.base.as_ref().map(|b| (b.parent.as_deref(), b.name.as_deref()));
            let name = row.rel.file_name().and_then(|n| n.to_str());
            if let (Some(base), Some(name)) = (base, name) {
                if base != (row.target_parent.as_deref(), row.target_name.as_deref()) {
                    let above = match &row.target_parent {
                        Some(parent) => Above::Item(parent.clone()),
                        None => Above::Local(row.rel.parent().map(PathBuf::from).unwrap_or_default()),
                    };
                    now.moved.insert(id, (above, name.to_owned()));
                }
            }
        }
        *kept = Arc::new(now);
        Arc::clone(&kept)
    }
}
