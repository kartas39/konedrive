//! The two Microsoft Graph calls the account page needs, one of them also the account's identity.

use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub display_name: String,
    pub email: String,
}

/// The drive's quota: `used` and `total` for the account page, Graph's `remaining` and
/// `state` for the outbox (issue #2).
pub use crate::drive::DriveQuota as Quota;

/// `GET /me/drive`: the drive's id — the account's identity (design §8) — and its quota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    /// Empty only if Graph left it out.
    pub id: String,
    pub quota: Quota,
}

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("Microsoft Graph rejected the access token")]
    Unauthorized,
    #[error("{0}")]
    Failed(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MeBody {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    mail: Option<String>,
    #[serde(default)]
    user_principal_name: Option<String>,
}

#[derive(Deserialize)]
struct DriveBody {
    #[serde(default)]
    id: String,
    quota: Quota,
}

#[derive(Clone)]
pub struct GraphClient {
    http: reqwest::Client,
    base: Url,
}

impl GraphClient {
    pub fn new(http: reqwest::Client, base: Url) -> Self {
        Self { http, base }
    }

    pub async fn profile(&self, token: &str) -> Result<Profile, GraphError> {
        let me: MeBody = self.get_json("me", token).await?;
        Ok(Profile {
            display_name: me.display_name.unwrap_or_default(),
            email: me
                .mail
                .filter(|mail| !mail.is_empty())
                .or(me.user_principal_name)
                .unwrap_or_default(),
        })
    }

    pub async fn drive(&self, token: &str) -> Result<Drive, GraphError> {
        let drive: DriveBody = self.get_json("me/drive", token).await?;
        Ok(Drive { id: drive.id, quota: drive.quota })
    }

    async fn get_json<T: DeserializeOwned>(&self, route: &str, token: &str) -> Result<T, GraphError> {
        let url = self.base.join(route).map_err(|e| GraphError::Failed(e.to_string()))?;
        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| GraphError::Failed(format!("cannot reach Microsoft Graph: {}", e.without_url())))?;
        match response.status() {
            status if status.is_success() => response
                .json()
                .await
                .map_err(|e| GraphError::Failed(format!("unreadable response from {route}: {}", e.without_url()))),
            reqwest::StatusCode::UNAUTHORIZED => Err(GraphError::Unauthorized),
            status => Err(GraphError::Failed(format!("{route} returned {status}"))),
        }
    }
}

#[cfg(test)]
mod tests;
