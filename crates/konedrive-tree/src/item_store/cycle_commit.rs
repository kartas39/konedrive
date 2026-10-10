//! What a cycle's commit does to the rows of `items`, inside the commit's transaction, once
//! `onedrive_items` is what the cycle made it ([`follow_onedrive_items`]): the swap of a
//! staged tree and a page of a first listing call it, and nothing else does.
//!
//! It is a rule over all rows, asked in SQL of `onedrive_items` as it is now and never of
//! the ids one cycle changed: a directory that becomes placed shows what was listed in it
//! before, and rows that a commit before left behind are caught up with here.
//!
//! **What `onedrive_items` places** is found by one walk down from the drive's root: a row
//! whose own placement is placed, in a directory the walk reached, under a name a directory
//! entry can have. The walk gives each item with its parent, so that a place is written with
//! the parent's local item id.
//!
//! **The rule**, for the drive the store is a listing of:
//!
//! - the root of the drive is row 1, before any parent is looked up
//!   ([`bind_root`](super::bindings::bind_root));
//! - every item `onedrive_items` places has a bound row. A row is made with the binding and
//!   the kind alone, so every parent has its row before a place names it;
//! - where a row's synced place and content are its `onedrive_items` row already, nothing
//!   of the row is written: its local version is the folder's own. That is what the outbox
//!   holds (a read-write cycle keeps its row in `onedrive_items` and lets OneDrive's change
//!   wait), and what the user changed here that is not sent;
//! - where they are not, OneDrive changed the item, or the row is new: the local version
//!   becomes the `onedrive_items` row, and then the synced place and content do, from the
//!   same values. A row the folder does not show while something is agreed on for it was
//!   removed here: it takes the synced place and content, and stays not shown;
//! - a row takes a place in the folder only when the place is free. One that another row
//!   holds and keeps (an item of the user's own that could not be moved aside; the file of
//!   a create that is not committed yet; an item the outbox holds at a name OneDrive gave
//!   to another) is not taken: the row that wanted it is left wholly as it was, not shown if
//!   it is new, and the next commit asks again. So is a row whose directory the folder does
//!   not show, and one whose place would put it below itself;
//! - rows that exchange or rotate names step aside first, all of them in one statement, to
//!   a name no file can have, and then take their places in one statement: the index of
//!   one name per directory holds at each;
//! - `content_presence` of a file that is shown anew, moved, or given another content by
//!   OneDrive is what the cycle step found on disk (`copies_on_disk`): a whole copy marked
//!   with the item's id at its place. A file that is at its place with nothing agreed on
//!   yet (a file made here whose create was confirmed) keeps what it has: its bytes are
//!   the ones that were sent;
//! - a bound row that `onedrive_items` places no more and the folder does not show goes,
//!   deepest first: only a row that no row names as its parent. A row the folder shows is
//!   never deleted here: the cycle step removed it from the folder first, or gave its
//!   binding up.
//!
//! Nothing is done before the store knows the drive and its root: no row can be bound.
//!
//! **The order of the statements** is the order the schema's rules ask for. Rows are made
//! first and with no place, so the triggers of a parent's row have nothing to refuse. The
//! rows that move are found with their old places still in the table, and those that cannot
//! move are found before any steps aside, as the least set that holds every row whose
//! place is kept by a row that stays. Local places are written before synced ones, because
//! "OneDrive changed it" is read from the synced columns. Rows are deleted last, when no
//! row that moved out of a directory names it any more.

use rusqlite::{params, Transaction};

use super::bindings::bind_root;
use crate::meta::{self, DRIVE_ID, ROOT_ITEM_ID};
use crate::model::{placed, DriveId, FILE};
use crate::{TreeError, MAX_CHAIN};

/// The columns of a content, after the word that says whose it is, each with what the walk
/// gives for it.
const CONTENT: [(&str, &str); 6] = [("size", "t.size"), ("mtime", "t.mtime"), ("mtime_nsec", "t.mtime_nsec"), ("hash", "t.hash"), ("ctag", "t.ctag"), ("change_count", "NULL")];

