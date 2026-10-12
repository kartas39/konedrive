//! The content source of a folder that shows OneDrive: each fetch asks Graph
//! for the item's metadata afresh — the cTag
//! and quickXorHash the fill checks the bytes against, and a download URL that
//! has not expired — and streams from the offset asked for, to the end of
//! the file or to the end of the piece asked for.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use super::source::{ContentSource, Fetched, SourceError, Version};
use konedrive_graph::drive::{DriveClient, DriveError, DriveItem};
use konedrive_graph::quickxor::decode_base64;

pub(crate) struct GraphSource {
    drive: DriveClient,
    /// The download address of each item a program reads, and when it was given: Graph hands
    /// out an address that works for about an hour, and a read of 32 KiB should not cost a
    /// request for one each time.
    addresses: std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
}

/// How long a download address is used before a new one is asked for. Graph's last about an
/// hour; one that stops working sooner is asked for again at once.
const ADDRESS_KEPT: Duration = Duration::from_secs(30 * 60);

impl GraphSource {
    pub fn new(drive: DriveClient) -> Self {
        Self { drive, addresses: Default::default() }
    }
}

#[async_trait]
impl ContentSource for GraphSource {
    async fn read_range(&self, item_id: &str, from: u64, end: u64) -> Result<Fetched, SourceError> {
        let remembered = || {
            let addresses = crate::panic::lock(&self.addresses);
            addresses.get(item_id).filter(|(_, since)| since.elapsed() < ADDRESS_KEPT).map(|(address, _)| address.clone())
        };
        // The remembered address first; when it does not serve, a fresh one, once.
        for address in [remembered(), None] {
            let (address, fresh) = match address {
                Some(address) => (address, false),
                None => {
                    let item = self.drive.item(item_id).await.map_err(source_error)?;
                    if item.deleted.is_some() || item.file.is_none() {
                        return Err(SourceError::NotFound(format!("{item_id} is not a file in OneDrive any more")));
                    }
                    let address = match &item.download_url {
                        Some(url) => url.clone(),
                        None => self.drive.content_url(item_id).await.map_err(source_error)?,
                    };
                    crate::panic::lock(&self.addresses).insert(item_id.to_owned(), (address.clone(), std::time::Instant::now()));
                    (address, true)
                }
            };
            match self.drive.download(&address, from, Some(end)).await {
                Ok(download) => {
                    return Ok(Fetched { served_from: download.served_from, size: None, mtime: SystemTime::UNIX_EPOCH, version: None, stream: download.stream })
                }
                Err(e) if fresh => return Err(source_error(e)),
                Err(_) => self.forget(item_id),
            }
        }
        Err(SourceError::Transient(format!("no download address of {item_id} served")))
    }

    fn forget(&self, item_id: &str) {
        crate::panic::lock(&self.addresses).remove(item_id);
    }

    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let mut fresh_link = false;
        loop {
            let item = self.drive.item(item_id).await.map_err(source_error)?;
            if item.deleted.is_some() || item.file.is_none() {
                return Err(SourceError::NotFound(format!("{item_id} is not a file in OneDrive any more")));
            }
            // When cTag is missing, version is None entirely — the fill checks cTag and hash together.
            let version = item.c_tag.clone().map(|ctag| Version {
                ctag,
                quick_xor: item.quick_xor_hash().and_then(decode_base64),
            });
            let url = match &item.download_url {
                Some(url) => url.clone(),
                None => self.drive.content_url(item_id).await.map_err(source_error)?,
            };
            match self.drive.download(&url, from, end).await {
                Ok(download) => {
                    return Ok(Fetched {
                        served_from: download.served_from,
                        // As OneDrive said it: a size it left out is not a size of 0.
                        size: item.size,
                        mtime: mtime_of(&item),
                        version,
                        stream: download.stream,
                    })
                }
                // A download URL lives about an hour: ask for a fresh one, once.
                Err(DriveError::UrlExpired) if !fresh_link => fresh_link = true,
                Err(e) => return Err(source_error(e)),
            }
        }
    }
}

/// A missing item — and a signed-out account, which no retry can fix — ends
/// the fill at once; everything else is worth another try. A throttle (429, 503) is told
/// apart from the rest of what passes: the drive has waited it out already as far as it
/// does, and told the pool the time OneDrive named, so none is given here.
fn source_error(error: DriveError) -> SourceError {
    let is_throttled = matches!(error.status().map(|status| status.code()), Some(429 | 503));
    match error {
        DriveError::NotFound => SourceError::NotFound(error.to_string()),
        DriveError::SignedOut => SourceError::SignedOut(error.to_string()),
        other if is_throttled => SourceError::Throttled { detail: other.to_string(), retry_after: None },
        other => SourceError::Transient(other.to_string()),
    }
}

fn mtime_of(item: &DriveItem) -> SystemTime {
    let seconds = item.mtime();
    if seconds <= 0 {
        return SystemTime::UNIX_EPOCH;
    }
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds as u64)
}


#[cfg(test)]
mod tests;
