//! `org.konedrive.Account1` and `org.konedrive.Dev1`, one of each per account on the
//! account's object `/org/konedrive/Accounts/<id>` (definitions: `dbus/*.xml`). The
//! accounts themselves, and the client id every account signs in with, are
//! `org.konedrive.Accounts1`'s (`crate::accounts`).

use std::sync::Arc;

use tokio::task::JoinHandle;
use zbus::object_server::InterfaceRef;
use zbus::zvariant::ObjectPath;
use zbus::{fdo, interface, Connection};

use crate::account::{AccountError, AccountService};
use crate::state::AccountSnapshot;

pub struct Account1 {
    service: Arc<AccountService>,
}

#[interface(name = "org.konedrive.Account1")]
impl Account1 {
    async fn begin_sign_in(&self) -> fdo::Result<String> {
        self.service.begin_sign_in().await.map_err(to_fdo)
    }

    async fn cancel_sign_in(&self) {
        self.service.cancel_sign_in().await;
    }

    async fn sign_out(&self) -> fdo::Result<()> {
        self.service.sign_out().await.map_err(to_fdo)
    }

    async fn refresh_account_info(&self) {
        let service = Arc::clone(&self.service);
        tokio::spawn(async move { service.refresh_account_info().await });
    }

    /// The rules of `Accounts1.Add`; `InvalidArgs` otherwise.
    async fn set_label(&self, label: &str) -> fdo::Result<()> {
        self.service.set_label(label).map_err(to_fdo)
    }

    #[zbus(property)]
    async fn id(&self) -> String {
        self.service.id().to_owned()
    }

    #[zbus(property)]
    async fn label(&self) -> String {
        self.service.state().get().label
    }

    #[zbus(property)]
    async fn mode(&self) -> String {
        self.service.mode().as_str().to_owned()
    }

    #[zbus(property)]
    async fn state(&self) -> String {
        self.service.state().get().state.as_str().to_owned()
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.service.state().get().last_error
    }

    #[zbus(property)]
    async fn display_name(&self) -> String {
        self.service.state().get().display_name
    }

    #[zbus(property)]
    async fn email(&self) -> String {
        self.service.state().get().email
    }

    #[zbus(property)]
    async fn quota_used(&self) -> u64 {
        self.service.state().get().quota_used
    }

    #[zbus(property)]
    async fn quota_total(&self) -> u64 {
        self.service.state().get().quota_total
    }
}

fn to_fdo(error: AccountError) -> fdo::Error {
    match error {
        AccountError::InvalidClientId | AccountError::InvalidLabel(_) => fdo::Error::InvalidArgs(error.to_string()),
        other => fdo::Error::Failed(other.to_string()),
    }
}

/// `org.konedrive.Dev1`: development only.
pub struct Dev1 {
    service: Arc<AccountService>,
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.konedrive.Error")]
enum DevFault {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotSignedIn(String),
    Failed(String),
}

#[zbus::interface(name = "org.konedrive.Dev1")]
impl Dev1 {
    /// This account's current access token — never the refresh token.
    async fn access_token(&self) -> std::result::Result<String, DevFault> {
        match self.service.tokens().access_token().await {
            Ok(token) => Ok(token),
            Err(crate::token::AuthError::SignedOut) => Err(DevFault::NotSignedIn("nobody is signed in".into())),
            Err(e) => Err(DevFault::Failed(e.to_string())),
        }
    }
}

/// Serves one account's `Account1` and `Dev1` at `path`, and turns its state changes into
/// `PropertiesChanged`; the task that sends them, to stop when the account goes.
///
/// At startup this runs before the bus name is claimed (`crate::accounts::serve`), and
/// after the session was restored from the wallet: a D-Bus-activated client's first call is
/// never answered from stale, pre-restore state.
pub async fn export(connection: &Connection, path: &ObjectPath<'_>, service: Arc<AccountService>) -> zbus::Result<JoinHandle<()>> {
    // Captured before the interface is on the bus (not inside the task): otherwise a state
    // change landing in between would be absorbed into this baseline instead of being
    // emitted as a PropertiesChanged signal.
    let mut changes = service.state().subscribe();
    let mut previous = changes.borrow_and_update().clone();
    let server = connection.object_server();
    server.at(path, Account1 { service: Arc::clone(&service) }).await?;
    server.at(path, Dev1 { service: Arc::clone(&service) }).await?;
    let iface = server.interface::<_, Account1>(path).await?;
    Ok(tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            let current = changes.borrow_and_update().clone();
            if let Err(e) = emit_changes(&iface, &previous, &current).await {
                tracing::warn!("cannot emit PropertiesChanged: {e}");
            }
            previous = current;
        }
    }))
}

/// Takes one account's `Account1` and `Dev1` off the bus (`Accounts1.Remove`).
pub async fn unexport(connection: &Connection, path: &ObjectPath<'_>) -> zbus::Result<()> {
    let server = connection.object_server();
    server.remove::<Dev1, _>(path).await?;
    server.remove::<Account1, _>(path).await.map(drop)
}

async fn emit_changes(
    iface: &InterfaceRef<Account1>,
    old: &AccountSnapshot,
    new: &AccountSnapshot,
) -> zbus::Result<()> {
    let emitter = iface.signal_emitter();
    let account = iface.get().await;
    if old.state != new.state {
        account.state_changed(emitter).await?;
    }
    if old.last_error != new.last_error {
        account.last_error_changed(emitter).await?;
    }
    if old.label != new.label {
        account.label_changed(emitter).await?;
    }
    if old.display_name != new.display_name {
        account.display_name_changed(emitter).await?;
    }
    if old.email != new.email {
        account.email_changed(emitter).await?;
    }
    if old.quota_used != new.quota_used {
        account.quota_used_changed(emitter).await?;
    }
    if old.quota_total != new.quota_total {
        account.quota_total_changed(emitter).await?;
    }
    Ok(())
}
