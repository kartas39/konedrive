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

use std::collections::HashSet;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

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
/// was deleted, or now lies inside another chosen one. It writes
/// `config.toml`. Called on the store's thread.
pub type SelectionSink = Arc<dyn Fn(&Selection) + Send + Sync>;

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
pub(super) fn pass(conn: &Connection, source: Source, selecting: &Selecting, only: Option<&str>) -> Result<Vec<String>, TreeError> {
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
    }
    Ok(flips.into_iter().map(|(id, _)| id).collect())
}

/// After an outbox commit wrote `row` into `items`: the selection applied to
/// it — and, for a folder, to all of `items`, since a folder that moved
/// changes which folders are partial.
pub(super) fn after_write(conn: &Connection, selecting: &Selecting, row: &Row) -> Result<(), TreeError> {
    let only = (row.kind == Kind::File).then_some(row.id.as_str());
    pass(conn, Source::Items, selecting, only).map(|_| ())
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
            // Through the index of skipped items.
            None => tx.execute(&format!("UPDATE items SET placement = 'placed' WHERE placement != 'placed' AND placement = '{NOT_SELECTED}'"), [])?,
            Some(_) => pass(&tx, Source::Items, &self.select, None)?.len(),
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
        let changed = pass(&tx, source, &self.select, None)?;
        tx.commit()?;
        Ok(changed)
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
                sink(&settled);
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
            Arc::new(move |selection| heard.lock().unwrap().push(selection.clone()))
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
}
