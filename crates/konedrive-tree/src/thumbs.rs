//! The thumbnails still to make, and what each cached one was made for.

use std::path::PathBuf;

use rusqlite::params;

use crate::model::{placed, row_from, Row, FILE, ROW_COLUMNS, ROW_WIDTH};
use crate::source::{chains_then, Source};
use crate::{TreeError, TreeStore};

/// Thumbnail candidates looked at by one query, and in one call
/// ([`TreeStore::thumbnail_candidates`]; guesses).
pub const THUMB_PAGE: usize = 500;
pub const THUMB_SCAN: usize = 5000;

/// A file whose thumbnail is to be made: its row, and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub row: Row,
    pub rel: PathBuf,
}

/// What [`TreeStore::thumbnail_candidates`] found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThumbnailBatch {
    pub wanted: Vec<Thumbnail>,
    /// The id to go on from; `None` once the last candidate has been
    /// looked at.
    pub next: Option<String>,
}

/// SQL for what a thumbnail is made for (`items.thumb_key`): the version
/// `ctag`, the path `path` (its cache name) and the mtime `mtime` (which
/// KIO checks), each an SQL expression. Any of them changing needs a new
/// one. The one place the key is put together: the candidates are compared
/// with it and a made thumbnail is recorded with it.
fn thumb_key_sql(ctag: &str, path: &str, mtime: &str) -> String {
    format!("{ctag} || '|' || {path} || '|' || {mtime}")
}

impl TreeStore {
    /// Placed images and videos whose cached thumbnail was not made for what
    /// they are now ([`TreeStore::thumbnail_made`]), with their paths: up to
    /// `limit` of them, looking at the candidates after id `after` in id
    /// order, [`THUMB_PAGE`] at a time and at most
    /// [`THUMB_SCAN`] in one call. Each page is filtered and its paths found
    /// in one query.
    pub fn thumbnail_candidates(&self, after: &str, limit: usize) -> Result<ThumbnailBatch, TreeError> {
        if limit == 0 {
            return Ok(ThumbnailBatch { wanted: Vec::new(), next: Some(after.to_owned()) });
        }
        let page = format!(
            "SELECT id, parent_id, name, placement FROM items
              WHERE kind = '{FILE}' AND {placed} AND ctag IS NOT NULL
                AND (mime LIKE 'image/%' OR mime LIKE 'video/%') AND id > ?2
              ORDER BY id LIMIT ?3",
            placed = placed("placement")
        );
        let Some(root) = self.root_item_id()? else { return Ok(ThumbnailBatch::default()) };
        let not_made = format!(
            "SELECT {}, c.path FROM chain c JOIN items i ON i.id = c.start
              WHERE c.parent_id = ?1 AND c.above AND c.own
                AND (i.thumb_key IS NULL OR i.thumb_key != {key})
              ORDER BY i.id",
            ROW_COLUMNS.split(", ").map(|c| format!("i.{c}")).collect::<Vec<_>>().join(", "),
            key = thumb_key_sql("i.ctag", "c.path", "i.mtime")
        );
        let sql = chains_then(Source::Items, &page, &not_made);
        let mut wanted = Vec::new();
        let mut from = after.to_owned();
        let mut scanned = 0;
        loop {
            let (last, n): (Option<String>, usize) = self.conn.prepare_cached(&format!("SELECT max(id), count(*) FROM ({page})"))?.query_row(
                params![root, from, THUMB_PAGE as i64],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)),
            )?;
            let Some(last) = last else { return Ok(ThumbnailBatch { wanted, next: None }) };
            let mut statement = self.conn.prepare_cached(&sql)?;
            let found = statement
                .query_map(params![root, from, THUMB_PAGE as i64], |r| Ok(Thumbnail { row: row_from(r)?, rel: PathBuf::from(r.get::<_, String>(ROW_WIDTH)?) }))?;
            for candidate in found {
                wanted.push(candidate?);
                if wanted.len() == limit {
                    // The next call looks at the rest of the page again.
                    let next = wanted.last().map(|taken| taken.row.id.clone());
                    return Ok(ThumbnailBatch { wanted, next });
                }
            }
            scanned += n;
            if n < THUMB_PAGE {
                return Ok(ThumbnailBatch { wanted, next: None });
            }
            from = last;
            if scanned >= THUMB_SCAN {
                return Ok(ThumbnailBatch { wanted, next: Some(from) });
            }
        }
    }

    /// The cached thumbnail of `made.row`'s item was made for that row at
    /// that place: the next cycle does not make it again while its version,
    /// path and mtime stay.
    pub fn thumbnail_made(&self, made: &Thumbnail) -> Result<(), TreeError> {
        // The row and the place it was made for, not what `items` has now.
        let sql = format!("UPDATE items SET thumb_key = {} WHERE id = ?1", thumb_key_sql("?2", "?3", "?4"));
        let rel = made.rel.to_string_lossy();
        self.conn.execute(&sql, params![made.row.id, made.row.ctag.as_deref().unwrap_or(""), rel, made.row.mtime])?;
        Ok(())
    }
}
