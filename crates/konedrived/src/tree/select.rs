//! Selective sync (issue #58): which folders of the drive are on this
//! computer.
//!
//! The selection — a list of chosen folders, by item id, and the root files'
//! switch — lives in `config.toml`; the store holds a copy in memory
//! ([`TreeStore::set_selection`]) and turns it into placements. With no
//! selection everything is synced, and nothing here runs.
//!
//! A folder above a chosen one is *partial*: it is on disk as a plain
//! directory that holds only its chosen sub-folders. The root is always
//! partial while a selection is set. A row whose parent is partial is
//! [`SkipReason::NotSelected`] when it is a folder that is neither chosen nor
//! partial itself, or a file (unless the parent is the root and the root's
//! files are on). Rows deeper inside such a folder keep their own placement,
//! as the descendants of any skipped folder do. Every other skip reason wins:
//! only a row that would be placed gets this one.
//!
//! One function applies that rule, [`pass`]: on what a cycle staged, before
//! the reconcile compares it with `items`; on `items`, for the rows a first
//! listing's page or an outbox commit wrote; and on the whole of `items` when
//! the selection changes. `classify` never does.
//!
//! On a read-write folder (design §3, §4) the store also:
//!
//! - says what a change of the selection would lose
//!   ([`TreeStore::selection_would_lose`]), so that the change is refused
//!   before anything is written;
//! - makes a folder committed by the outbox in a partial folder chosen, the
//!   list first, then the row ([`commit_written`]);
//! - forgets the local objects of what comes back into the folder
//!   ([`forget_local`]): they are from before it left this computer, and an
//!   examination that found them gone would delete the items in OneDrive.

use std::collections::HashSet;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use std::path::{Path, PathBuf};

use super::outbox::OutboxKind;
use super::{Kind, Placement, Row, SkipReason, Source, Table, TreeError, TreeStore, COLUMNS, MAX_CHAIN, UNTOUCHED};

/// [`SkipReason::NotSelected`] as `items.placement` has it.
pub(super) const NOT_SELECTED: &str = "skipped:not-selected";

/// The selection, as `[accounts.sync_only]` in `config.toml` keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// The chosen folders, by item id. May be empty.
    #[serde(default)]
    pub folders: Vec<String>,
    /// Whether the files directly in the root are synced.
    #[serde(default)]
    pub root_files: bool,
}

/// Told the selection whenever the store itself changes it: a chosen folder
/// was deleted, or now lies inside another chosen one, or a folder made on
/// this computer became chosen. It writes `config.toml`, and says why when it
/// could not. Called on the store's thread.
pub type SelectionSink = Arc<dyn Fn(&Selection) -> Result<(), String> + Send + Sync>;

/// One sub-folder, as `FolderChildren` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderChild {
    pub id: String,
    pub name: String,
    /// `chosen`, `inside` (in a chosen folder; every folder while there is
    /// no selection), `partial` or `none`.
    pub state: &'static str,
    pub has_subfolders: bool,
}

/// The store's copy of the selection.
#[derive(Default)]
pub(super) struct Selecting {
    selection: Option<Selection>,
    /// The chosen ids `items` has had since the selection was set: one of
    /// them that is gone was deleted, and leaves the list. One the store
    /// never knew stays.
    known: HashSet<String>,
    sink: Option<SelectionSink>,
}

/// What the selection makes of a tree.
struct Shape {
    /// The chosen folders the tree has, less those inside another.
    chosen: HashSet<String>,
    /// The partial folders, the root included.
    partial: HashSet<String>,
}

/// `id`'s parent, and whether it is a folder.
fn brief(conn: &Connection, source: Source, id: &str) -> Result<Option<(Option<String>, bool)>, TreeError> {
    let sql = format!("SELECT parent_id, kind FROM {} WHERE id = ?1", source.rows());
    Ok(conn.prepare_cached(&sql)?.query_row([id], |r| Ok((r.get(0)?, r.get::<_, String>(1)? == "folder"))).optional()?)
}

/// The folders above `id`, nearest first, as far as the tree has them.
fn ancestors(conn: &Connection, source: Source, id: &str) -> Result<Vec<String>, TreeError> {
    let mut out: Vec<String> = Vec::new();
    let mut at = id.to_owned();
    while out.len() < MAX_CHAIN {
        match brief(conn, source, &at)? {
            Some((Some(parent), _)) => {
                out.push(parent.clone());
                at = parent;
            }
            _ => break,
        }
    }
    Ok(out)
}

fn shape(conn: &Connection, source: Source, root: &str, selection: &Selection) -> Result<Shape, TreeError> {
    let listed: HashSet<&str> = selection.folders.iter().map(String::as_str).collect();
    if listed.contains(root) {
        // The root chosen is the whole drive: nothing is partial.
        return Ok(Shape { chosen: HashSet::from([root.to_owned()]), partial: HashSet::new() });
    }
    let mut shape = Shape { chosen: HashSet::new(), partial: HashSet::from([root.to_owned()]) };
    for id in &listed {
        if !matches!(brief(conn, source, id)?, Some((_, true))) {
            continue;
        }
        let above = ancestors(conn, source, id)?;
        if above.iter().any(|a| listed.contains(a.as_str())) {
            continue;
        }
        shape.chosen.insert((*id).to_owned());
        shape.partial.extend(above);
    }
    Ok(shape)
}

/// The rule, of a row `p`: `?1` is the root, `?2` the root files' switch.
const RULE: &str = "p.parent_id IN (SELECT id FROM temp.sel_partial)
    AND ((p.kind = 'folder' AND p.id NOT IN (SELECT id FROM temp.sel_keep))
      OR (p.kind != 'folder' AND NOT (?2 AND p.parent_id = ?1)))";

