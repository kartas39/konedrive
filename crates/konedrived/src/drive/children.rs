//! A folder's children, page by page: what the outbox worker compares a
//! folder's base with when the folder's cTag guard failed on its delete
//! (`docs/design/writes.md` §6.2).

use serde::Deserialize;

use super::{DriveClient, DriveError, DriveItem};

/// A folder of more children than this is not listed further: its delete
/// is then treated as one whose folder changed (nothing unseen is deleted).
pub const MAX_CHILDREN: usize = 100_000;

#[derive(Deserialize)]
struct ChildrenBody {
    value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

impl DriveClient {
    /// Every child of folder `id`, following Graph's pages. `None` when there
    /// are more than [`MAX_CHILDREN`].
    pub async fn children(&self, id: &str) -> Result<Option<Vec<DriveItem>>, DriveError> {
        let mut url = self.item_url(id, Some("children"))?;
        let mut out = Vec::new();
        loop {
            let body: ChildrenBody = self.get_json(url).await?;
            out.extend(body.value);
            if out.len() > MAX_CHILDREN {
                return Ok(None);
            }
            match body.next_link {
                Some(next) => url = self.same_host(&next)?,
                None => return Ok(Some(out)),
            }
        }
    }
}
