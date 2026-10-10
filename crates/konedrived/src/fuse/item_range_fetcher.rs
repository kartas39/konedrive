//! The bytes of a file that is not on disk, asked of OneDrive as a range, by the item's
//! binding and the content the reader opened: what `range_fetcher` is today by a row of
//! `onedrive_items`.
//!
//! **State owned:** none of its own: the files being read as ranges are `range_readers`',
//! keyed by the binding's item id as today (`RangeSource::onedrive_item`), which is what
//! the content source is asked by: nothing of it changes; what the content source
//! remembers between two reads is the source's.
//!
//! **Answers for:** design §9, "a read: not on disk: the ranges come from OneDrive"; the
//! range is of the content the handle was opened as, never of whatever OneDrive has by
//! then at the size the mount shows.
//!
//! **Tested without the rest**, with a content source the test holds:
// test: a_range_is_asked_by_the_bindings_item_id_and_answered_from_the_offset_asked
// test: a_stream_that_starts_elsewhere_than_asked_is_an_error
// test: a_range_of_a_content_onedrive_no_longer_has_is_stale
// test: what_the_source_remembers_of_an_item_is_dropped_when_its_content_changes
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use konedrive_tree::LocalItemId;

use super::item_read_origin::RangeSource;
use super::range_readers::RangeReaders;
use crate::hydration::source::ContentSource;

/// Why a range did not come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RangeRefusal {
    /// OneDrive has another content of the item than the one the reader opened: `ESTALE`.
    AnotherContent,
    /// The source failed: the errno.
    Failed { errno: i32 },
}

/// The range fetcher of one mount.
pub(super) struct ItemRangeFetcher {
    range_readers: Arc<RangeReaders>,
    source: Arc<dyn ContentSource>,
}

impl ItemRangeFetcher {
    /// Made by the mount's build. `new` and `forget_ranges` are filled first, with the
    /// daemon's leaves (the steps towards the folder call `forget_ranges`); `fetch_range`
    /// with the reading requests.
    pub(super) fn new(range_readers: Arc<RangeReaders>, source: Arc<dyn ContentSource>) -> Self {
        todo!()
    }

    /// The bytes of `source` from `from` up to `end`. Asks the content source by the
    /// binding's item id (`ContentSource::read_range`; the drive is the account's until
    /// the source can ask by drive), and tells another content by what the answer says of
    /// the item's size and version against `source.content`.
    // OPEN: whether Graph takes a condition on a content download (the review's verdict
    // on OPEN 14: to be measured). Until then another content is told after the answer,
    // by its size and time, and not by a refused request: the source's answer carries no
    // cTag, so a content of the same size and time reads as the same content.
    pub(super) async fn fetch_range(&self, item: LocalItemId, source: &RangeSource, from: u64, end: u64) -> Result<Vec<u8>, RangeRefusal> {
        todo!()
    }

    /// `item` shows another content, or is gone: what the source remembered of its
    /// binding between range reads is dropped. Called by the steps towards the folder
    /// that take OneDrive's content or remove an item (today the cycle step, for every
    /// cycle: `cycle_step.rs:126`).
    pub(super) fn forget_ranges(&self, source: &RangeSource) {
        todo!()
    }
}