/// SQL: the kind of the row `o` of `onedrive_items` as `items` says a kind, and each column
/// of its content, which a directory has none of.
fn onedrive_columns() -> String {
    let file_only = |column: &str| format!("CASE o.kind WHEN 'folder' THEN NULL ELSE {column} END");
    format!("CASE o.kind WHEN 'folder' THEN 'directory' ELSE '{FILE}' END, {}, {}, {}, {}, {}", file_only("o.size"), file_only("o.mtime"), file_only("0"), file_only("o.quickxor"), file_only("o.ctag"))
}

/// SQL: the items `onedrive_items` places, as `placed`: the walk down from the drive's
/// root, whose id is `?2`.
fn placed_sql() -> String {
    format!(
        "placed(id, parent_id, name, kind, size, mtime, mtime_nsec, hash, ctag, depth) AS (
             SELECT id, NULL, NULL, 'directory', NULL, NULL, NULL, NULL, NULL, 0 FROM onedrive_items WHERE id = ?2
             UNION ALL
             SELECT o.id, o.parent_id, o.name, {columns}, p.depth + 1
               FROM onedrive_items o JOIN placed p ON o.parent_id = p.id
              WHERE p.kind = 'directory' AND p.depth < {MAX_CHAIN} AND {own}
                AND o.id != '' AND o.name NOT IN ('', '.', '..') AND instr(o.name, '/') = 0 AND instr(o.name, char(0)) = 0)",
        columns = onedrive_columns(),
        own = placed("o.placement"),
    )
}

/// SQL: the placed items whose row does not follow them, by their OneDrive item ids: one
/// that has no bound row, and one whose bound row has not its place and content as what was
/// agreed on. The one statement that reads every placed item; `?1` is the drive.
fn rows_behind_sql() -> String {
    format!(
        "WITH RECURSIVE {placed}
         SELECT t.id FROM (
             SELECT p.id, d.local_item_id AS parent, p.name, p.size, p.mtime, p.mtime_nsec, p.hash, p.ctag
               FROM placed p LEFT JOIN items d ON d.onedrive_drive_id = ?1 AND d.onedrive_item_id = p.parent_id
              WHERE p.depth > 0) t
           LEFT JOIN items i ON i.onedrive_drive_id = ?1 AND i.onedrive_item_id = t.id
          WHERE i.local_item_id IS NULL OR {changed}",
        placed = placed_sql(),
        changed = onedrive_changed(),
    )
}

/// SQL: each of the items `?4` names (a JSON array of OneDrive item ids, which the walk
/// found placed) that has a bound row, with its parent's row, as `target`: what the row is
/// to say. `?1` is the drive. The joins are in the order they are read in: the array once,
/// and a row found by its key for each id in it.
fn targets_sql() -> String {
    format!(
        "target(item, id, parent, name, kind, size, mtime, mtime_nsec, hash, ctag) AS MATERIALIZED (
             SELECT i.local_item_id, o.id, d.local_item_id, o.name, {columns}
               FROM json_each(?4) behind
              CROSS JOIN onedrive_items o ON o.id = behind.value
              CROSS JOIN items i ON i.onedrive_drive_id = ?1 AND i.onedrive_item_id = o.id
              CROSS JOIN items d ON d.onedrive_drive_id = ?1 AND d.onedrive_item_id = o.parent_id
              WHERE i.kind = CASE o.kind WHEN 'folder' THEN 'directory' ELSE '{FILE}' END AND d.kind = 'directory' AND i.local_item_id != 1)",
        columns = onedrive_columns(),
    )
}

/// SQL: the content `whose` of the row `i` is not the content of its target `t`.
fn content_differs(whose: &str) -> String {
    CONTENT.iter().map(|(column, value)| format!("i.{whose}_{column} IS NOT {value}")).collect::<Vec<_>>().join(" OR ")
}

