use std::sync::Arc;

use tokio::task::JoinHandle;
use zbus::object_server::InterfaceRef;
use zbus::zvariant::ObjectPath;
use zbus::{interface, Connection};

use crate::account::AccountService;
use crate::account::state::AccountSnapshot;
use crate::dbus::fault::{Fault, Result};
#[cfg(feature = "dev-tools")]
use crate::dbus::token_export::TokenExport;

pub struct Account {
    service: Arc<AccountService>,
}

#[interface(name = "org.konedrive.Account")]
impl Account {
    async fn begin_sign_in(&self) -> Result<String> {
        self.service.begin_sign_in().await.map_err(Fault::from)
    }

    async fn cancel_sign_in(&self) {
        self.service.cancel_sign_in().await;
    }

    async fn sign_out(&self) -> Result<()> {
        self.service.sign_out().await.map_err(Fault::from)
    }

    async fn refresh_info(&self) {
        let service = Arc::clone(&self.service);
        tokio::spawn(async move { service.refresh_account_info().await });
    }

    /// The rules of `Accounts.Add`; `InvalidArgs` otherwise.
    async fn set_label(&self, label: &str) -> Result<()> {
        self.service.set_label(label).map_err(Fault::from)
    }

    /// Switches the account's mode (`docs/design/writes.md` §2); the URL of the sign-in the switch
    /// needs, empty when it needs none.
    #[zbus(out_args("sign_in_url"))]
    async fn set_mode(&self, mode: &str, force: bool) -> Result<String> {
        self.service.set_mode(mode, force).await.map_err(Fault::from)
    }

    #[zbus(property)]
    async fn id(&self) -> String {
        self.service.id().to_string()
    }

    #[zbus(property)]
    async fn label(&self) -> String {
        self.service.state().get().label
    }

    /// The mode the account runs in (`docs/design/writes.md` §2), not only the one `config.toml` asks
    /// for: `LastError` says why the two differ.
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
        self.service.state().get().published_error()
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
        self.service.state().get().quota.used
    }

    #[zbus(property)]
    async fn quota_total(&self) -> u64 {
        self.service.state().get().quota.total
    }

    /// Graph's `quota.remaining` as last read, less what went up since (`crate::account::quota`).
    #[zbus(property)]
    async fn quota_remaining(&self) -> u64 {
        self.service.state().get().quota.remaining
    }

    /// Graph's `quota.state` as last read: `normal`, `nearing`, `critical`, `exceeded`.
    #[zbus(property)]
    async fn quota_state(&self) -> String {
        self.service.state().get().quota.state
    }
}

/// Serves one account's `Account` (and, in a development build, `TokenExport`) at `path`, and turns its state changes into
/// `PropertiesChanged`; the task that sends them, to stop when the account goes.
///
/// At startup this runs before the bus name is claimed (`crate::daemon::startup::serve`), and
/// after the session was restored from the wallet: a D-Bus-activated client's first call is
/// never answered from stale, pre-restore state.
pub async fn export(connection: &Connection, path: &ObjectPath<'_>, service: Arc<AccountService>) -> zbus::Result<JoinHandle<()>> {
    // Captured before the interface is on the bus (not inside the task): otherwise a state
    // change landing in between would be absorbed into this baseline instead of being
    // emitted as a PropertiesChanged signal.
    let mut changes = service.state().subscribe();
    let mut previous = changes.borrow_and_update().clone();
    let server = connection.object_server();
    server.at(path, Account { service: Arc::clone(&service) }).await?;
    #[cfg(feature = "dev-tools")]
    server.at(path, TokenExport { service: Arc::clone(&service) }).await?;
    let iface = server.interface::<_, Account>(path).await?;
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

/// Takes one account's `Account` and `TokenExport` off the bus (`Accounts.Remove`).
pub async fn unexport(connection: &Connection, path: &ObjectPath<'_>, partly: bool) -> zbus::Result<()> {
    use crate::dbus::export::{all_taken_off, take_off};
    all_taken_off([
        #[cfg(feature = "dev-tools")]
        take_off::<TokenExport>(connection, path, partly).await,
        take_off::<Account>(connection, path, partly).await,
    ])
}

async fn emit_changes(
    iface: &InterfaceRef<Account>,
    old: &AccountSnapshot,
    new: &AccountSnapshot,
) -> zbus::Result<()> {
    let emitter = iface.signal_emitter();
    let account = iface.get().await;
    if old.state != new.state {
        account.state_changed(emitter).await?;
    }
    if old.published_error() != new.published_error() {
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
    if old.quota.used != new.quota.used {
        account.quota_used_changed(emitter).await?;
    }
    if old.quota.total != new.quota.total {
        account.quota_total_changed(emitter).await?;
    }
    if old.quota.remaining != new.quota.remaining {
        account.quota_remaining_changed(emitter).await?;
    }
    if old.quota.state != new.quota.state {
        account.quota_state_changed(emitter).await?;
    }
    if old.mode != new.mode {
        account.mode_changed(emitter).await?;
    }
    Ok(())
}
