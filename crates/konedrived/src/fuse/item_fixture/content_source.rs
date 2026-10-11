//! The content source of the fixture: the bytes of each OneDrive item as the test gave
//! them, and what the source answers in their place when the test asks for a refusal.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use crate::hydration::source::{ContentSource, Fetched, SourceError, Version};

/// What the source answers in place of an item's bytes. `SourceError` has these two cases
/// and no other: a source cannot say "signed out" or "throttled" through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentRefusal {
    /// `SourceError::NotFound`: OneDrive does not have the item any more.
    NotFound,
    /// `SourceError::Transient`: the network, or OneDrive for now.
    Transient,
}

/// One fetch the source was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContentFetch {
    pub(crate) onedrive_item_id: String,
    pub(crate) from: u64,
    pub(crate) end: Option<u64>,
}

struct ServedContent {
    bytes: Vec<u8>,
    /// Unix seconds: the time the source says the item was modified at.
    mtime: i64,
    /// The cTag and the hash the source says the bytes are of, when it says any.
    version: Option<Version>,
}

/// The source: what it serves by OneDrive item id, what it refuses, and what it was asked.
#[derive(Default)]
pub(crate) struct FixtureContentSource {
    served: Mutex<BTreeMap<String, ServedContent>>,
    refusals: Mutex<BTreeMap<String, ContentRefusal>>,
    fetches: Mutex<Vec<ContentFetch>>,
    forgotten: Mutex<Vec<String>>,
}

impl FixtureContentSource {
    /// The source serves `bytes` for `onedrive_item_id` from now on, modified at `mtime`,
    /// and says they are of `version`.
    pub(crate) fn serve(&self, onedrive_item_id: &str, bytes: &[u8], mtime: i64, version: Option<Version>) {
        crate::panic::lock(&self.served).insert(onedrive_item_id.to_owned(), ServedContent { bytes: bytes.to_vec(), mtime, version });
    }

    /// Every fetch of `onedrive_item_id` is answered `refusal` from now on; `None` takes
    /// the refusal away.
    pub(crate) fn refuse(&self, onedrive_item_id: &str, refusal: Option<ContentRefusal>) {
        let mut refusals = crate::panic::lock(&self.refusals);
        match refusal {
            Some(refusal) => refusals.insert(onedrive_item_id.to_owned(), refusal),
            None => refusals.remove(onedrive_item_id),
        };
    }

    /// The bytes the source serves for `onedrive_item_id`.
    pub(crate) fn find_served_bytes(&self, onedrive_item_id: &str) -> Option<Vec<u8>> {
        crate::panic::lock(&self.served).get(onedrive_item_id).map(|content| content.bytes.clone())
    }

    /// Every fetch the source was asked for, refused ones among them, in the order asked.
    pub(crate) fn list_fetches(&self) -> Vec<ContentFetch> {
        crate::panic::lock(&self.fetches).clone()
    }

    /// The items the source was told to forget (`ContentSource::forget`), in the order told.
    pub(crate) fn list_forgotten(&self) -> Vec<String> {
        crate::panic::lock(&self.forgotten).clone()
    }
}

#[async_trait::async_trait]
impl ContentSource for FixtureContentSource {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        crate::panic::lock(&self.fetches).push(ContentFetch { onedrive_item_id: item_id.to_owned(), from, end });
        match crate::panic::lock(&self.refusals).get(item_id) {
            Some(ContentRefusal::NotFound) => return Err(SourceError::NotFound(format!("the test said {item_id} is gone"))),
            Some(ContentRefusal::Transient) => return Err(SourceError::Transient(format!("the test said {item_id} cannot be fetched now"))),
            None => {}
        }
        let served = crate::panic::lock(&self.served);
        let Some(content) = served.get(item_id) else { return Err(SourceError::NotFound(format!("the test serves nothing for {item_id}"))) };
        let size = content.bytes.len() as u64;
        // A `from` at the end or past it is answered with an empty stream, as the trait asks.
        let start = from.min(size);
        let stop = end.map_or(size, |end| end.min(size)).max(start);
        let bytes = content.bytes[start as usize..stop as usize].to_vec();
        Ok(Fetched { served_from: start, size, mtime: UNIX_EPOCH + Duration::from_secs(content.mtime.max(0) as u64), version: content.version.clone(), stream: Box::new(std::io::Cursor::new(bytes)) })
    }

    fn forget(&self, item_id: &str) {
        crate::panic::lock(&self.forgotten).push(item_id.to_owned());
    }
}