/// SQL: the content `whose` of a row is set to the content of its target `t`.
fn content_set(whose: &str) -> String {
    CONTENT.iter().map(|(column, value)| format!("{whose}_{column} = {value}")).collect::<Vec<_>>().join(", ")
}

/// SQL: OneDrive changed the item of the row `i`, or the row is new: what was agreed on is
/// not its target `t`.
fn onedrive_changed() -> String {
    format!("(i.synced_parent_id IS NOT t.parent OR i.synced_name IS NOT t.name OR {})", content_differs("synced"))
}

/// SQL: the row `i` is at the place of its target `t` in the folder.
const AT_ITS_PLACE: &str = "(i.local_parent_id IS t.parent AND i.local_name IS t.name)";

/// SQL: the row `i` was removed here: the folder does not show it, and something is agreed
/// on for it.
const REMOVED_HERE: &str = "(i.local_parent_id IS NULL AND i.synced_parent_id IS NOT NULL)";

/// SQL: the name a row steps aside to, the one
/// [`set_local_place_aside`](super::item_row::set_local_place_aside) gives.
const ASIDE_NAME: &str = "unhex('002F' || hex(CAST(local_item_id AS TEXT)))";

/// SQL: the row `items` has stepped aside.
const IS_ASIDE: &str = "(typeof(items.local_name) = 'blob' AND substr(items.local_name, 1, 2) = x'002F')";

/// SQL: what a file's `content_presence` is by what the cycle step found on disk; `?3` is
/// the ids that have a whole copy there, as a JSON array.
const PRESENCE_ON_DISK: &str = "CASE WHEN t.kind = 'file' AND t.id IN (SELECT value FROM json_each(?3)) THEN 'on-disk' ELSE 'not-on-disk' END";

/// SQL: the rows that are to move in the folder, as `mover`, with the place each has and
/// the place it is to take; and those of them that cannot, as `blocked`: the least set that
/// holds a row whose place is held by a row that does not move, a row that is not the first
/// to want its place, one whose directory the folder does not show and will not, one that
/// would be placed below itself, and then every row whose place a blocked row keeps, and
/// every row whose directory is a blocked row the folder does not show.
fn movers_sql() -> String {
    format!(
        "mover(item, from_parent, from_name, to_parent, to_name) AS MATERIALIZED (
             SELECT i.local_item_id, i.local_parent_id, i.local_name, t.parent, t.name
               FROM target t JOIN items i ON i.local_item_id = t.item
              WHERE {changed} AND NOT {REMOVED_HERE} AND NOT {AT_ITS_PLACE}),
         first_mover(to_parent, to_name, item) AS MATERIALIZED (
             SELECT to_parent, to_name, min(item) FROM mover GROUP BY to_parent, to_name),
         blocked(item) AS (
             SELECT m.item FROM mover m JOIN items h ON h.local_parent_id = m.to_parent AND h.local_name = m.to_name
              WHERE h.local_item_id NOT IN (SELECT item FROM mover)
             UNION
             SELECT m.item FROM mover m JOIN first_mover f ON f.to_parent = m.to_parent AND f.to_name = m.to_name
              WHERE f.item != m.item
             UNION
             SELECT m.item FROM mover m JOIN items d ON d.local_item_id = m.to_parent
              WHERE d.local_item_id != 1 AND d.local_parent_id IS NULL AND d.local_item_id NOT IN (SELECT item FROM mover)
             UNION
             SELECT m.item FROM mover m
              WHERE m.from_parent IS NOT NULL AND EXISTS (
                    WITH RECURSIVE above(item, depth) AS (
                        SELECT m.to_parent, 0
                        UNION ALL
                        SELECT a.local_parent_id, above.depth + 1 FROM items a JOIN above ON a.local_item_id = above.item
                         WHERE a.local_parent_id IS NOT NULL AND above.depth < {MAX_CHAIN})
                    SELECT 1 FROM above WHERE item = m.item)
             UNION
             SELECT m.item FROM blocked b JOIN mover kept ON kept.item = b.item
               JOIN mover m ON m.to_parent = kept.from_parent AND m.to_name = kept.from_name
             UNION
             SELECT m.item FROM blocked b JOIN mover kept ON kept.item = b.item
               JOIN mover m ON m.to_parent = kept.item
              WHERE kept.from_parent IS NULL)",
        changed = onedrive_changed(),
    )
}

