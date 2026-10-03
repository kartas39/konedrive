use std::sync::Arc;

use crate::account::AccountService;
use crate::dbus::fault::ModeFault;

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
    async fn read_only(&self) -> std::result::Result<String, ModeFault> {
        match self.service.read_only_token().await {
            Ok(token) => Ok(token),
            Err(konedrive_graph::token::AuthError::SignedOut) => Err(ModeFault::NotSignedIn("nobody is signed in".into())),
            Err(e) => Err(ModeFault::Failed(e.to_string())),
        }
    }

    /// The test-account harness's token, which can change files: refused `WritesNotAllowed`
    /// for an account the gate does not let through, and `ModeNotGranted` for one that is
    /// not read-write.
    async fn read_write(&self) -> std::result::Result<String, ModeFault> {
        self.service.read_write_token().await.map_err(ModeFault::from)
    }
}
