//! The mount's write operations, as the tests make them: the change in the backing
//! directory, and the detection of it written into the outbox by the same path the mount
//! writes it ([`crate::local::record::record`]).
//!
//! A detection is built here as the mount builds it for the same event (`fuse::detections`),
//! except that a row of content is ready at once: no test waits for a file to be quiet. The
//! tree lock is not taken: the store's transaction is all a test needs of it.

use std::collections::BTreeSet;
use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, XATTR_ITEM_ID};
use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::{Kind, Row, Table};

use super::world::World;
use crate::local::names;

/// The row as the base has it: what a change to it was made against.
fn base(row: &Row) -> Base {
    Base { etag: row.etag.clone(), ctag: row.ctag.clone(), parent: row.parent_id.clone(), name: Some(row.name.clone()) }
}

fn removed(row: Option<&Row>, handle: Option<FileHandle>, rel: &Path) -> Detection {
    Detection {
        kind: OutboxKind::Delete,
        item_id: row.map(|r| r.id.clone()),
        object: handle,
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

impl World {
    /// The id of the OneDrive directory the directory at `rel` backs: the root's for the
    /// folder itself, the id it is marked with for one the base has. `None` for a local-only
    /// directory.
    fn directory_id(&self, rel: &Path) -> Option<String> {
        if rel.as_os_str().is_empty() {
            return Some("R".to_owned());
        }
        let marked = self.attr(rel.to_str().unwrap(), XATTR_ITEM_ID)?;
        self.base(&marked).filter(|row| row.kind == Kind::Folder).map(|row| row.id)
    }

    /// The row of OneDrive that the entry at `rel` on disk is marked as.
    fn marked_row(&self, rel: &Path) -> Option<Row> {
        self.attr(rel.to_str().unwrap(), XATTR_ITEM_ID).and_then(|id| self.base(&id))
    }

    /// A detection of `kind` at `rel`, which names the directory of OneDrive it is in.
    pub fn detection(&self, kind: OutboxKind, rel: &Path, handle: Option<FileHandle>, size: Option<u64>) -> Detection {
        Detection {
            kind,
            item_id: None,
            object: handle,
            rel: rel.to_path_buf(),
            base: None,
            target_parent: self.directory_id(rel.parent().unwrap_or(Path::new(""))),
            target_name: Some(World::name_of(rel).to_string_lossy().into_owned()),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size,
        }
    }

    /// The local-only entry on disk at `rel`: a file to create, a directory to make. A name
    /// OneDrive refuses is said on the row, which waits for the user.
    fn local_only(&self, rel: &Path) -> Detection {
        let metadata = std::fs::symlink_metadata(self.root.path.join(rel)).unwrap();
        let (kind, size) = if metadata.is_dir() { (OutboxKind::Mkdir, None) } else { (OutboxKind::Create, Some(metadata.len())) };
        let mut d = self.detection(kind, rel, Some(self.handle(rel.to_str().unwrap())), size);
        if let Some(refused) = names::refused(World::name_of(rel)) {
            d.state = OutboxState::Blocked;
            d.reason = Some(refused.reason());
        }
        d
    }

    /// The local-only entry at `rel` and, for a directory, the local-only entries it holds:
    /// itself first. What it holds of OneDrive has rows of its own.
    fn local_only_tree(&self, rel: &Path) -> Vec<Detection> {
        let mut all = vec![self.local_only(rel)];
        if self.root.path.join(rel).is_dir() {
            for name in self.names_in(rel.to_str().unwrap()) {
                let inside = rel.join(name);
                if self.marked_row(&inside).is_none() {
                    all.extend(self.local_only_tree(&inside));
                }
            }
        }
        all
    }

    /// Writes `detections` and `ops` into the outbox, as the mount's recorder does.
    pub fn record(&self, detections: Vec<Detection>, ops: Vec<OutboxOp>) {
        let now = crate::clock::unix_now();
        self.store.call_blocking(move |s| crate::local::record::record(s, detections, ops, now).map(|_| ())).unwrap();
    }

    /// A program made the file `rel`, wrote `content` and closed it.
    pub fn make_file(&self, rel: &str, content: &[u8]) {
        self.write(rel, content);
        self.record(vec![self.local_only(Path::new(rel))], Vec::new());
    }

    /// A program made the directory `rel`.
    pub fn make_directory(&self, rel: &str) {
        std::fs::create_dir(self.path(rel)).unwrap();
        self.record(vec![self.local_only(Path::new(rel))], Vec::new());
    }

    /// The content of the file at `rel` becomes `content`, at a later time, and nothing is
    /// recorded: what a program that still holds the file open has written.
    pub fn overwrite(&self, rel: &str, content: &[u8]) {
        let path = self.path(rel);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        // A downloaded copy has no write bit until it is written.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&path, content).unwrap();
        let file = File::options().write(true).open(&path).unwrap();
        placeholder::set_mtime(&file, before.max(SystemTime::now()) + Duration::from_secs(5)).unwrap();
    }

    /// A program wrote `content` into the file at `rel` and closed it: new content of a file
    /// of OneDrive in its downloaded copy, or of a local-only file.
    pub fn write_file(&self, rel: &str, content: &[u8]) {
        self.overwrite(rel, content);
        self.file_written(rel);
    }

    /// The last writer of the file at `rel` closed it, with its content changed.
    pub fn file_written(&self, rel: &str) {
        let at = Path::new(rel);
        let detection = match self.marked_row(at) {
            Some(row) => {
                let size = std::fs::metadata(self.path(rel)).unwrap().len();
                Detection { item_id: Some(row.id.clone()), base: Some(base(&row)), ..self.detection(OutboxKind::Update, at, Some(self.handle(rel)), Some(size)) }
            }
            None => self.local_only(at),
        };
        self.record(vec![detection], Vec::new());
    }

    /// A program renamed or moved `from` to `to`, where nothing is: a file or directory of
    /// OneDrive (with the entry the folder holds for it, when it holds one), or a local-only
    /// entry with all it holds.
    pub fn rename_entry(&self, from: &str, to: &str) {
        let (from_rel, to_rel) = (Path::new(from), Path::new(to));
        assert!(std::fs::symlink_metadata(self.path(to)).is_err(), "{to} is taken: the tests' rename displaces nothing");
        let on_disk = std::fs::symlink_metadata(self.path(from)).ok();
        let in_onedrive = match &on_disk {
            Some(_) => self.marked_row(from_rel),
            None => Some(self.shown_at(from_rel).unwrap_or_else(|| panic!("nothing is at {from}"))),
        };
        if on_disk.is_some() {
            std::fs::rename(self.path(from), self.path(to)).unwrap();
        }
        let is_dir = on_disk.as_ref().map_or(in_onedrive.as_ref().is_some_and(|row| row.kind == Kind::Folder), |m| m.is_dir());
        let detections = match in_onedrive {
            Some(row) => {
                let id = row.id.clone();
                let updated = self.store.call_blocking(move |s| s.outbox_for_item(&id)).unwrap().iter().any(|r| r.kind == OutboxKind::Update);
                let handle = on_disk.is_some().then(|| self.handle(to));
                vec![Detection { item_id: Some(row.id.clone()), base: Some(base(&row)), same_content: !updated, ..self.detection(OutboxKind::Move, to_rel, handle, None) }]
            }
            None if is_dir => self.local_only_tree(to_rel),
            None => {
                let mut d = self.local_only(to_rel);
                // One that is going up right now goes up as it is: only where it is changes.
                let handle = self.handle(to);
                if self.store.call_blocking(move |s| s.outbox_for_object(&handle)).unwrap().iter().any(|r| r.state == OutboxState::Running) {
                    d.kind = OutboxKind::Move;
                }
                vec![d]
            }
        };
        let ops = if is_dir { vec![OutboxOp::Rebase { from: from_rel.to_path_buf(), to: to_rel.to_path_buf() }] } else { Vec::new() };
        self.record(detections, ops);
    }

    /// The file or directory of OneDrive the mount shows at `rel` with nothing on disk for
    /// it: where a row of the outbox takes it, or where the base has it while no row of
    /// the outbox moves or removes it.
    fn shown_at(&self, rel: &Path) -> Option<Row> {
        let rows = self.rows();
        if let Some(id) = rows.iter().filter(|r| r.rel == rel && r.kind != OutboxKind::Delete).find_map(|r| r.item_id.clone()) {
            return self.base(&id);
        }
        self.base_at(rel).filter(|row| !rows.iter().any(|r| r.item_id.as_deref() == Some(row.id.as_str())))
    }

    /// The names the mount shows in the directory at `rel`: what is on disk there, and the
    /// files and directories of OneDrive with nothing on disk ([`shown_at`](Self::shown_at)).
    fn shown_in(&self, rel: &Path, in_onedrive: Option<&Row>) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        if self.root.path.join(rel).is_dir() {
            names.extend(self.names_in(rel.to_str().unwrap()));
        }
        let rows = self.rows();
        names.extend(rows.iter().filter(|r| r.item_id.is_some() && r.kind != OutboxKind::Delete && r.rel.parent() == Some(rel)).map(|r| World::name_of(&r.rel).to_string_lossy().into_owned()));
        if let Some(id) = in_onedrive.filter(|row| row.kind == Kind::Folder).map(|row| row.id.clone()) {
            let listed = self.store.call_blocking(move |s| s.children(Table::Items, &id)).unwrap();
            names.extend(listed.into_iter().filter(|child| !rows.iter().any(|r| r.item_id.as_deref() == Some(child.id.as_str()))).map(|child| child.name));
        }
        names
    }

    /// A program removed what is at `rel`, a directory with all it holds, as `rm -r` does:
    /// what is inside first. A file or directory of OneDrive is removed from OneDrive by the
    /// outbox; of a local-only entry, what the outbox held is removed.
    pub fn remove_entry(&self, rel: &str) {
        let at = Path::new(rel);
        let on_disk = std::fs::symlink_metadata(self.path(rel)).ok();
        let in_onedrive = match &on_disk {
            Some(_) => self.marked_row(at),
            None => self.shown_at(at),
        };
        assert!(on_disk.is_some() || in_onedrive.is_some(), "nothing is at {rel}");
        for name in self.shown_in(at, in_onedrive.as_ref()) {
            self.remove_entry(at.join(name).to_str().unwrap());
        }
        let handle = on_disk.as_ref().map(|_| self.handle(rel));
        match &on_disk {
            Some(m) if m.is_dir() => std::fs::remove_dir(self.path(rel)).unwrap(),
            Some(_) => std::fs::remove_file(self.path(rel)).unwrap(),
            None => {}
        }
        match (in_onedrive, handle) {
            (Some(row), _) => self.record(vec![removed(Some(&row), None, at)], Vec::new()),
            (None, Some(handle)) => {
                let asked = handle.clone();
                if !self.store.call_blocking(move |s| s.outbox_for_object(&asked)).unwrap().is_empty() {
                    self.record(vec![removed(None, Some(handle), at)], Vec::new());
                }
            }
            (None, None) => {}
        }
    }

    /// The directory of OneDrive at `rel` is removed from the disk with what it holds there,
    /// and only its own removal is recorded: the one row a directory's removal is, whatever
    /// OneDrive still holds inside it. The mount records what is inside too
    /// ([`remove_entry`](Self::remove_entry)); this is the row by itself.
    pub fn remove_directory_alone(&self, rel: &str) {
        let at = Path::new(rel);
        let row = self.marked_row(at).unwrap_or_else(|| panic!("{rel} is no directory of OneDrive"));
        std::fs::remove_dir_all(self.path(rel)).unwrap();
        self.record(vec![removed(Some(&row), None, at)], Vec::new());
    }
}
