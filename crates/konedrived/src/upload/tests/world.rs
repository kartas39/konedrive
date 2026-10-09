//! The one fixture of the worker's tests: [`World`], a folder in a temporary directory that
//! stands for the backing directory, its store with a base, a fake OneDrive holding the same,
//! and a worker for it ([`Harness`]).
//!
//! Nothing is mounted: the folder is as the mount keeps its backing directory. A directory
//! of OneDrive is a directory marked with its id; a file of OneDrive is not there at all
//! until it is downloaded ([`World::download`]), and its downloaded copy is a file marked
//! with its id and marked whole, without a write bit; anything unmarked is local-only. What
//! the mount's write operations do to it, and record, is in [`super::write_operations`].

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, State, XATTR_ITEM_ID, XATTR_ROOT};
use konedrive_tree::outbox::{OutboxKind, OutboxRow, OutboxState};
use konedrive_tree::{Change, Kind, NewTree, Placement, Row, Store, Table, TreeStore};

use super::harness::Harness;
use crate::fake_onedrive as fake;
use crate::folder::locks::InodeLocks;
use crate::folder::root::SyncRoot;
use crate::upload::Engine;

/// The `mtime` of every listed item.
pub(super) const TIME: i64 = 1_700_000_000;

/// An item as OneDrive lists it: a file holds `content`.
pub(super) fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind, content: &[u8]) -> Row {
    Row {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        name: name.into(),
        kind,
        size: if kind == Kind::File { content.len() as u64 } else { 0 },
        mtime: TIME,
        etag: Some(format!("e-{id}")),
        ctag: Some(format!("c-{id}")),
        quickxor: (kind == Kind::File).then(|| fake::qx(content)),
        mime: None,
        placement: Placement::Placed,
    }
}

pub(super) fn folder(id: &str, parent: &str, name: &str) -> Change {
    Change::Upsert(row(id, Some(parent), name, Kind::Folder, b""))
}

pub(super) fn file(id: &str, parent: &str, name: &str, content: &[u8]) -> Change {
    Change::Upsert(row(id, Some(parent), name, Kind::File, content))
}

pub(super) struct World {
    /// The temporary directory the folder is in, kept for as long as the folder is used.
    _dir: tempfile::TempDir,
    pub root: SyncRoot,
    pub store: Store,
    pub locks: InodeLocks,
    pub h: Harness,
}

impl World {
    /// A folder whose base is `changes` (the root, `R`, comes first by itself), with the
    /// directories of OneDrive made and marked, and no file downloaded.
    pub fn new(changes: &[Change]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("OneDrive");
        std::fs::create_dir(&path).unwrap();
        let root_id = "5b0e2c7a-1d3f-4e8a-9b6c-0f1e2d3c4b5a".to_owned();
        xattr::set(&path, XATTR_ROOT, root_id.as_bytes()).unwrap();
        let root = SyncRoot { path, root_id };
        let store = Store::new(TreeStore::in_memory().unwrap());
        let mut all = vec![Change::Root(row("R", None, "", Kind::Folder, b""))];
        all.extend_from_slice(changes);
        store
            .call_blocking(move |s| {
                s.begin_staging(NewTree::Whole)?;
                s.stage(&all)?;
                s.commit_staging("link-1")
            })
            .unwrap();
        let locks = InodeLocks::new();
        let h = Harness::new(&root, &store, &locks);
        let world = World { _dir: dir, root, store, locks, h };
        for change in changes {
            if let Change::Upsert(row) = change {
                if row.kind == Kind::Folder {
                    let rel = world.place_of(&row.id).expect("a folder of the base has a place");
                    std::fs::create_dir_all(world.root.path.join(&rel)).unwrap();
                    xattr::set(world.root.path.join(&rel), XATTR_ITEM_ID, row.id.as_bytes()).unwrap();
                }
            }
        }
        world
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.path.join(rel)
    }

    /// Where the base places the item `id`, relative to the folder.
    pub fn place_of(&self, id: &str) -> Option<PathBuf> {
        let id = id.to_owned();
        self.store.call_blocking(move |s| s.locate(Table::Items, &id)).unwrap().map(|located| located.rel)
    }

    pub fn base(&self, id: &str) -> Option<Row> {
        let id = id.to_owned();
        self.store.call_blocking(move |s| s.get(Table::Items, &id)).unwrap()
    }

    /// The item the base has at `rel`.
    pub fn base_at(&self, rel: &Path) -> Option<Row> {
        let names: Vec<String> = rel.components().map(|name| name.as_os_str().to_string_lossy().into_owned()).collect();
        self.store
            .call_blocking(move |s| {
                let mut at = s.get(Table::Items, "R")?;
                for name in &names {
                    let Some(parent) = at else { return Ok(None) };
                    at = s.child(Table::Items, &parent.id, name)?;
                }
                Ok(at)
            })
            .unwrap()
    }

    /// The file of OneDrive at `rel` is downloaded: its downloaded copy, as a landed
    /// download leaves it.
    pub fn download(&self, rel: &str, content: &[u8]) {
        let base = self.base_at(Path::new(rel)).unwrap_or_else(|| panic!("the base has nothing at {rel}"));
        std::fs::write(self.path(rel), content).unwrap();
        let file = File::options().write(true).open(self.path(rel)).unwrap();
        placeholder::write_item_id(&file, &base.id).unwrap();
        placeholder::write_state(&file, State::Hydrated).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(base.mtime as u64)).unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o444)).unwrap();
    }

    /// Writes the file at `rel` in the folder, and records nothing.
    pub fn write(&self, rel: &str, content: &[u8]) {
        std::fs::write(self.path(rel), content).unwrap();
    }

    pub fn handle(&self, rel: &str) -> FileHandle {
        let path = self.path(rel);
        FileHandle::at(&File::open(path.parent().unwrap()).unwrap(), path.file_name().unwrap()).unwrap()
    }

    pub fn attr(&self, rel: &str, name: &str) -> Option<String> {
        xattr::get(self.path(rel), name).unwrap().map(|v| String::from_utf8(v).unwrap())
    }

    /// The names in the directory `rel` of the folder, sorted.
    pub fn names_in(&self, rel: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.path(rel)).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    pub fn rows(&self) -> Vec<OutboxRow> {
        self.store.call_blocking(move |s| s.outbox_rows()).unwrap()
    }

    pub fn summary(&self) -> Vec<(OutboxKind, String, OutboxState)> {
        self.rows().into_iter().map(|r| (r.kind, r.rel.display().to_string(), r.state)).collect()
    }

    /// The outbox's row at `rel`.
    pub fn row_at(&self, rel: &str) -> Option<OutboxRow> {
        self.rows().into_iter().find(|r| r.rel == Path::new(rel))
    }

    pub fn run(&self) -> Arc<Engine> {
        self.h.run()
    }

    pub fn cloud<T>(&self, f: impl FnOnce(&mut fake::Cloud) -> T) -> T {
        self.h.graph.with(f)
    }

    /// The cloud's content at `path`.
    pub fn content(&self, path: &str) -> Option<Vec<u8>> {
        self.cloud(|c| c.at(path).map(|i| i.content.clone()))
    }

    pub fn id_at(&self, path: &str) -> Option<String> {
        self.cloud(|c| c.at(path).map(|i| i.id.clone()))
    }

    /// The last part of `rel`.
    pub(super) fn name_of(rel: &Path) -> &OsStr {
        rel.file_name().unwrap_or_else(|| panic!("{} has no name", rel.display()))
    }
}
