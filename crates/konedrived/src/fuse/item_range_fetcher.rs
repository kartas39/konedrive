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
// test: an_answer_tells_another_content_by_its_ctag_else_by_its_size_and_time_and_one_that_says_nothing_tells_none
// test: a_source_that_fails_is_an_error_of_the_read
#![allow(dead_code, unused_variables)]

use std::sync::Arc;
use std::time::UNIX_EPOCH;

use konedrive_tree::{ContentVersion, LocalItemId};

use super::errno::to_raw_errno;
use crate::hydration::source::{Fetched, SourceError};

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

/// What the answer to a range says of the item it is a range of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RangeAnswerFacts {
    /// The item's size; 0 from a source that does not say.
    pub(super) size: u64,
    /// The item's modification time in Unix seconds; 0 from a source that does not say.
    pub(super) mtime: i64,
    /// The cTag of the content the bytes are of, when the source says it.
    pub(super) ctag: Option<String>,
}

impl RangeAnswerFacts {
    fn of(fetched: &Fetched) -> Self {
        let mtime = fetched.mtime.duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs() as i64);
        RangeAnswerFacts { size: fetched.size, mtime, ctag: fetched.version.as_ref().map(|version| version.ctag.clone()) }
    }
}

/// Whether an answer that says `answer` is of another content than `content`, the one the
/// reader opened. Pure. The cTags decide when both sides have one. Else the size and the
/// time in whole seconds do; an answer with neither (`ContentSource::read_range` may
/// leave both empty, and the source over Graph does) says nothing, and tells no other
/// content.
pub(super) fn decide_is_another_content(answer: &RangeAnswerFacts, content: &ContentVersion) -> bool {
    if let (Some(answered), ContentVersion::Hashed { ctag: Some(opened), .. }) = (&answer.ctag, content) {
        return answered != opened;
    }
    if answer.size == 0 && answer.mtime == 0 {
        return false;
    }
    answer.size != content.size() || answer.mtime != content.mtime().0
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
        ItemRangeFetcher { range_readers, source }
    }

    /// The bytes of `source` from `from` up to `end`. Asks the content source by the
    /// binding's item id (`ContentSource::read_range`; the drive is the account's until
    /// the source can ask by drive), and tells another content by what the answer says of
    /// the item's size and version against `source.content`
    /// ([`decide_is_another_content`]). A source that says OneDrive has no such file any
    /// more (`SourceError::NotFound`) is another content too: the one the reader opened
    /// is not there. While the range is fetched the item counts as being read
    /// (`RangeReaders`), which holds the background downloads back. `item` is named in
    /// the journal.
    ///
    /// **The limit:** the source over Graph answers a range with no size, no time and no
    /// cTag, so that through it another content is told only by OneDrive not having the
    /// file; a content OneDrive replaced is read as the one opened until the listing's
    /// step changes the row (`decide_not_on_disk_read_origin` then says `ESTALE`). And
    /// where a source says a size and a time and no cTag, a content of the same size and
    /// time reads as the same content.
    // OPEN: whether Graph takes a condition on a content download (the review's verdict
    // on OPEN 14: to be measured). Until then another content is told after the answer
    // and not by a refused request.
    pub(super) async fn fetch_range(&self, item: LocalItemId, source: &RangeSource, from: u64, end: u64) -> Result<Vec<u8>, RangeRefusal> {
        use tokio::io::AsyncReadExt;
        let onedrive_item_id = source.onedrive_item.item_id.as_str();
        let _active_read = self.range_readers.begin_read(onedrive_item_id).await;
        let asked = std::time::Instant::now();
        let fetched = match self.source.read_range(onedrive_item_id, from, end).await {
            Ok(fetched) => fetched,
            Err(SourceError::NotFound(detail)) => {
                tracing::info!("a range of item {item} was not fetched: OneDrive does not have the content that was opened: {detail}");
                return Err(RangeRefusal::AnotherContent);
            }
            // A sign-out and a throttle are no other content: the read fails, and is asked again.
            Err(SourceError::Transient(detail) | SourceError::SignedOut(detail) | SourceError::Throttled { detail, .. }) => {
                tracing::warn!("a range of item {item} was not fetched: {detail}");
                return Err(RangeRefusal::Failed { errno: libc::EIO });
            }
        };
        if decide_is_another_content(&RangeAnswerFacts::of(&fetched), &source.content) {
            tracing::info!("a range of item {item} is of another content than the one that was opened: nothing of it is read");
            return Err(RangeRefusal::AnotherContent);
        }
        if fetched.served_from != from {
            tracing::error!("item {item}: asked for byte {from} and got a stream starting at {}", fetched.served_from);
            return Err(RangeRefusal::Failed { errno: libc::EIO });
        }
        let mut bytes = Vec::with_capacity(end.saturating_sub(from) as usize);
        let answered = asked.elapsed();
        fetched.stream.take(end.saturating_sub(from)).read_to_end(&mut bytes).await.map_err(|error| RangeRefusal::Failed { errno: to_raw_errno(&error) })?;
        tracing::info!("{} bytes at {from} of item {item} came from OneDrive in {} ms ({} ms to the answer)", bytes.len(), asked.elapsed().as_millis(), answered.as_millis());
        Ok(bytes)
    }

    /// `item` shows another content, or is gone: what the source remembered of its
    /// binding between range reads is dropped. Called by the steps towards the folder
    /// that take OneDrive's content or remove an item (today the cycle step, for every
    /// cycle: `cycle_step.rs:126`).
    pub(super) fn forget_ranges(&self, source: &RangeSource) {
        self.source.forget(source.onedrive_item.item_id.as_str());
    }
}

#[cfg(test)]
mod tests;
