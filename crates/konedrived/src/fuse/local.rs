//! What a directory of the mount holds: the items the store has there, as the outbox's rows
//! leave them, and the files and directories OneDrive has no item for yet.
//!
//! An object without an item is a file or a directory of the folder under the mount that
//! carries no item id: made by a program through the mount, and uploaded by the outbox. It
//! is shown where it lies, and is known by its file handle, which goes with it through every
//! rename and is never another object's, as an inode number is once its file is removed. An
//! item is shown where the store has it, or where the user moved it; a name both have is
//! the item's.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use konedrive_tree::{Kind, Placement, Row, Table};

use super::nodes::Key;
use super::outbox::{Above, Waiting};
use super::shared::Shared;
use super::under::Object;
use crate::folder::disk::daemon_owned;

/// One name in a directory of the mount.
pub(super) enum Entry {
    /// An item, under the name the folder shows it by.
    Item(Row),
    /// A file or directory OneDrive has no item for yet.
    Local(Local),
}

pub(super) struct Local {
    pub(super) name: String,
    pub(super) object: Object,
}

impl Entry {
    pub(super) fn name(&self) -> &str {
        match self {
            Entry::Item(row) => &row.name,
            Entry::Local(local) => &local.name,
        }
    }

    pub(super) fn is_dir(&self) -> bool {
        match self {
            Entry::Item(row) => row.kind == Kind::Folder,
            Entry::Local(local) => local.object.meta.is_dir(),
        }
    }

    /// The item's id; `None` for what has no item yet.
    pub(super) fn id(&self) -> Option<&str> {
        match self {
            Entry::Item(row) => Some(&row.id),
            Entry::Local(_) => None,
        }
    }
}

