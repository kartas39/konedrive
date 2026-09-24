//! `org.konedrive.Account1` on the session bus.

use std::sync::Arc;

use konedrive_dbus::{OBJECT_PATH, SERVICE_NAME};
use zbus::object_server::InterfaceRef;
use zbus::{fdo, interface, Connection};

use crate::account::{AccountError, AccountService};
use crate::state::AccountSnapshot;

pub struct Account1 {
    service: Arc<AccountService>,
}

#[interface(name = "org.konedrive.Account1")]
impl Account1 {
    async fn set_client_id(&self, id: &str) -> fdo::Result<()> {
        self.service.set_client_id(id).map_err(to_fdo)
    }

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

    #[zbus(property)]
    async fn state(&self) -> String {
        self.service.state().get().state.as_str().to_owned()
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.service.state().get().last_error
    }

    #[zbus(property)]
    async fn client_id(&self) -> String {
        self.service.state().get().client_id
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
        AccountError::InvalidClientId => fdo::Error::InvalidArgs(error.to_string()),
        other => fdo::Error::Failed(other.to_string()),
    }
}

/// `org.konedrive.Dev1`: development only.
struct Dev1 {
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
    /// The current access token — never the refresh token.
    async fn access_token(&self) -> std::result::Result<String, DevFault> {
        match self.service.tokens().access_token().await {
            Ok(token) => Ok(token),
            Err(crate::token::AuthError::SignedOut) => Err(DevFault::NotSignedIn("nobody is signed in".into())),
            Err(e) => Err(DevFault::Failed(e.to_string())),
        }
    }
}

/// Serves both interfaces through `builder` and turns state changes into
/// PropertiesChanged.
///
/// `sync` is served on the same object path, and — the point of it being a
/// parameter here rather than a later `attach` — through the same builder,
/// so that both interfaces are advertised *before* `SERVICE_NAME` is
/// claimed (zbus requests the name after registering everything
/// `serve_at` was given). A D-Bus-activated client's first call therefore
/// cannot land on a daemon that owns the name but does not yet answer
/// `Sync1`, and the same discipline the comment below states
/// for `Account1`'s restored state.
pub async fn serve(
    builder: zbus::connection::Builder<'_>,
    service: Arc<AccountService>,
    sync: Option<Arc<crate::sync::SyncService>>,
) -> zbus::Result<Connection> {
    let mut builder = builder
        .name(SERVICE_NAME)?
        .serve_at(OBJECT_PATH, Account1 { service: Arc::clone(&service) })?
        .serve_at(OBJECT_PATH, Dev1 { service: Arc::clone(&service) })?;
    if let Some(sync) = &sync {
        builder = crate::sync::dbus::add_to_builder(builder, Arc::clone(sync))?;
    }
    let connection = builder.build().await?;
    if let Some(sync) = sync {
        crate::sync::dbus::start_signals(&connection, sync).await?;
    }
    let iface = connection
        .object_server()
        .interface::<_, Account1>(OBJECT_PATH)
        .await?;
    // Captured before spawning (not inside the task): otherwise a state change landing
    // between claiming the name and the task's first poll would be absorbed into this
    // baseline instead of being emitted as a PropertiesChanged signal.
    let mut changes = service.state().subscribe();
    let mut previous = changes.borrow_and_update().clone();
    tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            let current = changes.borrow_and_update().clone();
            if let Err(e) = emit_changes(&iface, &previous, &current).await {
                tracing::warn!("cannot emit PropertiesChanged: {e}");
            }
            previous = current;
        }
    });
    Ok(connection)
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
    if old.client_id != new.client_id {
        account.client_id_changed(emitter).await?;
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
