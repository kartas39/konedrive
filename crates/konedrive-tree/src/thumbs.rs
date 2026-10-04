//! The thumbnails still to make, and what each cached one was made for.

use std::path::PathBuf;

use rusqlite::params;

use crate::model::{row_from, Row, FILE, PLACED, ROW_COLUMNS, ROW_WIDTH};
use crate::source::{chains_then, Source};
use crate::{TreeError, TreeStore};

/// Thumbnail candidates looked at by one query, and in one call
/// ([`TreeStore::thumbnail_candidates`]; guesses, issue #39).
pub const THUMB_PAGE: usize = 500;
pub const THUMB_SCAN: usize = 5000;

/// Thumbnails to make, with their paths, and the id to go on from
/// ([`TreeStore::thumbnail_candidates`]).
pub type ThumbnailBatch = (Vec<(Row, PathBuf)>, Option<String>);

impl TreeStore {
    /// Placed images and videos whose cached thumbnail was not made for what
    /// they are now (`thumb_key`, which `desktop::thumbs::thumb_key` writes: the
    /// cTag, the path and the time), with their paths: up to `limit` of them,
    /// looking at the candidates after id `after` in id order (issue #39),
    /// [`THUMB_PAGE`] at a time and at most [`THUMB_SCAN`] in one call. Each
    /// page is filtered and its paths found in one query. Also the id to go
    /// on from, `None` once the last candidate has been looked at.
    pub fn thumbnail_candidates(&self, after: &str, limit: usize) -> Result<ThumbnailBatch, TreeError> {
        if limit == 0 {
            return Ok((Vec::new(), Some(after.to_owned())));
        }
        let page = format!(
            "SELECT id, parent_id, name, placement FROM items
              WHERE kind = '{FILE}' AND placement = '{PLACED}' AND ctag IS NOT NULL
                AND (mime LIKE 'image/%' OR mime LIKE 'video/%') AND id > ?2
              ORDER BY id LIMIT ?3"
        );
        let Some(root) = self.root_item_id()? else { return Ok((Vec::new(), None)) };
        let wanted = format!(
            "SELECT {}, c.path FROM chain c JOIN items i ON i.id = c.start
              WHERE c.parent_id = ?1 AND c.above AND c.own
                AND (i.thumb_key IS NULL OR i.thumb_key != i.ctag || '|' || c.path || '|' || i.mtime)
              ORDER BY i.id",
            ROW_COLUMNS.split(", ").map(|c| format!("i.{c}")).collect::<Vec<_>>().join(", ")
        );
        let sql = chains_then(Source::Items, &page, &wanted);
        let mut out = Vec::new();
        let mut from = after.to_owned();
        let mut scanned = 0;
        loop {
            let (last, n): (Option<String>, usize) = self.conn.prepare_cached(&format!("SELECT max(id), count(*) FROM ({page})"))?.query_row(
                params![root, from, THUMB_PAGE as i64],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)),
            )?;
            let Some(last) = last else { return Ok((out, None)) };
            let mut statement = self.conn.prepare_cached(&sql)?;
            let found = statement.query_map(params![root, from, THUMB_PAGE as i64], |r| Ok((row_from(r)?, PathBuf::from(r.get::<_, String>(ROW_WIDTH)?))))?;
            for candidate in found {
                out.push(candidate?);
                if out.len() == limit {
                    // The next call looks at the rest of the page again.
                    let taken = out[limit - 1].0.id.clone();
                    return Ok((out, Some(taken)));
                }
            }
            scanned += n;
            if n < THUMB_PAGE {
                return Ok((out, None));
            }
            from = last;
            if scanned >= THUMB_SCAN {
                return Ok((out, Some(from)));
            }
        }
    }

    /// Records what a cached thumbnail of `id` was made for (`key`), so the
    /// next cycle does not make it again.
    pub fn set_thumb_key(&self, id: &str, key: &str) -> Result<(), TreeError> {
        self.conn.execute("UPDATE items SET thumb_key = ?2 WHERE id = ?1", params![id, key])?;
        Ok(())
    }
}
