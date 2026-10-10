//! "Always keep on this device", for a folder served as a FUSE mount: a file that is not
//! downloaded has no object to carry the pin, so the store keeps it.

use crate::{TreeError, TreeStore};

impl TreeStore {
    /// The items that carry a pin of their own.
    pub fn pins(&self) -> Result<Vec<String>, TreeError> {
        let mut statement = self.conn.prepare_cached("SELECT id FROM pins")?;
        let ids = statement.query_map([], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Puts the pin on `id`, or takes it off.
    pub fn set_pin(&self, id: &str, pinned: bool) -> Result<(), TreeError> {
        let sql = if pinned { "INSERT OR IGNORE INTO pins (id) VALUES (?1)" } else { "DELETE FROM pins WHERE id = ?1" };
        self.conn.prepare_cached(sql)?.execute([id])?;
        Ok(())
    }
}
