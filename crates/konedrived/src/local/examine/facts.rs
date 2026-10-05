//! The store as one examination reads it: the live rows, and what the base says of
//! each item, asked once and kept. The only part of a run that calls the store before
//! [`finish`](super::Run::finish) writes it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::{Inode, LocalSkipped, OutboxKind, OutboxRow, OutboxState};
use konedrive_tree::{Located, Placement, Row, Store, Table, TreeError, TreeStore};

use crate::local::entry::Entry;

/// Where an item is expected to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Expect {
    At(PathBuf),
    /// Deleted or moved out already (a row says so), or not placed.
    Nowhere,
    /// Not in the base.
    Unknown,
}

pub(super) struct Facts<'e> {
    store: &'e Store,
    pub(super) root_id: String,
    /// The live rows before this examination.
    pub(super) rows: Rows,
    base: HashMap<String, Option<Rc<Row>>>,
    /// Item id → the local object the base records (`items.local_handle`),
    /// and where the base places the item: asked with the item's row, in one
    /// job of the store's thread.
    recorded: HashMap<String, (Option<FileHandle>, Option<Located>)>,
    expected: HashMap<String, Expect>,
}

impl<'e> Facts<'e> {
    pub(super) fn new(store: &'e Store, root_id: String) -> Result<Self, TreeError> {
        let rows = Rows::new(store.call_blocking(move |s| s.outbox_rows())?);
        Ok(Facts { store, root_id, rows, base: HashMap::new(), recorded: HashMap::new(), expected: HashMap::new() })
    }

