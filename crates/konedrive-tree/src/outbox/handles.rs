//! An item's local object (`items.local_handle`).

use konedrive_fs::handle::FileHandle;
use rusqlite::OptionalExtension;

use crate::{TreeError, TreeStore};

impl TreeStore {
    /// Records the inode item `id` is now, in both tables, so that a cycle
    /// between staging and swap keeps it: the tests' way to give an item a
    /// local object.
    #[cfg(test)]
    pub fn set_local_handle(&self, id: &str, handle: Option<&FileHandle>) -> Result<(), TreeError> {
        let stored = handle.map(FileHandle::encode);
        for table in [crate::model::Table::Items, crate::model::Table::Staging] {
            self.conn.execute(&format!("UPDATE {} SET local_handle = ?2 WHERE id = ?1", table.name()), rusqlite::params![id, stored])?;
        }
        Ok(())
    }

    pub fn local_handle(&self, id: &str) -> Result<Option<FileHandle>, TreeError> {
        let stored: Option<Option<Vec<u8>>> =
            self.conn.query_row("SELECT local_handle FROM items WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(stored.flatten().as_deref().and_then(FileHandle::decode))
    }
}
