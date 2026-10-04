//! Where the rows of a tree are, and the queries that walk it.

use crate::model::{COLUMNS, PLACED};
use crate::MAX_CHAIN;

/// An `items` row `p` the delta laid over it leaves as it is.
pub(crate) const UNTOUCHED: &str =
    "NOT EXISTS (SELECT 1 FROM staging s WHERE s.id = p.id) AND NOT EXISTS (SELECT 1 FROM staging_gone g WHERE g.id = p.id)";

/// Where the rows of a tree are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// `items`.
    Items,
    /// `staging` alone: a full listing staged whole.
    Whole,
    /// `staging` over `items`, less `staging_gone`: a delta staged.
    Overlay,
}

impl Source {
    /// The tree as a table expression, for a query SQLite can push its
    /// `WHERE` into (a point or index lookup) — never for a join or a
    /// recursion, which would read it whole; see [`Source::step`].
    pub(crate) fn rows(self) -> String {
        match self {
            Source::Items => "items".into(),
            Source::Whole => "staging".into(),
            Source::Overlay => {
                format!("(SELECT {COLUMNS} FROM staging UNION ALL SELECT {COLUMNS} FROM items p WHERE {UNTOUCHED})")
            }
        }
    }

    /// One recursive step of a CTE over the tree: `select` reads `p`, the
    /// tree's row, and `c`, the CTE's, joined `on`, where `filter` holds.
    pub(crate) fn step(self, select: &str, cte: &str, on: &str, filter: &str) -> String {
        match self {
            Source::Items | Source::Whole => {
                let t = if self == Source::Items { "items" } else { "staging" };
                format!("SELECT {select} FROM {cte} c JOIN {t} p ON {on} WHERE {filter}")
            }
            Source::Overlay => format!(
                "SELECT {select} FROM {cte} c JOIN staging p ON {on} WHERE {filter}
                 UNION ALL
                 SELECT {select} FROM {cte} c JOIN items p ON {on} WHERE {filter} AND {UNTOUCHED}"
            ),
        }
    }
}

/// Everything below `?1` in the tree, as a query of ids.
pub(crate) fn below_sql(source: Source) -> String {
    format!(
        "WITH RECURSIVE below(id, depth) AS (
             SELECT id, 1 FROM {rows} WHERE parent_id = ?1
             UNION ALL
             {step})
         SELECT id FROM below",
        rows = source.rows(),
        step = source.step("p.id, c.depth + 1", "below", "p.parent_id = c.id", &format!("c.depth < {MAX_CHAIN}"))
    )
}

/// Where each item `start` selects is (`id, parent_id, name, placement` of
/// the tree's rows, with `?1` the drive's root): `(id, path, above,
/// own)` — the chain of names from the root, whether every folder above it
/// (below the root) is placed, and whether it is itself. An item whose
/// chain does not reach the root is left out. One query for the lot.
pub(crate) fn chains_sql(source: Source, start: &str) -> String {
    chains_then(source, start, "SELECT start, path, above, own FROM chain WHERE parent_id = ?1")
}

/// [`chains_sql`] with a query of its own over `chain(start, parent_id,
/// path, above, own)`: a row whose `parent_id` is the root (`?1`) has its
/// whole path.
pub(crate) fn chains_then(source: Source, start: &str, then: &str) -> String {
    format!(
        "WITH RECURSIVE chain(start, parent_id, path, above, own, depth) AS (
             SELECT id, parent_id, name, 1, placement = '{PLACED}', 0 FROM ({start}) WHERE id != ?1
             UNION ALL
             {step})
         {then}",
        step = source.step(
            &format!("c.start, p.parent_id, p.name || '/' || c.path, c.above AND p.placement = '{PLACED}', c.own, c.depth + 1"),
            "chain",
            "p.id = c.parent_id",
            &format!("c.depth < {MAX_CHAIN} AND c.parent_id != ?1")
        )
    )
}
