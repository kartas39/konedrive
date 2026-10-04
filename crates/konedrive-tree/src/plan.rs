//! The plan of a reconcile, item by item: what the base has of an item and
//! where, and what the new tree has of it and where. Read in one call for
//! many items, so that the reconcile decides from it without asking again.

use std::collections::HashMap;

use crate::model::{Located, Placement, Row, Table};
use crate::{TreeError, TreeStore};

/// An item as one tree has it: its row, and where the tree has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    pub row: Row,
    /// `None` when its chain of folders does not reach the drive's root.
    pub at: Option<Located>,
}

impl Side {
    /// Whether the tree places it: the item and every folder above it.
    pub fn placed(&self) -> bool {
        self.at.as_ref().is_some_and(|at| at.placed)
    }

    /// Where the tree places it; `None` when it does not.
    pub fn place(&self) -> Option<&Located> {
        self.at.as_ref().filter(|at| at.placed)
    }
}

/// What a reconcile is to do with one item: the base's side and the new
/// tree's. Both `None` for an id neither has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Planned {
    /// As `items` has it: the tree the folder was last made to match.
    pub base: Option<Side>,
    /// As the new tree has it.
    pub new: Option<Side>,
}

impl Planned {
    /// Where the base places it; `None` when it does not.
    pub fn base_place(&self) -> Option<&Located> {
        self.base.as_ref().and_then(Side::place)
    }

    /// Where the new tree places it; `None` when it does not.
    pub fn new_place(&self) -> Option<&Located> {
        self.new.as_ref().and_then(Side::place)
    }

    /// The new tree places it and the base does not: what is below it comes
    /// into view with it.
    pub fn comes_into_view(&self) -> bool {
        self.new_place().is_some() && self.base_place().is_none()
    }

    /// Both trees have it, the new one placed, in the same folder under the
    /// same name.
    pub fn stays(&self) -> bool {
        matches!((&self.base, &self.new), (Some(base), Some(new))
            if new.row.placement == Placement::Placed && new.row.parent_id == base.row.parent_id && new.row.name == base.row.name)
    }
}

/// The plan of the items it was asked for ([`TreeStore::plan`]).
#[derive(Debug, Default)]
pub struct Plan {
    items: HashMap<String, Planned>,
}

/// What the plan says of an id it was not asked for.
static UNKNOWN: Planned = Planned { base: None, new: None };

impl Plan {
    /// The plan of item `id`; of an id it was not asked for, that neither
    /// tree has it.
    pub fn of(&self, id: &str) -> &Planned {
        self.items.get(id).unwrap_or(&UNKNOWN)
    }

    /// The ids it was asked for.
    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.items.keys()
    }

    /// Adds the plan of more items.
    pub fn absorb(&mut self, more: Plan) {
        self.items.extend(more.items);
    }
}

impl TreeStore {
    /// The plan of every item of `ids`: its base row and place, its new row
    /// and place.
    pub fn plan(&self, ids: &[String]) -> Result<Plan, TreeError> {
        let mut items = HashMap::with_capacity(ids.len());
        for id in ids {
            if items.contains_key(id) {
                continue;
            }
            let planned = Planned { base: self.side(Table::Items, id)?, new: self.side(Table::Staging, id)? };
            items.insert(id.clone(), planned);
        }
        Ok(Plan { items })
    }

    fn side(&self, table: Table, id: &str) -> Result<Option<Side>, TreeError> {
        let Some(row) = self.get(table, id)? else { return Ok(None) };
        Ok(Some(Side { row, at: self.locate(table, id)? }))
    }
}

#[cfg(test)]
mod tests;