/// `ids` as a JSON array of texts, which a statement reads with `json_each`.
fn json_array(ids: &[String]) -> String {
    let mut json = String::from("[");
    for (n, id) in ids.iter().enumerate() {
        if n > 0 {
            json.push(',');
        }
        json.push('"');
        for c in id.chars() {
            match c {
                '"' => json.push_str("\\\""),
                '\\' => json.push_str("\\\\"),
                c if (c as u32) < 0x20 => json.push_str(&format!("\\u{:04x}", c as u32)),
                c => json.push(c),
            }
        }
        json.push('"');
    }
    json.push(']');
    json
}

/// The rows of `items` follow `onedrive_items` as the commit of a cycle just made it: the
/// rule of this module. `copies_on_disk` is what the cycle step found in the backing
/// directory once it had done what the cycle asks: the OneDrive item ids of the files whose
/// whole copy, marked with the id, is at the place the cycle has the file at.
///
/// Inside the commit's transaction: on any error the caller drops it and never commits it.
/// A commit that is dropped and staged again finds the rows as they were, and one that is
/// done twice changes nothing the second time.
pub(crate) fn follow_onedrive_items(tx: &Transaction<'_>, copies_on_disk: &[String]) -> Result<(), TreeError> {
    let (Some(drive), Some(root)) = (meta::get(tx, DRIVE_ID)?.and_then(DriveId::new), meta::get(tx, ROOT_ITEM_ID)?.filter(|id| !id.is_empty())) else {
        return Ok(());
    };
    bind_root(tx)?;
    let drive = drive.as_str();
    // The one reading of every placed item: those whose row does not follow.
    let behind: Vec<String> = tx.prepare_cached(&rows_behind_sql())?.query_map(params![drive, root], |row| row.get(0))?.collect::<Result<_, _>>()?;
    if !behind.is_empty() {
        write_rows_behind(tx, drive, &json_array(copies_on_disk), &json_array(&behind))?;
    }

    // What OneDrive places no more and the folder does not show goes, deepest first. The
    // placed items are read again only when such a row is there.
    let mut delete = tx.prepare_cached(&format!(
        "WITH RECURSIVE {placed}
         DELETE FROM items
          WHERE local_parent_id IS NULL AND onedrive_drive_id = ?1 AND local_item_id != 1
            AND NOT EXISTS (SELECT 1 FROM items c WHERE c.local_parent_id = items.local_item_id)
            AND NOT EXISTS (SELECT 1 FROM items c WHERE c.synced_parent_id = items.local_item_id)
            AND onedrive_item_id NOT IN (SELECT id FROM placed)",
        placed = placed_sql(),
    ))?;
    for _ in 0..=MAX_CHAIN {
        if delete.execute(params![drive, root])? == 0 {
            break;
        }
    }
    Ok(())
}

