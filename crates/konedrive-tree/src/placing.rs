//! Whether the base places a row: the row itself, and every folder above it
//! up to the drive's root.

use std::collections::HashMap;

use rusqlite::OptionalExtension;

use crate::meta::{self, ROOT_ITEM_ID};
use crate::model::Placement;
use crate::{TreeError, MAX_CHAIN};

/// Whether the base places rows of `items`: the row itself and every folder
/// above it, up to the drive's root. A point lookup for each folder on the
/// way up, asked once however many rows share it.
pub(crate) struct Placing {
    known: HashMap<String, bool>,
}

impl Placing {
    /// `None` while the drive's root has not come: nothing is placed yet.
    pub(crate) fn new(tx: &rusqlite::Transaction<'_>) -> Result<Option<Self>, TreeError> {
        Ok(meta::get(tx, ROOT_ITEM_ID)?.map(|root| Self { known: HashMap::from([(root, true)]) }))
    }

    /// Whether the base places row `id`; `None` when `items` has no such row.
    pub(crate) fn of(&mut self, tx: &rusqlite::Transaction<'_>, id: &str) -> Result<Option<bool>, TreeError> {
        let mut row_of = tx.prepare_cached("SELECT parent_id, placement FROM items WHERE id = ?1")?;
        let mut chain: Vec<String> = Vec::new();
        let mut at = id.to_owned();
        let placed = loop {
            if let Some(&placed) = self.known.get(&at) {
                break placed;
            }
            let row: Option<(Option<String>, String)> = row_of.query_row([&at], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let Some((parent, placement)) = row else {
                if chain.is_empty() {
                    return Ok(None);
                }
                // A folder that never came places nothing.
                break false;
            };
            chain.push(at);
            match parent {
                Some(parent) if Placement::decode(&placement) == Placement::Placed && chain.len() <= MAX_CHAIN => at = parent,
                _ => break false,
            }
        };
        // Below a folder the base places, each row on the way was read as
        // placed itself; below any other, none is placed.
        self.known.extend(chain.into_iter().map(|id| (id, placed)));
        Ok(Some(placed))
    }
}

/// Whether the base places row `id`, as `items` is now.
pub(crate) fn base_places(tx: &rusqlite::Transaction<'_>, id: &str) -> Result<bool, TreeError> {
    let Some(mut placing) = Placing::new(tx)? else { return Ok(false) };
    Ok(placing.of(tx, id)?.unwrap_or(false))
}
