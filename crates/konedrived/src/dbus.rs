//! `org.konedrive.Account1` and `org.konedrive.Dev1`, one of each per account on the
//! account's object `/org/konedrive/Accounts/<id>` (definitions: `dbus/*.xml`). The
//! accounts themselves, and the client id every account signs in with, are
//! `org.konedrive.Accounts1`'s (`crate::accounts`).

use std::sync::Arc;

use tokio::task::JoinHandle;
use zbus::object_server::InterfaceRef;
use zbus::zvariant::ObjectPath;
use zbus::{fdo, interface, Connection};

use crate::account::{AccountError, AccountService, ModeError};
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

    /// Switches the account's mode (`docs/design/writes.md` §2); the URL of the sign-in the switch
    /// needs, empty when it needs none.
    #[zbus(out_args("sign_in_url"))]
    async fn set_mode(&self, mode: &str, force: bool) -> Result<String, SetModeFault> {
        self.service.set_mode(mode, force).await.map_err(SetModeFault::from)
    }

    #[zbus(property)]
    async fn id(&self) -> String {
        self.service.id().to_owned()
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

/// The named refusals of `SetMode` and `Dev1` (`docs/design/writes.md` §11).
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.konedrive.Error")]
pub enum ModeFault {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotSignedIn(String),
    /// The development gate: the account's drive is not in `write_test_drive_ids`.
    WritesNotAllowed(String),
    /// The account's token does not carry `Files.ReadWrite`.
    ModeNotGranted(String),
    /// Changes wait to be uploaded, and the switch to read-only was not forced.
    PendingUploads(String),
    Failed(String),
}

impl From<ModeError> for ModeFault {
    fn from(error: ModeError) -> Self {
        match error {
            ModeError::WritesNotAllowed(why) => ModeFault::WritesNotAllowed(why),
            ModeError::ModeNotGranted(why) => ModeFault::ModeNotGranted(why),
            ModeError::PendingUploads(why) => ModeFault::PendingUploads(why),
            ModeError::NotSignedIn(why) => ModeFault::NotSignedIn(why),
            ModeError::InvalidMode(why) | ModeError::Failed(why) => ModeFault::Failed(why),
        }
    }
}

/// How `SetMode` refuses: under the named errors of [`ModeFault`], and under the bus's own
/// `InvalidArgs` for a mode that is not one, as `SetLabel` refuses a label.
#[derive(Debug)]
pub enum SetModeFault {
    Named(ModeFault),
    Fdo(fdo::Error),
}

impl From<ModeError> for SetModeFault {
    fn from(error: ModeError) -> Self {
        match error {
            ModeError::InvalidMode(why) => SetModeFault::Fdo(fdo::Error::InvalidArgs(why)),
            other => SetModeFault::Named(other.into()),
        }
    }
}

impl From<zbus::Error> for SetModeFault {
    fn from(error: zbus::Error) -> Self {
        SetModeFault::Named(ModeFault::ZBus(error))
    }
}

impl zbus::DBusError for SetModeFault {
    fn create_reply(&self, call: &zbus::message::Header<'_>) -> zbus::Result<zbus::message::Message> {
        match self {
            SetModeFault::Named(fault) => fault.create_reply(call),
            SetModeFault::Fdo(error) => error.create_reply(call),
        }
    }

    fn name(&self) -> zbus::names::ErrorName<'_> {
        match self {
            SetModeFault::Named(fault) => fault.name(),
            SetModeFault::Fdo(error) => error.name(),
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            SetModeFault::Named(fault) => fault.description(),
            SetModeFault::Fdo(error) => error.description(),
        }
    }
}

impl std::fmt::Display for SetModeFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", zbus::DBusError::name(self), zbus::DBusError::description(self).unwrap_or(""))
    }
}

impl std::error::Error for SetModeFault {}

/// `org.konedrive.Dev1`: development only.
pub struct Dev1 {
    service: Arc<AccountService>,
}

#[zbus::interface(name = "org.konedrive.Dev1")]
impl Dev1 {
    /// An access token of this account that can change nothing, whatever its mode (write
    /// design §10) — never the refresh token.
    async fn access_token(&self) -> std::result::Result<String, ModeFault> {
        match self.service.read_only_token().await {
            Ok(token) => Ok(token),
            Err(crate::token::AuthError::SignedOut) => Err(ModeFault::NotSignedIn("nobody is signed in".into())),
            Err(e) => Err(ModeFault::Failed(e.to_string())),
        }
    }

    /// The test-account harness's token, which can change files: refused `WritesNotAllowed`
    /// for an account the gate does not let through, and `ModeNotGranted` for one that is
    /// not read-write.
    async fn read_write_access_token(&self) -> std::result::Result<String, ModeFault> {
        self.service.read_write_token().await.map_err(ModeFault::from)
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
    if old.mode != new.mode {
        account.mode_changed(emitter).await?;
    }
    Ok(())
}