/// Applies the selection to the tree `source` — to row `only` alone, when
/// given — and returns the ids whose placement it changed. Over `items` (a
/// delta staged), a row of `items` that changes is staged first, so that the
/// reconcile sees it change. Does nothing with no selection, or before the
/// drive's root is known.
///
/// `forget`, over `items` alone: what the pass brings back into the folder —
/// a row it places again, with everything below it — has no local object on
/// record from then on ([`forget_local`]). Whatever was recorded is from
/// before it left, and gone: an examination would take it for deleted here.
pub(super) fn pass(conn: &Connection, source: Source, selecting: &Selecting, only: Option<&str>, forget: bool) -> Result<Vec<String>, TreeError> {
    let Some(selection) = &selecting.selection else { return Ok(Vec::new()) };
    let root: Option<Option<String>> = conn.query_row("SELECT value FROM meta WHERE key = 'root_item_id'", [], |r| r.get(0)).optional()?;
    let Some(root) = root.flatten() else { return Ok(Vec::new()) };
    let shape = shape(conn, source, &root, selection)?;
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS sel_partial (id TEXT PRIMARY KEY);
         CREATE TEMP TABLE IF NOT EXISTS sel_keep (id TEXT PRIMARY KEY);
         DELETE FROM temp.sel_partial;
         DELETE FROM temp.sel_keep;",
    )?;
    {
        let mut partial = conn.prepare_cached("INSERT OR IGNORE INTO temp.sel_partial (id) VALUES (?1)")?;
        let mut keep = conn.prepare_cached("INSERT OR IGNORE INTO temp.sel_keep (id) VALUES (?1)")?;
        for id in &shape.partial {
            partial.execute([id])?;
            keep.execute([id])?;
        }
        for id in &shape.chosen {
            keep.execute([id])?;
        }
    }
    let untouched = format!(" AND {UNTOUCHED}");
    let tables: &[(&str, &str)] = match source {
        Source::Items => &[("items", "")],
        Source::Whole => &[("staging", "")],
        Source::Overlay => &[("staging", ""), ("items", untouched.as_str())],
    };
    let one = if only.is_some() { " AND p.id = ?3" } else { "" };
    let mut flips: Vec<(String, &str)> = Vec::new();
    for (table, rest) in tables {
        let leaves = format!("SELECT p.id FROM {table} p WHERE p.placement = 'placed' AND p.id != ?1 AND {RULE}{rest}{one}");
        let comes = format!(
            "SELECT p.id FROM {table} p
              WHERE p.placement != 'placed' AND p.placement = '{NOT_SELECTED}' AND NOT COALESCE(({RULE}), 0){rest}{one}"
        );
        for (sql, to) in [(leaves, NOT_SELECTED), (comes, "placed")] {
            let mut statement = conn.prepare_cached(&sql)?;
            let ids: Vec<String> = match only {
                Some(only) => statement.query_map(rusqlite::params![root, selection.root_files, only], |r| r.get(0))?.collect::<Result<_, _>>()?,
                None => statement.query_map(rusqlite::params![root, selection.root_files], |r| r.get(0))?.collect::<Result<_, _>>()?,
            };
            flips.extend(ids.into_iter().map(|id| (id, to)));
        }
    }
    let target = if source == Source::Items { "items" } else { "staging" };
    let mut stage = conn.prepare_cached(&format!("INSERT OR IGNORE INTO staging ({COLUMNS}) SELECT {COLUMNS} FROM items WHERE id = ?1"))?;
    let mut write = conn.prepare_cached(&format!("UPDATE {target} SET placement = ?2 WHERE id = ?1"))?;
    for (id, to) in &flips {
        if source == Source::Overlay {
            stage.execute([id])?;
        }
        write.execute([id.as_str(), to])?;
        if forget && source == Source::Items && *to == "placed" {
            forget_local(conn, id)?;
        }
    }
    Ok(flips.into_iter().map(|(id, _)| id).collect())
}

/// Forgets the local object of `id` and of everything `items` has inside
/// it, in both tables: until the reconcile places them again, no examination
/// can prove them deleted, so none of them becomes a delete in OneDrive.
pub(crate) fn forget_local(conn: &Connection, id: &str) -> Result<(), TreeError> {
    for table in ["items", "staging"] {
        conn.execute(
            &format!(
                "WITH RECURSIVE below(id, depth) AS (
                     SELECT ?1, 0
                     UNION ALL
                     SELECT c.id, b.depth + 1 FROM items c JOIN below b ON c.parent_id = b.id WHERE b.depth < {MAX_CHAIN})
                 UPDATE {table} SET local_handle = NULL WHERE local_handle IS NOT NULL AND id IN (SELECT id FROM below)"
            ),
            [id],
        )?;
    }
    Ok(())
}

/// Whether `items` has `id` in the folder: itself and every folder above it
/// placed, up to the root.
pub(super) fn in_view(conn: &Connection, id: &str) -> Result<bool, TreeError> {
    let sql = format!(
        "WITH RECURSIVE chain(parent_id, placement, depth) AS (
             SELECT parent_id, placement, 0 FROM items WHERE id = ?1
             UNION ALL
             SELECT p.parent_id, p.placement, c.depth + 1 FROM chain c JOIN items p ON p.id = c.parent_id WHERE c.depth < {MAX_CHAIN})
         SELECT COALESCE(SUM(parent_id IS NULL), 0), COALESCE(SUM(placement != 'placed' AND parent_id IS NOT NULL), 0) FROM chain"
    );
    let (roots, out): (i64, i64) = conn.prepare_cached(&sql)?.query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(roots == 1 && out == 0)
}

/// Before an outbox commit writes the folder `row` into `items` (issue #58,
/// design §4): a folder made on this computer in a partial folder — or moved
/// there, or adopted there — becomes chosen. The list goes to `config.toml`
/// first (the sink), then the caller commits the row: a crash between the
/// two never leaves a folder the next pass would remove. Whether it was
/// added.
pub(super) fn choose_made_here(conn: &Connection, selecting: &mut Selecting, row: &Row) -> Result<bool, TreeError> {
    let Some(selection) = &selecting.selection else { return Ok(false) };
    if row.kind != Kind::Folder {
        return Ok(false);
    }
    let Some(parent) = row.parent_id.as_deref() else { return Ok(false) };
    let root: Option<Option<String>> = conn.query_row("SELECT value FROM meta WHERE key = 'root_item_id'", [], |r| r.get(0)).optional()?;
    let Some(root) = root.flatten() else { return Ok(false) };
    let shape = shape(conn, Source::Items, &root, selection)?;
    if !shape.partial.contains(parent) || shape.chosen.contains(&row.id) || shape.partial.contains(&row.id) {
        return Ok(false);
    }
    let mut chosen = selection.clone();
    if !chosen.folders.contains(&row.id) {
        chosen.folders.push(row.id.clone());
    }
    if let Some(sink) = &selecting.sink {
        sink(&chosen).map_err(|why| TreeError::Io(std::io::Error::other(format!("{} cannot become a chosen folder: {why}", row.name))))?;
    }
    selecting.known.insert(row.id.clone());
    selecting.selection = Some(chosen);
    Ok(true)
}

/// After an outbox commit wrote `row` into `items`: the selection applied to
/// it — and, for a folder, to all of `items`, since a folder that moved
/// changes which folders are partial.
pub(super) fn after_write(conn: &Connection, selecting: &Selecting, row: &Row) -> Result<(), TreeError> {
    let only = (row.kind == Kind::File).then_some(row.id.as_str());
    pass(conn, Source::Items, selecting, only, true).map(|_| ())
}

