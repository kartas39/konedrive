//! An item's local object (`items.local_handle`).

use konedrive_fs::handle::FileHandle;
use rusqlite::{params, Connection, OptionalExtension};

use crate::forget::forget_subtrees;
use crate::model::{row_from, Row, Table, ROW_COLUMNS};
use crate::{TreeError, TreeStore};

pub(super) fn set_local_handle(conn: &Connection, id: &str, handle: Option<&FileHandle>) -> Result<(), TreeError> {
    let stored = handle.map(FileHandle::encode);
    for table in [Table::Items, Table::Staging] {
        conn.execute(&format!("UPDATE {} SET local_handle = ?2 WHERE id = ?1", table.name()), params![id, stored])?;
    }
    Ok(())
}

impl TreeStore {
    /// Records the inode item `id` is now: the placement's, a scan's
    /// refresh. In both tables, so that a cycle between staging and swap
    /// keeps it.
    pub fn set_local_handle(&self, id: &str, handle: Option<&FileHandle>) -> Result<(), TreeError> {
        set_local_handle(&self.conn, id, handle)
    }

    /// Records the inodes `placed` items are now, in one transaction (issue
    /// #39): a placement's batch.
    pub fn set_local_handles(&mut self, placed: &[(String, FileHandle)]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        {
            let mut items = tx.prepare_cached("UPDATE items SET local_handle = ?2 WHERE id = ?1")?;
            let mut staging = tx.prepare_cached("UPDATE staging SET local_handle = ?2 WHERE id = ?1")?;
            for (id, handle) in placed {
                let stored = handle.encode();
                items.execute(params![id, stored])?;
                staging.execute(params![id, stored])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every item forgets its local object, in both tables: the handles were
    /// taken on a filesystem the folder is no longer on.
    pub fn forget_local_handles(&self) -> Result<(), TreeError> {
        for table in [Table::Items, Table::Staging] {
            self.conn.execute(&format!("UPDATE {} SET local_handle = NULL", table.name()), [])?;
        }
        Ok(())
    }

    /// What the daemon is about to take off the disk itself (issue #104):
    /// the subtrees at `roots` — by `items` and by the new tree in `staging`
    /// — forget their local objects, in both tables, and so does every row
    /// that records one of `handles`, the objects themselves: one statement
    /// per table for the lot. Done before anything is removed, in one
    /// transaction: an examination
    /// that then misses one of them finds it unproven, never gone, and a row
    /// placed again later carries no object that is not there.
    pub fn forget_local_objects(&mut self, roots: &[String], handles: &[FileHandle]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        forget_subtrees(&tx, roots, true, handles)?;
        tx.commit()?;
        Ok(())
    }

    pub fn local_handle(&self, id: &str) -> Result<Option<FileHandle>, TreeError> {
        let stored: Option<Option<Vec<u8>>> =
            self.conn.query_row("SELECT local_handle FROM items WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(stored.flatten().as_deref().and_then(FileHandle::decode))
    }

    /// The base item whose local object has `handle`.
    pub fn item_by_handle(&self, handle: &FileHandle) -> Result<Option<Row>, TreeError> {
        let sql = format!("SELECT {ROW_COLUMNS} FROM items WHERE local_handle = ?1");
        Ok(self.conn.query_row(&sql, [handle.encode()], row_from).optional()?)
    }
}
