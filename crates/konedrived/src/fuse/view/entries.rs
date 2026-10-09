//! What a directory of the folder holds: the items the store has there, as the outbox's rows
//! leave them, and the files and directories OneDrive has no item for yet.
//!
//! An object without an item is a file or a directory of the folder under the mount that
//! carries no item id: made by a program through the mount, and uploaded by the outbox. It
//! is shown where it lies, and is known by its file handle. An item is shown where the store
//! has it, or where the user moved it; a name both have is the item's.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use konedrive_tree::{Kind, Placement, Row, Table};

use super::View;
use crate::folder::disk::daemon_owned;
use crate::fuse::overlay::{Above, Overlay};
use crate::fuse::backing_directory::Object;

/// One name in a directory of the folder.
pub(in crate::fuse) enum Entry {
    /// An item, under the name the folder shows it by.
    Item(Row),
    /// A file or directory OneDrive has no item for yet.
    Local(Local),
}

pub(in crate::fuse) struct Local {
    pub(in crate::fuse) name: String,
    pub(in crate::fuse) object: Object,
}

impl Entry {
    pub(in crate::fuse) fn name(&self) -> &str {
        match self {
            Entry::Item(row) => &row.name,
            Entry::Local(local) => &local.name,
        }
    }

    pub(in crate::fuse) fn is_dir(&self) -> bool {
        match self {
            Entry::Item(row) => row.kind == Kind::Folder,
            Entry::Local(local) => local.object.meta.is_dir(),
        }
    }

    /// The item's id; `None` for what has no item yet.
    pub(in crate::fuse) fn id(&self) -> Option<&str> {
        match self {
            Entry::Item(row) => Some(&row.id),
            Entry::Local(_) => None,
        }
    }
}

/// A directory of the folder: its item, when it has one, and its place.
#[derive(Debug, Clone)]
pub(in crate::fuse) struct Dir {
    pub(in crate::fuse) id: Option<String>,
    pub(in crate::fuse) rel: PathBuf,
}

impl Dir {
    fn is(&self, above: &Above) -> bool {
        match above {
            Above::Item(id) => self.id.as_deref() == Some(id),
            // By its place alone: the directory may have got its item since the row was
            // written, and the row its parent only when the worker next looks at it.
            Above::Local(rel) => *rel == self.rel,
        }
    }
}

/// Whether an item the store has in a directory still stands there, for what `overlay` says.
fn stays(row: &Row, overlay: &Overlay) -> bool {
    row.placement == Placement::Placed && !overlay.gone.contains(&row.id) && !overlay.moved.contains_key(&row.id)
}

impl View {
    /// The object without an item at `rel` in the folder under the mount.
    pub(in crate::fuse) fn local(&self, rel: &Path) -> Option<Local> {
        let name = rel.file_name()?;
        if daemon_owned(name) {
            return None;
        }
        let object = self.under.object(rel).filter(|o| o.meta.is_file() || o.meta.is_dir())?;
        // One that carries the id of an item the store does not have is not an item's yet:
        // its upload is being committed, or the store was made anew.
        let of_an_item = self.under.carried(rel).is_some_and(|id| self.row(&id).is_some());
        (!of_an_item).then(|| Local { name: name.to_string_lossy().into_owned(), object })
    }

    /// The item `id` as the folder shows it under `name`, unless the store has it out of the
    /// folder. What the outbox's rows say of it is the caller's to ask.
    pub(in crate::fuse) fn shown_as(&self, id: &str, name: &str) -> Option<Row> {
        let row = self.row(id).filter(|r| r.placement == Placement::Placed)?;
        Some(Row { name: name.to_owned(), ..row })
    }

    /// What `dir` holds under `name`.
    pub(in crate::fuse) fn entry(&self, dir: &Dir, name: &str) -> Option<Entry> {
        self.item_at(dir, name).map(Entry::Item).or_else(|| self.local(&dir.rel.join(name)).map(Entry::Local))
    }

    /// The item `dir` holds under `name`.
    pub(in crate::fuse) fn item_at(&self, dir: &Dir, name: &str) -> Option<Row> {
        let overlay = self.overlay();
        let stored = dir.id.as_deref().and_then(|id| self.read(|s| s.child(Table::Items, id, name)).flatten());
        // What the user moved to this name is what the name is: an item OneDrive has there
        // too is shown once the move is sent, or refused.
        let moved = overlay.moved.iter().find(|(_, (above, as_))| dir.is(above) && as_ == name);
        match moved {
            Some((id, _)) => self.shown_as(id, name),
            None => stored.filter(|row| stays(row, &overlay)),
        }
    }

    /// Everything `dir` holds.
    pub(in crate::fuse) fn entries(&self, dir: &Dir) -> Vec<Entry> {
        let overlay = self.overlay();
        let stored = dir.id.as_deref().and_then(|id| self.read(|s| s.children(Table::Items, id))).unwrap_or_default();
        let mut items: Vec<Row> = overlay.moved.iter().filter(|(_, (above, _))| dir.is(above)).filter_map(|(id, (_, name))| self.shown_as(id, name)).collect();
        let moved_here: HashSet<String> = items.iter().map(|row| row.name.clone()).collect();
        items.extend(stored.into_iter().filter(|row| stays(row, &overlay) && !moved_here.contains(&row.name)));
        let taken: HashSet<String> = items.iter().map(|row| row.name.clone()).collect();
        let mut all: Vec<Entry> = items.into_iter().map(Entry::Item).collect();
        for name in self.under.list(&dir.rel).map(|e| e.file_name()) {
            if name.to_str().is_some_and(|n| !taken.contains(n)) {
                all.extend(self.local(&dir.rel.join(&name)).map(Entry::Local));
            }
        }
        all
    }
}