/// The rows of the placed items `behind` (a JSON array of OneDrive item ids) are made to
/// follow them, where they can: the statements of the module's rule, in its order.
fn write_rows_behind(tx: &Transaction<'_>, drive: &str, copies: &str, behind: &str) -> Result<(), TreeError> {
    let targets = targets_sql();
    let movers = movers_sql();
    let changed = onedrive_changed();
    // `?2` and `?3` are named in every statement, so that each has the same parameters.
    let named = "?2 IS ?2 AND ?3 IS ?3";

    // Every placed item has a bound row: the binding and the kind, and no place yet.
    tx.execute(
        &format!(
            "INSERT INTO items (onedrive_drive_id, onedrive_item_id, kind)
             SELECT ?1, o.id, CASE o.kind WHEN 'folder' THEN 'directory' ELSE '{FILE}' END
               FROM json_each(?4) behind CROSS JOIN onedrive_items o ON o.id = behind.value
              WHERE {named} AND NOT EXISTS (SELECT 1 FROM items i WHERE i.onedrive_drive_id = ?1 AND i.onedrive_item_id = o.id)"
        ),
        params![drive, "", copies, behind],
    )?;

    // The rows the folder shows that move, and can, step aside: all in one statement.
    tx.execute(
        &format!(
            "WITH RECURSIVE {targets}, {movers}
             UPDATE items SET local_name = {ASIDE_NAME}
              WHERE {named} AND local_parent_id IS NOT NULL
                AND local_item_id IN (SELECT item FROM mover) AND local_item_id NOT IN (SELECT item FROM blocked)"
        ),
        params![drive, "", copies, behind],
    )?;
    // And take their places, with the content OneDrive has: all in one statement.
    tx.execute(
        &format!(
            "WITH {targets}
             UPDATE items SET local_parent_id = t.parent, local_name = t.name, {content}, content_presence = {PRESENCE_ON_DISK}
               FROM target t WHERE {named} AND t.item = items.local_item_id AND {IS_ASIDE}",
            content = content_set("local"),
        ),
        params![drive, "", copies, behind],
    )?;

    // The rows the folder did not show take their places: a directory before what is in it,
    // one pass for each level that became shown, and the first of two that want one place.
    let mut show = tx.prepare_cached(&format!(
        "WITH {targets},
         unshown(item, parent, name) AS MATERIALIZED (
             SELECT t.item, t.parent, t.name FROM target t JOIN items i ON i.local_item_id = t.item
              WHERE i.local_parent_id IS NULL AND i.synced_parent_id IS NULL),
         first_unshown(parent, name, item) AS MATERIALIZED (SELECT parent, name, min(item) FROM unshown GROUP BY parent, name)
         UPDATE items SET local_parent_id = t.parent, local_name = t.name, {content}, content_presence = {PRESENCE_ON_DISK}
           FROM target t JOIN first_unshown f ON f.parent = t.parent AND f.name = t.name AND f.item = t.item
          WHERE {named} AND t.item = items.local_item_id
            AND (t.parent = 1 OR EXISTS (SELECT 1 FROM items d WHERE d.local_item_id = t.parent AND d.local_parent_id IS NOT NULL))
            AND NOT EXISTS (SELECT 1 FROM items h WHERE h.local_parent_id = t.parent AND h.local_name = t.name)",
        content = content_set("local"),
    ))?;
    for _ in 0..=MAX_CHAIN {
        if show.execute(params![drive, "", copies, behind])? == 0 {
            break;
        }
    }

    // A file that stays where it is and has another content in OneDrive now: the content,
    // and whether it is on disk. One with nothing agreed on yet keeps what it has.
    tx.execute(
        &format!(
            "WITH {targets}
             UPDATE items SET {content},
                    content_presence = CASE WHEN i.synced_parent_id IS NULL THEN i.content_presence ELSE {PRESENCE_ON_DISK} END
               FROM target t JOIN items i ON i.local_item_id = t.item
              WHERE {named} AND t.item = items.local_item_id AND t.kind = 'file' AND {changed} AND {AT_ITS_PLACE} AND ({local_differs})",
            content = content_set("local"),
            local_differs = content_differs("local"),
        ),
        params![drive, "", copies, behind],
    )?;

    // What the two sides agree on, for every row that is where OneDrive has its item, and
    // for one that was removed here.
    tx.execute(
        &format!(
            "WITH {targets}
             UPDATE items SET synced_parent_id = t.parent, synced_name = t.name, {content}
               FROM target t JOIN items i ON i.local_item_id = t.item
              WHERE {named} AND t.item = items.local_item_id AND {changed} AND ({AT_ITS_PLACE} OR {REMOVED_HERE})",
            content = content_set("synced"),
        ),
        params![drive, "", copies, behind],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
