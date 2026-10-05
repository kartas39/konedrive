use std::sync::Arc;

use crate::account::AccountService;
#[cfg(feature = "dev-tools")]
use crate::dbus::fault::{export_fault, Fault};
#[cfg(feature = "dev-tools")]
use konedrive_dbus::Refusal;

/// `org.konedrive.TokenExport`: only in a development build (the `dev-tools` feature); a
/// release has neither the interface nor `konedrivectl dev` (limitations log W11).
#[cfg(feature = "dev-tools")]
pub struct TokenExport {
    pub(crate) service: Arc<AccountService>,
}

#[cfg(feature = "dev-tools")]
#[zbus::interface(name = "org.konedrive.TokenExport")]
impl TokenExport {
    /// An access token of this account that can change nothing, whatever its mode (write
    /// design §10) — never the refresh token.
    async fn read_only(&self) -> std::result::Result<String, Fault> {
        match self.service.read_only_token().await {
            Ok(token) => Ok(token),
            Err(konedrive_graph::token::AuthError::SignedOut) => Err(Fault::refused(Refusal::NotSignedIn, "nobody is signed in")),
            Err(e) => Err(Fault::refused(Refusal::Failed, e.to_string())),
        }
    }

    /// The test-account harness's token, which can change files: refused `WritesNotAllowed`
    /// for an account whose drive `write_test_drive_ids` does not list, and `ModeNotGranted` for one that is
    /// not read-write.
    async fn read_write(&self) -> std::result::Result<String, Fault> {
        self.service.read_write_token().await.map_err(export_fault)
    }
}