    fn ask<T: Send + 'static>(&self, f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        self.store.call_blocking(f)
    }

    /// Item `id`'s row in the base.
    pub(super) fn row(&mut self, id: &str) -> Result<Option<Rc<Row>>, TreeError> {
        if let Some(row) = self.base.get(id) {
            return Ok(row.clone());
        }
        self.read(id)?;
        Ok(self.base.get(id).cloned().flatten())
    }

    /// Item `id`'s row, recorded object and place in the base, in one job.
    fn read(&mut self, id: &str) -> Result<(), TreeError> {
        let asked = id.to_owned();
        let (row, handle, located) = self.ask(move |s| Ok((s.get(Table::Items, &asked)?, s.local_handle(&asked)?, s.locate(Table::Items, &asked)?)))?;
        self.base.entry(id.to_owned()).or_insert(row.map(Rc::new));
        self.recorded.insert(id.to_owned(), (handle, located));
        Ok(())
    }

    /// The local object the base records for item `id`.
    pub(super) fn recorded(&mut self, id: &str) -> Result<Option<FileHandle>, TreeError> {
        if !self.recorded.contains_key(id) {
            self.read(id)?;
        }
        Ok(self.recorded.get(id).and_then(|(handle, _)| handle.clone()))
    }

    /// Where the base places item `id`.
    pub(super) fn located(&mut self, id: &str) -> Result<Option<Located>, TreeError> {
        if !self.recorded.contains_key(id) {
            self.read(id)?;
        }
        Ok(self.recorded.get(id).and_then(|(_, located)| located.clone()))
    }

    /// Where item `id` should be: where its live row last saw it, or else
    /// its base name under where its parent should be — so the items in a
    /// folder with a pending move are looked for where the folder is now.
    /// Cached, and derived from the parent's answer, so that a Full scan
    /// costs one lookup per item rather than one walk up the tree.
    pub(super) fn expected(&mut self, id: &str) -> Result<Expect, TreeError> {
        self.expected_at(id, 0)
    }

    fn expected_at(&mut self, id: &str, depth: usize) -> Result<Expect, TreeError> {
        if let Some(expect) = self.expected.get(id) {
            return Ok(expect.clone());
        }
        let last = self.rows.of_item(id).next_back().map(|row| (row.kind.removes(), row.rel.clone()));
        let expect = match last {
            Some((true, _)) => Expect::Nowhere,
            Some((false, rel)) => Expect::At(rel),
            None => match self.row(id)? {
                None => Expect::Unknown,
                Some(row) if row.placement != Placement::Placed => Expect::Nowhere,
                Some(row) => match row.parent_id.as_deref() {
                    None => Expect::At(PathBuf::new()),
                    Some(parent) if parent == self.root_id => Expect::At(PathBuf::from(&row.name)),
                    // A cycle or corruption, not a drive.
                    Some(_) if depth > konedrive_fs::MAX_DEPTH => Expect::Nowhere,
                    Some(parent) => match self.expected_at(parent, depth + 1)? {
                        Expect::At(dir) => Expect::At(dir.join(&row.name)),
                        _ => Expect::Nowhere,
                    },
                },
            },
        };
        self.expected.insert(id.to_owned(), expect.clone());
        Ok(expect)
    }

    /// The items the base places directly in folder `parent`; their rows are kept.
    pub(super) fn children(&mut self, parent: &str) -> Result<Vec<Rc<Row>>, TreeError> {
        let asked = parent.to_owned();
        let children = self.ask(move |s| s.children(Table::Items, &asked))?;
        Ok(children
            .into_iter()
            .filter(|child| child.placement == Placement::Placed)
            .map(|child| match self.base.get(&child.id) {
                Some(Some(row)) => row.clone(),
                _ => {
                    let row = Rc::new(child);
                    self.base.insert(row.id.clone(), Some(row.clone()));
                    row
                }
            })
            .collect())
    }

    /// Everything the base has inside folder `id`.
    pub(super) fn descendants(&self, id: &str) -> Result<Vec<String>, TreeError> {
        let id = id.to_owned();
        self.ask(move |s| s.descendants(Table::Items, &id))
    }

    /// Whether the new tree a reconcile is placing right now has `id`.
    pub(super) fn being_placed(&self, id: &str) -> Result<bool, TreeError> {
        let id = id.to_owned();
        Ok(self.ask(move |s| s.get(Table::Staging, &id))?.is_some())
    }

    /// Where that new tree places `id`, if it does.
    pub(super) fn placed_anew(&self, id: &str) -> Result<Option<PathBuf>, TreeError> {
        let id = id.to_owned();
        Ok(self.ask(move |s| s.locate(Table::Staging, &id))?.filter(|l| l.placed).map(|l| l.rel))
    }

    /// Where the item or pending row an event's object handle names is
    /// expected: the place to look.
    pub(super) fn expected_of_handle(&mut self, handle: &FileHandle) -> Result<Option<PathBuf>, TreeError> {
        if let Some(item) = self.ask({ let handle = handle.to_owned(); move |s| s.item_by_handle(&handle) })? {
            if let Expect::At(rel) = self.expected(&item.id)? {
                return Ok(Some(rel));
            }
        }
        let handle = handle.clone();
        Ok(self.ask(move |s| s.outbox_by_handle(&handle))?.map(|row| row.rel))
    }

    /// The list of what stays local, as the store has it.
    pub(super) fn skipped(&self) -> Result<Vec<LocalSkipped>, TreeError> {
        self.ask(|s| s.local_skipped())
    }

    /// How many items the folder places.
    pub(super) fn placed(&self) -> Result<u64, TreeError> {
        Ok(self.ask(|s| s.counts())?.placed)
    }
}

/// The live rows as an examination looks them up: by item, by
/// local object, by place, by parent directory, built once per run, so that
/// no step walks every row for each entry, item or directory.
pub(super) struct Rows {
    /// In `seq` order.
    all: Vec<OutboxRow>,
    /// Item id → its rows, oldest first.
    by_item: HashMap<String, Vec<usize>>,
    /// Rows without an item id, by the handle of their object...
    by_handle: HashMap<FileHandle, Vec<usize>>,
    /// ... and by its inode.
    by_inode: HashMap<(u64, u64), Vec<usize>>,
    /// Every row by its place, in path order: what is below a directory is
    /// one range.
    by_rel: BTreeMap<PathBuf, Vec<usize>>,
    /// Running `mkdir` rows without an item id.
    making: Vec<usize>,
}

