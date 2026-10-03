//! The content source of a folder that shows OneDrive: each fetch asks Graph
//! for the item's metadata afresh — the cTag
//! and quickXorHash the fill checks the bytes against, and a download URL that
//! has not expired — and streams from the offset asked for, to the end of
//! the file or to the end of the piece asked for.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use super::source::{ContentSource, Fetched, SourceError, Version};
use crate::drive::{DriveClient, DriveError, DriveItem};
use crate::quickxor::decode_base64;

pub struct GraphSource {
    drive: DriveClient,
}

impl GraphSource {
    pub fn new(drive: DriveClient) -> Self {
        Self { drive }
    }
}

#[async_trait]
impl ContentSource for GraphSource {
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
                        size: item.size.unwrap_or(0),
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
/// the fill at once; everything else is worth another try.
fn source_error(error: DriveError) -> SourceError {
    match error {
        DriveError::NotFound | DriveError::SignedOut => SourceError::NotFound(error.to_string()),
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
