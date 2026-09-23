//! The two Microsoft Graph calls the account page needs.

use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub display_name: String,
    pub email: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    pub used: u64,
    pub total: u64,
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
    quota: QuotaBody,
}

#[derive(Deserialize)]
struct QuotaBody {
    #[serde(default)]
    used: u64,
    #[serde(default)]
    total: u64,
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

    pub async fn quota(&self, token: &str) -> Result<Quota, GraphError> {
        let drive: DriveBody = self.get_json("me/drive", token).await?;
        Ok(Quota { used: drive.quota.used, total: drive.quota.total })
    }

    async fn get_json<T: DeserializeOwned>(&self, route: &str, token: &str) -> Result<T, GraphError> {
        let url = self.base.join(route).map_err(|e| GraphError::Failed(e.to_string()))?;
        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| GraphError::Failed(format!("cannot reach Microsoft Graph: {e}")))?;
        match response.status() {
            status if status.is_success() => response
                .json()
                .await
                .map_err(|e| GraphError::Failed(format!("unreadable response from {route}: {e}"))),
            reqwest::StatusCode::UNAUTHORIZED => Err(GraphError::Unauthorized),
            status => Err(GraphError::Failed(format!("{route} returned {status}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn graph(server: &MockServer) -> GraphClient {
        GraphClient::new(reqwest::Client::new(), Url::parse(&format!("{}/", server.uri())).unwrap())
    }

    async fn mock_get(server: &MockServer, route: &str, status: u16, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", "Bearer T"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn profile_prefers_mail() {
        let server = MockServer::start().await;
        mock_get(&server, "/me", 200, json!({"displayName": "Ann", "mail": "ann@example.com", "userPrincipalName": "upn@example.com"})).await;
        let profile = graph(&server).profile("T").await.unwrap();
        assert_eq!(profile, Profile { display_name: "Ann".into(), email: "ann@example.com".into() });
    }

    #[tokio::test]
    async fn profile_falls_back_to_user_principal_name() {
        let server = MockServer::start().await;
        mock_get(&server, "/me", 200, json!({"displayName": "Ann", "mail": null, "userPrincipalName": "ann@outlook.com"})).await;
        assert_eq!(graph(&server).profile("T").await.unwrap().email, "ann@outlook.com");
    }

    #[tokio::test]
    async fn quota_reads_used_and_total() {
        let server = MockServer::start().await;
        mock_get(&server, "/me/drive", 200, json!({"id": "d", "quota": {"used": 10, "total": 100, "remaining": 90}})).await;
        assert_eq!(graph(&server).quota("T").await.unwrap(), Quota { used: 10, total: 100 });
    }

    #[tokio::test]
    async fn distinguishes_unauthorized_from_other_failures() {
        let server = MockServer::start().await;
        mock_get(&server, "/me", 401, json!({})).await;
        mock_get(&server, "/me/drive", 500, json!({})).await;
        assert!(matches!(graph(&server).profile("T").await, Err(GraphError::Unauthorized)));
        assert!(matches!(graph(&server).quota("T").await, Err(GraphError::Failed(_))));
    }
}