impl Rows {
    fn new(all: Vec<OutboxRow>) -> Self {
        let mut rows = Rows {
            all,
            by_item: HashMap::new(),
            by_handle: HashMap::new(),
            by_inode: HashMap::new(),
            by_rel: BTreeMap::new(),
            making: Vec::new(),
        };
        for (i, row) in rows.all.iter().enumerate() {
            rows.by_rel.entry(row.rel.clone()).or_default().push(i);
            match (&row.item_id, &row.inode) {
                (Some(id), _) => rows.by_item.entry(id.clone()).or_default().push(i),
                (None, Some(inode)) => {
                    if let Some(handle) = &inode.handle {
                        rows.by_handle.entry(handle.clone()).or_default().push(i);
                    }
                    rows.by_inode.entry((inode.dev, inode.ino)).or_default().push(i);
                    if row.kind == OutboxKind::Mkdir && row.state == OutboxState::Running {
                        rows.making.push(i);
                    }
                }
                (None, None) => {}
            }
        }
        rows
    }

    pub(super) fn iter(&self) -> std::slice::Iter<'_, OutboxRow> {
        self.all.iter()
    }

    /// The live rows of item `id`, oldest first.
    pub(super) fn of_item(&self, id: &str) -> impl DoubleEndedIterator<Item = &OutboxRow> + '_ {
        self.by_item.get(id).into_iter().flatten().map(|&i| &self.all[i])
    }

    /// The rows without an item id whose object is `e`'s ([`Inode::same_object`]), oldest first.
    fn of_object(&self, e: &Entry) -> Vec<&OutboxRow> {
        let mut found: Vec<usize> = Vec::new();
        if let Some(handle) = &e.handle {
            found.extend(self.by_handle.get(handle).into_iter().flatten());
        }
        found.extend(self.by_inode.get(&(e.dev, e.ino)).into_iter().flatten());
        found.sort_unstable();
        found.dedup();
        let object = e.inode();
        found.into_iter().map(|i| &self.all[i]).filter(|row| row.inode.as_ref().is_some_and(|i| i.same_object(&object))).collect()
    }

    /// The live row of a local object with no item id yet.
    pub(super) fn pending(&self, e: &Entry) -> Option<&OutboxRow> {
        self.of_object(e).last().copied()
    }

    /// Whether the worker is creating `e`'s object in OneDrive right now.
    pub(super) fn being_created(&self, e: &Entry) -> bool {
        self.of_object(e).iter().any(|row| row.state == OutboxState::Running)
    }

    /// Whether the worker is making the directory that is the object (`dev`, `ino`) in
    /// OneDrive right now: a running `mkdir` of it.
    pub(super) fn being_made(&self, object: impl FnOnce() -> Option<(u64, u64)>) -> bool {
        if self.making.is_empty() {
            return false;
        }
        let Some((dev, ino)) = object() else { return false };
        self.making.iter().map(|&i| &self.all[i]).any(|row| row.inode.as_ref().is_some_and(|i: &Inode| i.dev == dev && i.ino == ino))
    }

    /// The rows at `rel` exactly.
    pub(super) fn at(&self, rel: &Path) -> impl Iterator<Item = &OutboxRow> + '_ {
        self.by_rel.get(rel).into_iter().flatten().map(|&i| &self.all[i])
    }

    /// The rows strictly below `dir`, in `seq` order.
    pub(super) fn under(&self, dir: &Path) -> Vec<&OutboxRow> {
        let mut found: Vec<usize> = self
            .by_rel
            .range::<Path, _>((std::ops::Bound::Excluded(dir), std::ops::Bound::Unbounded))
            .take_while(|(rel, _)| rel.starts_with(dir))
            .flat_map(|(_, list)| list.iter().copied())
            .collect();
        found.sort_unstable();
        found.into_iter().map(|i| &self.all[i]).collect()
    }

    /// The rows whose place is directly in `dir`, in `seq` order.
    pub(super) fn in_dir(&self, dir: &Path) -> Vec<&OutboxRow> {
        self.under(dir).into_iter().filter(|row| row.rel.parent() == Some(dir)).collect()
    }
}