/// A directory of the mount: its item, when it has one, and its place.
#[derive(Debug, Clone)]
pub(super) struct Dir {
    pub(super) id: Option<String>,
    pub(super) rel: PathBuf,
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

impl Shared {
    /// The object without an item at `rel` in the folder under the mount.
    pub(super) fn local(&self, rel: &Path) -> Option<Local> {
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

    /// The item `id` as the folder shows it under `name`, unless it is gone from the folder.
    fn shown_as(&self, id: &str, name: &str) -> Option<Row> {
        let row = self.row(id).filter(|r| r.placement == Placement::Placed)?;
        Some(Row { name: name.to_owned(), ..row })
    }

    /// What `dir` holds under `name`.
    pub(super) fn entry(&self, dir: &Dir, name: &str) -> Option<Entry> {
        let waiting = self.waiting();
        self.item_at(dir, name, &waiting).map(Entry::Item).or_else(|| self.local(&dir.rel.join(name)).map(Entry::Local))
    }

    fn item_at(&self, dir: &Dir, name: &str, waiting: &Waiting) -> Option<Row> {
        let stored = dir.id.as_deref().and_then(|id| self.read(|s| s.child(Table::Items, id, name)).flatten());
        let stays = |row: &Row| row.placement == Placement::Placed && !waiting.gone.contains(&row.id) && !waiting.moved.contains_key(&row.id);
        // What the user moved to this name is what the name is: an item OneDrive has there
        // too is shown once the move is sent, or refused.
        let moved = waiting.moved.iter().find(|(_, (above, as_))| dir.is(above) && as_ == name);
        match moved {
            Some((id, _)) => self.shown_as(id, name),
            None => stored.filter(stays),
        }
    }

    /// Everything `dir` holds.
    pub(super) fn entries(&self, dir: &Dir) -> Vec<Entry> {
        let waiting = self.waiting();
        let stored = dir.id.as_deref().and_then(|id| self.read(|s| s.children(Table::Items, id))).unwrap_or_default();
        let stays = |row: &Row| row.placement == Placement::Placed && !waiting.gone.contains(&row.id) && !waiting.moved.contains_key(&row.id);
        let mut items: Vec<Row> = waiting.moved.iter().filter(|(_, (above, _))| dir.is(above)).filter_map(|(id, (_, name))| self.shown_as(id, name)).collect();
        let moved_here: HashSet<String> = items.iter().map(|row| row.name.clone()).collect();
        items.extend(stored.into_iter().filter(|row| stays(row) && !moved_here.contains(&row.name)));
        let taken: HashSet<String> = items.iter().map(|row| row.name.clone()).collect();
        let mut all: Vec<Entry> = items.into_iter().map(Entry::Item).collect();
        let listed = self.under.list(&dir.rel);
        for name in listed.map(|e| e.file_name()) {
            if name.to_str().is_some_and(|n| !taken.contains(n)) {
                all.extend(self.local(&dir.rel.join(&name)).map(Entry::Local));
            }
        }
        all
    }

    /// The node of `entry`, which stands in the directory `parent`. An object that got its
    /// item since the kernel was told of it keeps its node.
    pub(super) fn ino_of(&self, entry: &Entry, parent: u64) -> u64 {
        let mut nodes = crate::panic::lock(&self.nodes);
        match entry {
            Entry::Local(local) => nodes.ino(Key::Local(local.object.handle.clone()), parent, &local.name),
            Entry::Item(row) => {
                let key = Key::Item(row.id.clone());
                if !nodes.inos.contains_key(&key) {
                    let on_disk = nodes.rel(parent).and_then(|dir| self.under.object(&dir.join(&row.name)));
                    if let Some(object) = on_disk {
                        nodes.rekey(&Key::Local(object.handle), key.clone());
                    }
                }
                nodes.ino(key, parent, &row.name)
            }
        }
    }

    /// What the node `ino` is now, and its place. `None` for a node whose item or object is
    /// gone.
    pub(super) fn node(&self, ino: u64) -> Option<(Entry, PathBuf)> {
        if ino == 1 {
            let id = self.read(|s| s.root_item_id()).flatten().unwrap_or_default();
            let root = Row { id, parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
            return Some((Entry::Item(root), PathBuf::new()));
        }
        let (key, rel, name) = {
            let nodes = crate::panic::lock(&self.nodes);
            let node = nodes.items.get(&ino)?;
            (node.key.clone(), nodes.rel(ino)?, node.place.1.clone())
        };
        match key {
            Key::Item(id) => {
                let row = self.shown_as(&id, &name).filter(|_| !self.waiting().gone.contains(&id))?;
                Some((Entry::Item(row), rel))
            }
            Key::Local(handle) => {
                // Another object at its place is not this node.
                let object = self.under.object(&rel).filter(|o| o.handle == handle)?;
                // It got its item since: the node is the item's from now on.
                if let Some(row) = self.under.carried(&rel).and_then(|id| self.shown_as(&id, &name)) {
                    crate::panic::lock(&self.nodes).rekey(&Key::Local(handle), Key::Item(row.id.clone()));
                    return Some((Entry::Item(row), rel));
                }
                Some((Entry::Local(Local { name, object }), rel))
            }
        }
    }

    /// The directory the node `ino` is.
    pub(super) fn dir(&self, ino: u64) -> Option<Dir> {
        let (entry, rel) = self.node(ino)?;
        entry.is_dir().then(|| Dir { id: entry.id().filter(|id| !id.is_empty()).map(str::to_owned), rel })
    }

    /// The directory the node `ino` stands in, and that directory's node.
    pub(super) fn above(&self, ino: u64) -> Option<(u64, Dir)> {
        let parent = crate::panic::lock(&self.nodes).items.get(&ino)?.place.0;
        Some((parent, self.dir(parent)?))
    }

    /// Where the folder shows item `id`: where the store has it, or where the user moved it
    /// or a folder above it.
    pub(super) fn place(&self, id: &str) -> Option<PathBuf> {
        let waiting = self.waiting();
        let (mut names, mut at) = (Vec::new(), id.to_owned());
        for _ in 0..konedrive_fs::MAX_DEPTH {
            if waiting.gone.contains(&at) {
                return None;
            }
            let row = self.row(&at).filter(|r| r.placement == Placement::Placed)?;
            let (above, name) = match waiting.moved.get(&at) {
                Some((above, name)) => (Some(above.clone()), name.clone()),
                None => (row.parent_id.clone().map(Above::Item), row.name.clone()),
            };
            match above {
                // The drive's root: the folder itself.
                None => return Some(names.iter().rev().collect()),
                Some(Above::Local(dir)) => {
                    names.push(name);
                    return Some(dir.join(names.iter().rev().collect::<PathBuf>()));
                }
                Some(Above::Item(parent)) => {
                    names.push(name);
                    at = parent;
                }
            }
        }
        None
    }

    /// What stands at `rel`, name by name from the root.
    pub(super) fn resolve(&self, rel: &Path) -> Option<Entry> {
        let mut dir = self.dir(1)?;
        let mut names = rel.components().peekable();
        let mut found = self.node(1)?.0;
        while let Some(name) = names.next() {
            found = self.entry(&dir, name.as_os_str().to_str()?)?;
            if names.peek().is_some() {
                dir = Dir { id: found.id().map(str::to_owned), rel: dir.rel.join(name) };
                if !found.is_dir() {
                    return None;
                }
            }
        }
        Some(found)
    }

    /// The directories of the folder under the mount down to `rel`, made where they are
    /// missing, each carrying the id of the item it is the directory of.
    pub(super) fn backing(&self, rel: &Path) -> std::io::Result<()> {
        let root = self.read(|s| s.root_item_id()).flatten();
        self.backing_by(rel, root, |dir, name| self.item_at(dir, name, &self.waiting()).map(|row| row.id))
    }

    /// [`backing`](Self::backing) inside a cycle of the listing, for a place of the tree it
    /// has staged: on the store's own thread, where the mount's reader would not see it.
    pub(super) fn backing_staged(&self, store: &konedrive_tree::TreeStore, rel: &Path) -> std::io::Result<()> {
        let root = store.root_item_id().ok().flatten();
        let staged = |dir: &Dir, name: &str| {
            let row = store.child(Table::Staging, dir.id.as_deref()?, name).ok().flatten()?;
            (row.placement == Placement::Placed).then_some(row.id)
        };
        self.backing_by(rel, root, staged)
    }

    fn backing_by(&self, rel: &Path, root: Option<String>, item: impl Fn(&Dir, &str) -> Option<String>) -> std::io::Result<()> {
        let mut dir = Dir { id: root, rel: PathBuf::new() };
        for name in rel.components() {
            let at = dir.rel.join(name);
            let id = name.as_os_str().to_str().and_then(|n| item(&dir, n));
            match self.under.make_dir(&at) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            if let Some(id) = &id {
                if self.under.carried(&at).as_deref() != Some(id.as_str()) {
                    self.under.carry(&at, id)?;
                }
            }
            dir = Dir { id, rel: at };
        }
        Ok(())
    }
}