/// An outbox commit of `row` into `items`, with the selection (design §4):
/// the row is written; a folder becomes chosen first where it would be
/// left out ([`choose_made_here`]), the selection is applied to what was
/// written ([`after_write`]), and a folder the commit brings back into the
/// folder — adopted, or moved in from where it was left out — has nothing
/// recorded below it any more ([`forget_local`]).
pub(super) fn commit_written(conn: &rusqlite::Transaction<'_>, selecting: &mut Selecting, row: &Row) -> Result<(), TreeError> {
    if selecting.selection.is_none() {
        super::upsert(conn, Table::Items, row)?;
        return Ok(());
    }
    let returns = row.kind == Kind::Folder && brief(conn, Source::Items, &row.id)?.is_some() && !in_view(conn, &row.id)?;
    choose_made_here(conn, selecting, row)?;
    super::upsert(conn, Table::Items, row)?;
    after_write(conn, selecting, row)?;
    if returns && in_view(conn, &row.id)? {
        forget_local(conn, &row.id)?;
    }
    Ok(())
}

impl TreeStore {
    /// The selection the store applies; `None` while everything is synced.
    pub fn selection(&self) -> Option<&Selection> {
        self.select.selection.as_ref()
    }

    /// The selection from now on, applied to the whole of `items` in one
    /// transaction: how many placements changed. `sink` hears of every
    /// change the store makes to the list by itself.
    pub fn set_selection(&mut self, selection: Option<Selection>, sink: Option<SelectionSink>) -> Result<usize, TreeError> {
        self.select = Selecting { selection, known: HashSet::new(), sink };
        let tx = self.conn.transaction()?;
        let changed = match &self.select.selection {
            // Through the index of skipped items. What comes back has no
            // local object on record, as in [`pass`].
            None => {
                let back: Vec<String> = {
                    let mut statement = tx.prepare(&format!("SELECT id FROM items WHERE placement != 'placed' AND placement = '{NOT_SELECTED}'"))?;
                    let ids = statement.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
                    ids
                };
                for id in &back {
                    tx.execute("UPDATE items SET placement = 'placed' WHERE id = ?1", [id])?;
                    forget_local(&tx, id)?;
                }
                back.len()
            }
            Some(_) => pass(&tx, Source::Items, &self.select, None, true)?.len(),
        };
        tx.commit()?;
        self.settle_selection()?;
        Ok(changed)
    }

    /// The selection applied to what a cycle staged, before the reconcile
    /// compares it with `items`: the ids whose placement it changed.
    pub fn select_staged(&mut self) -> Result<Vec<String>, TreeError> {
        if self.select.selection.is_none() {
            return Ok(Vec::new());
        }
        let source = self.source(Table::Staging);
        let tx = self.conn.transaction()?;
        let changed = pass(&tx, source, &self.select, None, false)?;
        tx.commit()?;
        self.forget_returning()?;
        Ok(changed)
    }

