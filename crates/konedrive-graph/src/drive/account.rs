//! The two calls the account page needs, one of them also the account's identity: who is
//! signed in (`GET /me`) and which drive it is (`GET /me/drive`). Both take the token to
//! ask with, because a sign-in asks before the token is the account's: they neither renew
//! it nor wait out throttling.

use serde::de::DeserializeOwned;
use serde::Deserialize;

use super::error::{classify, Detail, Kind, Status};
use super::send::{Auth, Throttle};
use super::{DriveClient, DriveError, DriveQuota};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub display_name: String,
    pub email: String,
}

/// `GET /me/drive`: the drive's id — the account's identity (design §8) — and its quota:
/// `used` and `total` for the account page, Graph's `remaining` and `state` for the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    /// Empty only if Graph left it out.
    pub id: String,
    pub quota: DriveQuota,
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
    quota: DriveQuota,
}

impl DriveClient {
    /// Who `token` is for. A token Graph rejects is [`DriveError::Failed`] with the status
    /// `401` ([`DriveError::status`]).
    pub async fn profile(&self, token: &str) -> Result<Profile, DriveError> {
        let me: MeBody = self.account_json("me", token).await?;
        Ok(Profile {
            display_name: me.display_name.unwrap_or_default(),
            email: me.mail.filter(|mail| !mail.is_empty()).or(me.user_principal_name).unwrap_or_default(),
        })
    }

    /// Which drive `token` reaches, and its quota. A token Graph rejects is
    /// [`DriveError::Failed`] with the status `401`.
    pub async fn drive(&self, token: &str) -> Result<Drive, DriveError> {
        let drive: DriveBody = self.account_json("me/drive", token).await?;
        Ok(Drive { id: drive.id, quota: drive.quota })
    }

    async fn account_json<T: DeserializeOwned>(&self, route: &str, token: &str) -> Result<T, DriveError> {
        let url = self.base.join(route).map_err(|e| DriveError::Failed(e.to_string().into()))?;
        let response = self.send(Auth::Bearer(token), Throttle::Pass, || self.api.get(url.clone())).await?;
        let status = Status::of(&response);
        if status.is_success() {
            return response
                .json()
                .await
                .map_err(|e| DriveError::Failed(format!("unreadable response from {route}: {}", e.without_url()).into()));
        }
        let message = match classify(status, "") {
            Kind::Unauthorized => "Microsoft Graph rejected the access token".to_owned(),
            _ => format!("{route} returned {status}"),
        };
        Err(DriveError::Failed(Detail::answered(message, status, "")))
    }
}

#[cfg(test)]
mod tests;
