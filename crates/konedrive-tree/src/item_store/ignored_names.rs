//! The flag `is_name_ignored` of the rows gone over once, when the list of ignored names
//! was changed.
//!
//! **State owned:** none of its own; it rewrites `is_name_ignored` and, through the
//! statements of `item_row` that keep it, `is_below_ignored_directory`, of rows that stay
//! where they are. Every other writer of the flag writes it with a place (design §3).
//!
//! **Answers for:** design §3, "when the list of ignored names is changed, the rows are
//! gone over once".
//!
//! **Tested without the rest:**
// test: an_unbound_item_whose_name_the_new_list_ignores_is_left_here_with_what_is_below_it
// test: an_unbound_item_whose_name_the_new_list_no_longer_ignores_is_unsent_again
// test: an_item_onedrive_has_keeps_being_sent_whatever_the_list_says_of_its_name
// test: the_flag_below_an_ignored_directory_equals_the_walk_up_after_the_go_over
use std::ffi::OsStr;

use rusqlite::Connection;

use super::item_row::set_local_place;
use super::ItemStore;
use crate::model::{ItemPlace, LocalItemId};
use crate::outbox::path_from;
use crate::TreeError;

/// How many rows one read of the go-over takes.
const ROWS_READ_AT_ONCE: i64 = 1000;

/// What the go-over reads of a row the folder shows: its place, and the flag it has.
struct NamedRow {
    item: LocalItemId,
    place: ItemPlace,
    is_name_ignored: bool,
}

/// The rows the folder shows, the root left out, after the item `after`, by local item id.
fn list_named_rows(conn: &Connection, after: i64) -> Result<Vec<NamedRow>, TreeError> {
    let mut statement = conn.prepare_cached("SELECT local_item_id, local_parent_id, local_name, is_name_ignored FROM items WHERE local_parent_id IS NOT NULL AND local_item_id > ?1 ORDER BY local_item_id LIMIT ?2")?;
    let rows = statement.query_map([after, ROWS_READ_AT_ONCE], |row| {
        let place = ItemPlace { parent_local_item_id: LocalItemId::new(row.get(1)?), name: path_from(row.get_ref(2)?).into_os_string() };
        Ok(NamedRow { item: LocalItemId::new(row.get(0)?), place, is_name_ignored: row.get::<_, i64>(3)? != 0 })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

impl ItemStore<'_> {
    /// Asks `is_name_ignored` of the local name of every item the folder shows and writes
    /// the flag where the answer is another than the row's, with the place the row has, as
    /// every writer of the flag does (`item_row::set_local_place`, which keeps
    /// `is_below_ignored_directory` of the row and of what is below it); how many rows had
    /// their own flag changed. One
    /// transaction, however many rows: a plan made meanwhile sees the old list or the new.
    ///
    /// Called by the change of the account's ignore list (`sync/settings.rs`), under the
    /// tree lock, so that no write operation gives a name its flag by the old list after
    /// it; the plan's task is woken after it. `is_name_ignored` is the upload rules' pure
    /// question of a name (`fuse::UploadRules::is_name_ignored`). MISSING-4.f.
    pub fn rewrite_ignored_names(&mut self, is_name_ignored: &dyn Fn(&OsStr) -> bool) -> Result<u64, TreeError> {
        self.write(|tx| {
            let mut changed = 0;
            let mut after = 0;
            // The rows the folder shows, a bounded number at a time, by local item id.
            loop {
                let rows = list_named_rows(tx, after)?;
                let Some(last) = rows.last() else { break };
                after = last.item.as_i64();
                for row in &rows {
                    let is_ignored = is_name_ignored(&row.place.name);
                    if is_ignored != row.is_name_ignored {
                        set_local_place(tx, row.item, &row.place, is_ignored)?;
                        changed += 1;
                    }
                }
            }
            Ok(changed)
        })
    }
}

#[cfg(test)]
mod tests;