    /// What the new tree brings back into the folder — `items` has it left
    /// out, by the selection or below a folder it leaves out, and the new
    /// tree places it: the pass placed it again, or OneDrive moved it into a
    /// chosen folder — has no local object on record from now on, nor has
    /// anything below it ([`forget_local`]). Whatever was recorded is from
    /// before it left this computer: the reconcile places it again, and no
    /// examination takes it for deleted here. Only a row whose folder or
    /// placement the new tree changes can come back.
    fn forget_returning(&mut self) -> Result<(), TreeError> {
        let moved: Vec<String> = {
            let mut statement = self.conn.prepare_cached(
                "SELECT s.id FROM staging s JOIN items i ON i.id = s.id
                  WHERE s.parent_id IS NOT i.parent_id OR s.placement IS NOT i.placement",
            )?;
            let ids = statement.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            ids
        };
        let mut back: Vec<String> = Vec::new();
        for id in moved {
            if in_view(&self.conn, &id)? || !self.locate(Table::Staging, &id)?.is_some_and(|l| l.placed) {
                continue;
            }
            back.extend(self.descendants(Table::Staging, &id)?);
            back.push(id);
        }
        if back.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        for table in ["items", "staging"] {
            let mut forget = tx.prepare_cached(&format!("UPDATE {table} SET local_handle = NULL WHERE id = ?1 AND local_handle IS NOT NULL"))?;
            for id in &back {
                forget.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Whether the selection keeps `id` off this computer in `table`'s tree:
    /// the tree has it, and it — or a folder above it — is not selected. Such
    /// an item was not removed from OneDrive.
    pub fn left_out(&self, table: Table, id: &str) -> Result<bool, TreeError> {
        let source = self.source(table);
        let sql = format!(
            "WITH RECURSIVE chain(parent_id, placement, depth) AS (
                 SELECT parent_id, placement, 0 FROM {rows} WHERE id = ?1
                 UNION ALL
                 {step})
             SELECT EXISTS (SELECT 1 FROM chain WHERE placement = '{NOT_SELECTED}')",
            rows = source.rows(),
            step = source.step("p.parent_id, p.placement, c.depth + 1", "chain", "p.id = c.parent_id", &format!("c.depth < {MAX_CHAIN}"))
        );
        Ok(self.conn.prepare_cached(&sql)?.query_row([id], |r| r.get(0))?)
    }

    /// The folders whose own files are not synced, by item id: the partial
    /// folders, the root among them unless its files are on. `None` while
    /// everything is synced. A new file made in one of them on this computer
    /// is not uploaded (design §4).
    pub fn folders_without_files(&self) -> Result<Option<HashSet<String>>, TreeError> {
        let Some(selection) = &self.select.selection else { return Ok(None) };
        let Some(root) = self.root_item_id()? else { return Ok(None) };
        let mut partial = shape(&self.conn, Source::Items, &root, selection)?.partial;
        if selection.root_files {
            partial.remove(&root);
        }
        Ok(Some(partial))
    }

    /// The deepest folder of `items` on the way to `dir` (relative to the
    /// root), by name: the folder a local object in `dir` is in, as far as
    /// OneDrive has it — and whether that folder is `dir` itself, rather than
    /// a folder above a directory new to OneDrive.
    fn folder_towards(&self, root: &str, dir: &Path) -> Result<(String, bool), TreeError> {
        let mut child = self.conn.prepare_cached("SELECT id FROM items WHERE parent_id = ?1 AND name = ?2 AND kind = 'folder'")?;
        let mut at = root.to_owned();
        for part in dir.components() {
            let name = part.as_os_str().to_string_lossy();
            match child.query_row([at.as_str(), name.as_ref()], |r| r.get::<_, String>(0)).optional()? {
                Some(next) => at = next,
                None => return Ok((at, false)),
            }
        }
        Ok((at, true))
    }

    /// What exists only on this computer and would go, or would never be
    /// uploaded, if the selection became `selection` (design §3): each with
    /// its path relative to the root, and why. Empty when the change loses
    /// nothing. Two things are looked for, in the store alone, so that it
    /// also works while no sync runs:
    ///
    /// - an outbox row — a change waiting to be uploaded — on an item that
    ///   would leave, in a folder that would leave, or of a new file in a
    ///   folder whose files would stop being synced;
    /// - a kept-back object (`local_skipped`) in a directory that would
    ///   leave. One in a directory that stays — a folder that becomes
    ///   partial — blocks nothing.
    ///
    /// Nothing is changed: the selection is applied in a transaction that is
    /// rolled back.
    pub fn selection_would_lose(&mut self, selection: &Selection) -> Result<Vec<(PathBuf, String)>, TreeError> {
        let Some(root) = self.root_item_id()? else { return Ok(Vec::new()) };
        // (path, why, the item or folder it stands or falls with, whether it
        // is a new file, which needs its folder's files synced).
        let mut found: Vec<(PathBuf, String, String, bool)> = Vec::new();
        for row in self.outbox_rows()? {
            let why = format!("its change ({}) is waiting to be uploaded", row.kind.as_str());
            let dir = row.rel.parent().unwrap_or(Path::new("")).to_path_buf();
            let known = match &row.item_id {
                Some(id) => brief(&self.conn, Source::Items, id)?.map(|_| id.clone()),
                None => None,
            };
            match known {
                Some(id) => found.push((row.rel, why, id, false)),
                None => {
                    // A new file in a directory new to OneDrive goes up
                    // with it: that directory becomes chosen.
                    let (folder, directly) = match row.target_parent.as_ref().filter(|p| matches!(brief(&self.conn, Source::Items, p), Ok(Some(_)))) {
                        Some(parent) => (parent.clone(), true),
                        None => self.folder_towards(&root, &dir)?,
                    };
                    found.push((row.rel, why, folder, directly && row.kind == OutboxKind::Create));
                }
            }
        }
        for kept in self.local_skipped()? {
            let dir = kept.rel.parent().unwrap_or(Path::new("")).to_path_buf();
            let (folder, _) = self.folder_towards(&root, &dir)?;
            found.push((kept.rel, format!("it is never uploaded ({})", kept.reason), folder, false));
        }
        if found.is_empty() {
            return Ok(Vec::new());
        }
        let stands = |conn: &Connection, selecting: &Selecting| -> Result<Vec<bool>, TreeError> {
            let partial = match &selecting.selection {
                Some(s) => {
                    let mut partial = shape(conn, Source::Items, &root, s)?.partial;
                    if s.root_files {
                        partial.remove(&root);
                    }
                    partial
                }
                None => HashSet::new(),
            };
            found.iter().map(|(_, _, id, new_file)| Ok(in_view(conn, id)? && !(*new_file && partial.contains(id)))).collect()
        };
        let before = stands(&self.conn, &self.select)?;
        let wanted = Selecting { selection: Some(selection.clone()), known: HashSet::new(), sink: None };
        let tx = self.conn.unchecked_transaction()?;
        pass(&tx, Source::Items, &wanted, None, false)?;
        let after = stands(&tx, &wanted)?;
        tx.rollback()?;
        Ok(found
            .iter()
            .zip(before.iter().zip(&after))
            .filter(|(_, (before, after))| **before && !**after)
            .map(|((rel, why, _, _), _)| (rel.clone(), why.clone()))
            .collect())
    }

    /// Keeps the list normal after `items` changed: an id whose item was
    /// deleted leaves it, and so does a folder that now lies inside another
    /// chosen one. A list that loses its last id stays an empty list. An id
    /// `items` never had stays.
    pub(super) fn settle_selection(&mut self) -> Result<(), TreeError> {
        let Some(selection) = self.select.selection.clone() else { return Ok(()) };
        let listed: HashSet<&str> = selection.folders.iter().map(String::as_str).collect();
        let mut kept: Vec<String> = Vec::new();
        for id in &selection.folders {
            if kept.contains(id) {
                continue;
            }
            match brief(&self.conn, Source::Items, id)? {
                None if self.select.known.remove(id) => {}
                None => kept.push(id.clone()),
                Some(_) => {
                    self.select.known.insert(id.clone());
                    if !ancestors(&self.conn, Source::Items, id)?.iter().any(|a| listed.contains(a.as_str())) {
                        kept.push(id.clone());
                    }
                }
            }
        }
        if kept != selection.folders {
            let settled = Selection { folders: kept, root_files: selection.root_files };
            self.select.selection = Some(settled.clone());
            if let Some(sink) = &self.select.sink {
                // Said by the sink already; the store's copy is the one applied.
                let _ = sink(&settled);
            }
        }
        Ok(())
    }

    /// `ids` as a list to set: no id twice, and no folder inside another of
    /// them. `Err` says why one cannot be chosen: it is not in the list
    /// already and not in the store, is not a folder, or is, or lies inside,
    /// a folder skipped for another reason.
    pub fn check_selection(&self, ids: &[String]) -> Result<Result<Vec<String>, String>, TreeError> {
        let root = self.root_item_id()?;
        let current: HashSet<&str> = self.select.selection.iter().flat_map(|s| s.folders.iter().map(String::as_str)).collect();
        let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut out: Vec<String> = Vec::new();
        for id in ids {
            if out.contains(id) {
                continue;
            }
            let Some(row) = self.get(Table::Items, id)? else {
                if current.contains(id.as_str()) {
                    out.push(id.clone());
                    continue;
                }
                return Ok(Err(format!("{id} is not a folder of this OneDrive")));
            };
            if root.as_deref() == Some(id.as_str()) {
                return Ok(Err("the root is not chosen as a folder: sync everything instead".into()));
            }
            if row.kind != Kind::Folder {
                return Ok(Err(format!("{} is a file; only folders are chosen", row.name)));
            }
            let above = ancestors(&self.conn, Source::Items, id)?;
            let mut lineage = vec![row];
            for a in &above {
                lineage.extend(self.get(Table::Items, a)?);
            }
            if let Some((skipped, reason)) = lineage.iter().find_map(|r| match r.placement {
                Placement::Skipped(reason) if reason != SkipReason::NotSelected => Some((r, reason)),
                _ => None,
            }) {
                return Ok(Err(format!("{} cannot be in the folder ({})", skipped.name, reason.as_str())));
            }
            if !above.iter().any(|a| wanted.contains(a.as_str())) {
                out.push(id.clone());
            }
        }
        Ok(Ok(out))
    }

    /// The chosen folders: item id and path in OneDrive, relative to the
    /// root; an empty path for an id the store does not know.
    pub fn selected_folders(&self) -> Result<Vec<(String, String)>, TreeError> {
        let Some(selection) = &self.select.selection else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for id in &selection.folders {
            let rel = self.locate(Table::Items, id)?.map(|l| l.rel.display().to_string()).unwrap_or_default();
            out.push((id.clone(), rel));
        }
        Ok(out)
    }

    /// The sub-folders of folder `id` (`""`: the root), by name, each with
    /// its state and whether it has sub-folders. Folders skipped for another
    /// reason are left out. Read from `items`: also what is not on disk.
    pub fn folder_children(&self, id: &str) -> Result<Vec<FolderChild>, TreeError> {
        let Some(root) = self.root_item_id()? else { return Ok(Vec::new()) };
        let parent = if id.is_empty() { root.as_str() } else { id };
        let shape = match &self.select.selection {
            Some(selection) => Some(shape(&self.conn, Source::Items, &root, selection)?),
            None => None,
        };
        let covered = match &shape {
            None => true,
            Some(shape) => shape.chosen.contains(parent) || ancestors(&self.conn, Source::Items, parent)?.iter().any(|a| shape.chosen.contains(a)),
        };
        let mut has = self.conn.prepare_cached(&format!(
            "SELECT EXISTS (SELECT 1 FROM items WHERE parent_id = ?1 AND kind = 'folder' AND (placement = 'placed' OR placement = '{NOT_SELECTED}'))"
        ))?;
        let mut out = Vec::new();
        for row in self.children(Table::Items, parent)? {
            if row.kind != Kind::Folder || !matches!(row.placement, Placement::Placed | Placement::Skipped(SkipReason::NotSelected)) {
                continue;
            }
            let state = match &shape {
                _ if covered => "inside",
                Some(shape) if shape.chosen.contains(&row.id) => "chosen",
                Some(shape) if shape.partial.contains(&row.id) => "partial",
                _ => "none",
            };
            let has_subfolders = has.query_row([&row.id], |r| r.get(0))?;
            out.push(FolderChild { id: row.id, name: row.name, state, has_subfolders });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::tree::Change;

    fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> Row {
        Row {
            id: id.into(),
            parent_id: parent.map(str::to_owned),
            name: name.into(),
            kind,
            size: 0,
            mtime: 0,
            etag: None,
            ctag: Some(format!("c-{id}")),
            quickxor: None,
            mime: None,
            placement: Placement::Placed,
        }
    }

    fn folder(id: &str, parent: &str, name: &str) -> Change {
        Change::Upsert(row(id, Some(parent), name, Kind::Folder))
    }

    fn file(id: &str, parent: &str, name: &str) -> Change {
        Change::Upsert(row(id, Some(parent), name, Kind::File))
    }

    /// `R/{A/{B/{X/x.txt, b.txt}, C/c.txt, a.txt}, D/d.txt, V (the vault), r.txt}`.
    fn drive() -> TreeStore {
        let mut s = TreeStore::in_memory().unwrap();
        s.begin_staging(false).unwrap();
        s.stage(&[
            Change::Root(row("R", None, "", Kind::Folder)),
            folder("A", "R", "A"),
            folder("B", "A", "B"),
            folder("X", "B", "X"),
            file("x", "X", "x.txt"),
            file("b", "B", "b.txt"),
            folder("C", "A", "C"),
            file("c", "C", "c.txt"),
            file("a", "A", "a.txt"),
            folder("D", "R", "D"),
            file("d", "D", "d.txt"),
            Change::Upsert(Row { placement: Placement::Skipped(SkipReason::PersonalVault), ..row("V", Some("R"), "Vault", Kind::Folder) }),
            file("r", "R", "r.txt"),
        ])
        .unwrap();
        s.commit_staging("L1").unwrap();
        s
    }

    fn only(folders: &[&str], root_files: bool) -> Option<Selection> {
        Some(Selection { folders: folders.iter().map(|f| (*f).to_owned()).collect(), root_files })
    }

    fn placement(s: &TreeStore, table: Table, id: &str) -> Placement {
        s.get(table, id).unwrap().unwrap().placement
    }

    /// The ids of `items` the selection leaves out, sorted.
    fn left_out(s: &TreeStore) -> Vec<String> {
        let mut statement = s.conn.prepare(&format!("SELECT id FROM items WHERE placement = '{NOT_SELECTED}' ORDER BY id")).unwrap();
        let ids = statement.query_map([], |r| r.get(0)).unwrap().collect::<Result<Vec<String>, _>>().unwrap();
        ids
    }

    /// Every case of the rule: a chosen folder and what is inside it stay; the
    /// folders above it are partial, and stay without their files; a folder
    /// that is not chosen goes, and what is inside it keeps its own
    /// placement; another skip reason wins.
    #[test]
    fn the_rule_places_the_chosen_folders_and_the_folders_above_them() {
        let mut s = drive();
        s.set_selection(only(&["X"], false), None).unwrap();
        assert_eq!(left_out(&s), ["C", "D", "a", "b", "r"]);
        for id in ["A", "B", "X", "x", "c", "d"] {
            assert_eq!(placement(&s, Table::Items, id), Placement::Placed, "{id}");
        }
        assert_eq!(placement(&s, Table::Items, "V"), Placement::Skipped(SkipReason::PersonalVault), "another reason wins");
        assert!(s.locate(Table::Items, "x").unwrap().unwrap().placed, "inside the chosen folder");
        assert!(!s.locate(Table::Items, "c").unwrap().unwrap().placed, "inside a folder that is not chosen");
        assert!(!s.locate(Table::Items, "d").unwrap().unwrap().placed);
        // A, B, X and x.txt.
        let counts = s.counts().unwrap();
        assert_eq!((counts.listed, counts.placed, counts.skipped), (12, 4, 1), "only the vault counts as skipped");
        assert_eq!(s.skipped().unwrap(), vec![(PathBuf::from("Vault"), SkipReason::PersonalVault)]);
    }

    /// The root's own files follow their switch; the files of any other
    /// partial folder never come.
    #[test]
    fn the_roots_files_follow_their_switch() {
        let mut s = drive();
        s.set_selection(only(&["X"], true), None).unwrap();
        assert_eq!(left_out(&s), ["C", "D", "a", "b"]);
        s.set_selection(only(&["X"], false), None).unwrap();
        assert_eq!(left_out(&s), ["C", "D", "a", "b", "r"]);
        // An empty list with the switch off: nothing at all.
        s.set_selection(only(&[], false), None).unwrap();
        assert_eq!(left_out(&s), ["A", "D", "r"]);
        assert_eq!(s.counts().unwrap().placed, 0);
        // No selection: everything, as before.
        assert_eq!(s.set_selection(None, None).unwrap(), 3);
        assert!(left_out(&s).is_empty());
    }

    /// A delta for a folder that is not chosen — `classify` says placed —
    /// never turns it back into a placed one; and a delta that changes
    /// nothing of the selection flips nothing.
    #[test]
    fn a_staged_delta_for_a_folder_that_is_not_chosen_keeps_it_out() {
        let mut s = drive();
        s.set_selection(only(&["X"], false), None).unwrap();
        s.begin_staging(true).unwrap();
        s.stage(&[folder("D", "R", "D renamed"), file("n", "R", "new.txt"), file("y", "X", "y.txt")]).unwrap();
        s.select_staged().unwrap();
        assert_eq!(placement(&s, Table::Staging, "D"), Placement::Skipped(SkipReason::NotSelected));
        assert_eq!(placement(&s, Table::Staging, "n"), Placement::Skipped(SkipReason::NotSelected), "a new file in the root");
        assert_eq!(placement(&s, Table::Staging, "y"), Placement::Placed, "a new file in the chosen folder");
        s.commit_staging("L2").unwrap();
        assert_eq!(left_out(&s), ["C", "D", "a", "b", "n", "r"]);
    }

    /// A chosen folder moved in OneDrive changes which folders are partial,
    /// though only its own row arrives: the others are staged by the pass,
    /// and so are part of what the reconcile sees change.
    #[test]
    fn a_chosen_folder_moved_in_onedrive_changes_which_folders_are_partial() {
        let mut s = drive();
        s.set_selection(only(&["X"], false), None).unwrap();
        s.begin_staging(true).unwrap();
        s.stage(&[folder("X", "D", "X")]).unwrap();
        let mut flipped = s.select_staged().unwrap();
        flipped.sort();
        // D is partial now, so its file goes; A no longer is, and what is
        // inside it keeps its own placement again.
        assert_eq!(flipped, ["A", "C", "D", "a", "b", "d"]);
        assert_eq!(placement(&s, Table::Staging, "D"), Placement::Placed);
        assert_eq!(placement(&s, Table::Staging, "A"), Placement::Skipped(SkipReason::NotSelected));
        assert_eq!(placement(&s, Table::Staging, "d"), Placement::Skipped(SkipReason::NotSelected), "a file in a partial folder");
        let mut changed = s.changed_ids().unwrap();
        changed.sort();
        assert_eq!(changed, ["A", "C", "D", "X", "a", "b", "d"]);
        assert_eq!(placement(&s, Table::Items, "A"), Placement::Placed, "`items` is as it was until the swap");
        s.commit_staging("L2").unwrap();
        assert_eq!(left_out(&s), ["A", "d", "r"]);
        assert_eq!(s.selection(), only(&["X"], false).as_ref(), "kept by id");
    }

    /// A full listing staged whole gets the selection too.
    #[test]
    fn a_whole_listing_is_staged_with_the_selection() {
        let mut s = drive();
        s.set_selection(only(&["D"], true), None).unwrap();
        s.begin_staging(false).unwrap();
        s.stage(&[Change::Root(row("R", None, "", Kind::Folder)), folder("A", "R", "A"), folder("D", "R", "D"), file("d", "D", "d.txt"), file("r", "R", "r.txt")]).unwrap();
        assert_eq!(s.select_staged().unwrap(), ["A"]);
        s.commit_staging("L2").unwrap();
        assert_eq!(left_out(&s), ["A"]);
    }

    /// A page of a first listing goes into `items` with the selection applied.
    #[test]
    fn a_page_of_a_first_listing_is_committed_with_the_selection() {
        let mut s = TreeStore::in_memory().unwrap();
        s.set_selection(only(&[], false), None).unwrap();
        s.begin_placing().unwrap();
        s.commit_page(&[Change::Root(row("R", None, "", Kind::Folder)), folder("A", "R", "A"), file("a", "A", "a.txt"), file("r", "R", "r.txt")], "next").unwrap();
        assert_eq!(left_out(&s), ["A", "r"]);
        assert_eq!(s.counts().unwrap().placed, 0);
    }

    /// A chosen folder deleted in OneDrive leaves the list, and the last one
    /// leaves an empty list; an id the store never knew stays; a folder that
    /// lies inside another chosen one leaves.
    #[test]
    fn the_list_is_kept_normal_and_loses_what_is_deleted() {
        let mut s = drive();
        let heard: Arc<Mutex<Vec<Selection>>> = Arc::default();
        let sink: SelectionSink = {
            let heard = Arc::clone(&heard);
            Arc::new(move |selection| {
                heard.lock().unwrap().push(selection.clone());
                Ok(())
            })
        };
        s.set_selection(only(&["A", "X", "unknown", "D"], false), Some(sink)).unwrap();
        assert_eq!(s.selection(), only(&["A", "unknown", "D"], false).as_ref(), "X lies inside A");
        assert_eq!(heard.lock().unwrap().len(), 1);

        s.begin_staging(true).unwrap();
        s.stage(&[Change::Delete("A".into()), Change::Delete("D".into())]).unwrap();
        s.select_staged().unwrap();
        s.commit_staging("L2").unwrap();
        assert_eq!(s.selection(), only(&["unknown"], false).as_ref());
        assert_eq!(heard.lock().unwrap().last(), only(&["unknown"], false).as_ref());
        assert_eq!(left_out(&s), ["r"], "an empty list is not everything");
    }

    /// What can be chosen, and the list made normal.
    #[test]
    fn only_folders_that_can_be_placed_are_chosen() {
        let mut s = drive();
        assert_eq!(s.check_selection(&["X".into(), "A".into(), "X".into(), "D".into()]).unwrap(), Ok(vec!["A".to_owned(), "D".to_owned()]));
        assert!(s.check_selection(&["nope".into()]).unwrap().is_err());
        assert!(s.check_selection(&["x".into()]).unwrap().is_err(), "a file");
        assert!(s.check_selection(&["V".into()]).unwrap().is_err(), "skipped for another reason");
        assert!(s.check_selection(&["R".into()]).unwrap().is_err(), "the root");
        s.set_selection(only(&["later"], false), None).unwrap();
        assert_eq!(s.check_selection(&["later".into(), "C".into()]).unwrap(), Ok(vec!["later".to_owned(), "C".to_owned()]), "in the list already; C is left out, and can be chosen");
    }

    /// `FolderChildren` and `SelectedFolders`, as the store answers them.
    #[test]
    fn the_folders_are_listed_with_their_state() {
        let mut s = drive();
        let states = |s: &TreeStore, id: &str| -> Vec<(String, &'static str, bool)> {
            s.folder_children(id).unwrap().into_iter().map(|c| (c.name, c.state, c.has_subfolders)).collect()
        };
        assert_eq!(states(&s, ""), [("A".to_owned(), "inside", true), ("D".to_owned(), "inside", false)], "no selection; the vault is left out");
        assert!(s.selected_folders().unwrap().is_empty());
        s.set_selection(only(&["X", "later"], false), None).unwrap();
        assert_eq!(states(&s, ""), [("A".to_owned(), "partial", true), ("D".to_owned(), "none", false)]);
        assert_eq!(states(&s, "A"), [("B".to_owned(), "partial", true), ("C".to_owned(), "none", false)]);
        assert_eq!(states(&s, "B"), [("X".to_owned(), "chosen", false)]);
        s.set_selection(only(&["A"], false), None).unwrap();
        assert_eq!(states(&s, "B"), [("X".to_owned(), "inside", false)]);
        assert_eq!(s.selected_folders().unwrap(), [("A".to_owned(), "A".to_owned())]);
        s.set_selection(only(&["X", "later"], false), None).unwrap();
        assert_eq!(s.selected_folders().unwrap(), [("X".to_owned(), "A/B/X".to_owned()), ("later".to_owned(), String::new())]);
    }

    use crate::tree::outbox::{Base, Committed, Detection, Inode, OutboxKind, OutboxOp, OutboxState, Recorded};
    use konedrive_fs::handle::FileHandle;

    fn handle(n: u8) -> FileHandle {
        FileHandle { kind: 1, bytes: vec![n] }
    }

    /// A live outbox row of `kind` at `rel`: of item `id`, or of something
    /// new in the folder `parent`. Its `seq`.
    fn waiting(s: &mut TreeStore, kind: OutboxKind, id: Option<&str>, rel: &str, parent: Option<&str>) -> i64 {
        let base = id.and_then(|id| s.get(Table::Items, id).unwrap()).map(|r| Base { etag: r.etag, ctag: r.ctag, parent: r.parent_id, name: Some(r.name) });
        let rel = PathBuf::from(rel);
        let detection = Detection {
            kind,
            item_id: id.map(str::to_owned),
            inode: Some(Inode { dev: 1, ino: 1000 + s.outbox_rows().unwrap().len() as u64, handle: None }),
            target_name: rel.file_name().map(|n| n.to_string_lossy().into_owned()),
            rel,
            base,
            target_parent: parent.map(str::to_owned),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: None,
        };
        match s.outbox_record(&detection).unwrap() {
            Recorded::Inserted(seq) | Recorded::Merged(seq) => seq,
            other => panic!("{other:?}"),
        }
    }

    fn kept_back(s: &mut TreeStore, rel: &str, reason: &str) {
        s.outbox_apply(&[OutboxOp::Skip { rel: rel.into(), reason: reason.into(), size: 0 }], 5).unwrap();
    }

    fn lost(s: &mut TreeStore, folders: &[&str], root_files: bool) -> Vec<String> {
        let wanted = only(folders, root_files).unwrap();
        s.selection_would_lose(&wanted).unwrap().into_iter().map(|(rel, _)| rel.display().to_string()).collect()
    }

    /// Design §3: a change of the selection is refused for what exists only
    /// here and would go — an outbox row on an item that leaves, in a folder
    /// that leaves, or of a new file where files stop being synced, and a
    /// kept-back object in a directory that leaves. A kept-back object in a
    /// folder that becomes partial, and a new directory there, block
    /// nothing. The check changes nothing.
    #[test]
    fn a_change_that_would_lose_what_exists_only_here_is_found_and_changes_nothing() {
        let mut s = drive();
        assert!(lost(&mut s, &["X"], false).is_empty(), "nothing waits, nothing is kept back");

        waiting(&mut s, OutboxKind::Update, Some("d"), "D/d.txt", Some("D"));
        assert_eq!(lost(&mut s, &["A"], true), ["D/d.txt"], "a row in a folder that leaves");
        assert!(lost(&mut s, &["D"], false).is_empty(), "its folder stays");

        waiting(&mut s, OutboxKind::Update, Some("a"), "A/a.txt", Some("A"));
        assert_eq!(lost(&mut s, &["X", "D"], false), ["A/a.txt"], "a row on a file of a folder that becomes partial");
        assert!(lost(&mut s, &["A", "D"], false).is_empty());

        kept_back(&mut s, "A/C/link", "symlink");
        kept_back(&mut s, "A/own-link", "symlink");
        assert_eq!(lost(&mut s, &["X", "D"], false), ["A/a.txt", "A/C/link"], "a kept-back object in a folder that leaves; the one in A stays");
        assert_eq!(lost(&mut s, &["D"], false), ["A/a.txt", "A/C/link", "A/own-link"], "A leaves whole");

        // What is new here: a file where files stop being synced is lost to
        // OneDrive; a directory goes up and becomes chosen.
        waiting(&mut s, OutboxKind::Create, None, "D/new.txt", Some("D"));
        waiting(&mut s, OutboxKind::Mkdir, None, "A/B/made", Some("B"));
        waiting(&mut s, OutboxKind::Create, None, "A/B/made/in.txt", None);
        assert_eq!(lost(&mut s, &["A"], true), ["D/d.txt", "D/new.txt"]);
        assert_eq!(lost(&mut s, &["X", "D"], true), ["A/a.txt", "A/C/link"], "the new directory in B, partial now, blocks nothing");
        waiting(&mut s, OutboxKind::Create, None, "root.txt", Some("R"));
        assert!(lost(&mut s, &["A", "D"], false).contains(&"root.txt".to_owned()), "a new file in the root, its files off");
        assert!(!lost(&mut s, &["A", "D"], true).contains(&"root.txt".to_owned()));

        assert_eq!(s.selection(), None, "nothing was set");
        assert!(left_out(&s).is_empty(), "and nothing placed otherwise");
        let why = s.selection_would_lose(&only(&["D"], false).unwrap()).unwrap();
        assert!(why.iter().any(|(rel, why)| rel == Path::new("A/C/link") && why.contains("symlink")), "{why:?}");
        assert!(why.iter().any(|(rel, why)| rel == Path::new("A/a.txt") && why.contains("update")), "{why:?}");
    }

    /// What comes back into the folder has no local object on record: the
    /// one recorded is from before it left this computer, and an examination
    /// would take the item for deleted here. By a change of the selection,
    /// and by a move in OneDrive into a chosen folder.
    #[test]
    fn what_comes_back_has_no_local_object_on_record() {
        let mut s = drive();
        for (n, id) in ["A", "B", "X", "x", "C", "c", "D", "d", "r"].iter().enumerate() {
            s.set_local_handle(id, Some(&handle(n as u8))).unwrap();
        }
        s.set_selection(only(&["X"], false), None).unwrap();
        assert!(s.local_handle("d").unwrap().is_some(), "left out: whatever it had stays on record");
        s.set_selection(only(&["X", "D"], true), None).unwrap();
        for id in ["D", "d", "r"] {
            assert_eq!(s.local_handle(id).unwrap(), None, "{id} is placed again");
        }
        assert!(s.local_handle("x").unwrap().is_some() && s.local_handle("A").unwrap().is_some(), "what stayed keeps its object");
        assert!(s.local_handle("c").unwrap().is_some(), "C is still left out");

        // OneDrive moves c.txt from C, which is left out, into the chosen X.
        s.begin_staging(true).unwrap();
        s.stage(&[file("c", "X", "c.txt")]).unwrap();
        s.select_staged().unwrap();
        assert_eq!(s.local_handle("c").unwrap(), None);
        assert!(s.unplaced(Table::Staging).unwrap().contains(&"c".to_owned()), "so it is placed");
        s.commit_staging("L2").unwrap();
        assert_eq!(s.local_handle("c").unwrap(), None);
    }

    /// Design §4: a folder made on this computer in a partial folder becomes
    /// chosen as its row is committed, `config.toml` first; a folder kept
    /// here that adopts the one in OneDrive too, and what the base has below
    /// it is placed again rather than taken for deleted. A file does not.
    #[test]
    fn a_folder_committed_in_a_partial_folder_becomes_chosen() {
        let mut s = drive();
        let heard: Arc<Mutex<Vec<Selection>>> = Arc::default();
        let refuse: Arc<Mutex<bool>> = Arc::default();
        let sink: SelectionSink = {
            let (heard, refuse) = (Arc::clone(&heard), Arc::clone(&refuse));
            Arc::new(move |selection| {
                if *refuse.lock().unwrap() {
                    return Err("the disk is full".into());
                }
                heard.lock().unwrap().push(selection.clone());
                Ok(())
            })
        };
        s.set_local_handle("d", Some(&handle(9))).unwrap();
        s.set_selection(only(&["X"], false), Some(sink)).unwrap();

        // A new folder in A, which is partial.
        let seq = waiting(&mut s, OutboxKind::Mkdir, None, "A/made", Some("A"));
        s.outbox_commit(seq, Committed::Item { row: &row("N", Some("A"), "made", Kind::Folder), handle: Some(&handle(1)) }, None).unwrap();
        assert_eq!(s.selection(), only(&["X", "N"], false).as_ref());
        assert_eq!(heard.lock().unwrap().last(), only(&["X", "N"], false).as_ref(), "written to config.toml");
        assert_eq!(placement(&s, Table::Items, "N"), Placement::Placed);

        // A new folder inside a chosen one is in already; a new file in a
        // partial folder is left out.
        let seq = waiting(&mut s, OutboxKind::Mkdir, None, "A/B/X/sub", Some("X"));
        s.outbox_commit(seq, Committed::Item { row: &row("S", Some("X"), "sub", Kind::Folder), handle: None }, None).unwrap();
        let seq = waiting(&mut s, OutboxKind::Create, None, "A/n.txt", Some("A"));
        s.outbox_commit(seq, Committed::Item { row: &row("n", Some("A"), "n.txt", Kind::File), handle: None }, None).unwrap();
        assert_eq!(s.selection(), only(&["X", "N"], false).as_ref());
        assert_eq!(placement(&s, Table::Items, "n"), Placement::Skipped(SkipReason::NotSelected));

        // The list cannot be written: the row is not committed.
        *refuse.lock().unwrap() = true;
        let seq = waiting(&mut s, OutboxKind::Mkdir, None, "A/other", Some("A"));
        assert!(s.outbox_commit(seq, Committed::Item { row: &row("O", Some("A"), "other", Kind::Folder), handle: None }, None).is_err());
        assert!(s.get(Table::Items, "O").unwrap().is_none() && s.outbox_row(seq).unwrap().is_some());
        *refuse.lock().unwrap() = false;

        // D is left out; a directory here adopts it (its `mkdir` met it).
        let seq = waiting(&mut s, OutboxKind::Mkdir, None, "D", Some("R"));
        s.outbox_commit(seq, Committed::Item { row: &row("D", Some("R"), "D", Kind::Folder), handle: Some(&handle(2)) }, None).unwrap();
        assert_eq!(s.selection(), only(&["X", "N", "D"], false).as_ref());
        assert_eq!(placement(&s, Table::Items, "D"), Placement::Placed);
        assert_eq!(s.local_handle("D").unwrap(), Some(handle(2)), "the directory that adopted it");
        assert_eq!(s.local_handle("d").unwrap(), None, "what was in it is placed again, not deleted");
    }

    /// With many items left out, what has no local object is found from the
    /// root down: the same answer as from each item up.
    #[test]
    fn what_has_no_local_object_is_found_from_the_root_too() {
        let mut s = drive();
        s.set_selection(only(&["X"], true), None).unwrap();
        s.set_local_handle("B", Some(&handle(1))).unwrap();
        let sorted = |mut ids: Vec<String>| {
            ids.sort();
            ids
        };
        assert_eq!(sorted(s.unplaced(Table::Items).unwrap()), ["A", "X", "r", "x"]);
        assert_eq!(sorted(s.unplaced_from_the_root(Table::Items).unwrap()), ["A", "X", "r", "x"]);
        s.begin_staging(true).unwrap();
        s.stage(&[file("y", "X", "y.txt"), file("e", "D", "e.txt"), Change::Delete("x".into())]).unwrap();
        s.select_staged().unwrap();
        assert_eq!(sorted(s.unplaced(Table::Staging).unwrap()), ["A", "X", "r", "y"]);
        assert_eq!(sorted(s.unplaced_from_the_root(Table::Staging).unwrap()), ["A", "X", "r", "y"]);
    }
}
