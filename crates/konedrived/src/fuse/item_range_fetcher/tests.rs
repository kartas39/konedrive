use std::sync::Mutex;

use konedrive_tree::{ContentVersion, DriveId, OneDriveItemId, OneDriveItemReference};

use super::*;
use crate::fuse::range_readers::tests::range_readers;
use crate::hydration::source::{Fetched, SourceError};

/// A source that keeps what it was told to forget.
#[derive(Default)]
struct RememberingSource {
    forgotten: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ContentSource for RememberingSource {
    async fn fetch(&self, _item_id: &str, _from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        unreachable!("no range is fetched here")
    }

    fn forget(&self, item_id: &str) {
        self.forgotten.lock().unwrap().push(item_id.to_owned());
    }
}

fn range_source(item_id: &str) -> RangeSource {
    RangeSource {
        onedrive_item: OneDriveItemReference { drive_id: DriveId::new("drive").unwrap(), item_id: OneDriveItemId::new(item_id) },
        content: ContentVersion::Hashed { size: 10, mtime: 1_700_000_000, mtime_nsec: 0, hash: None, ctag: Some("c1".into()) },
    }
}

#[tokio::test]
async fn what_the_source_remembers_of_an_item_is_dropped_when_its_content_changes() {
    let (_pool, range_readers) = range_readers();
    let source = Arc::new(RememberingSource::default());
    let range_fetcher = ItemRangeFetcher::new(range_readers, Arc::clone(&source) as Arc<dyn ContentSource>);
    range_fetcher.forget_ranges(&range_source("A1"));
    range_fetcher.forget_ranges(&range_source("B2"));
    assert_eq!(*source.forgotten.lock().unwrap(), ["A1", "B2"], "asked by the binding's item id, as the ranges are");
}
