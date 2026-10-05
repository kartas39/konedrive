use std::sync::Arc;

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{interface, Connection};

use crate::dbus::fault::Fault;
use crate::daemon::manager::AccountManager;

/// `org.konedrive.Accounts` (`dbus/org.konedrive.Accounts.xml`).
pub struct Accounts {
    pub(crate) manager: Arc<AccountManager>,
}

#[interface(name = "org.konedrive.Accounts")]
impl Accounts {
    /// A new account by signing in: the sign-in's number and the URL to open. How it
    /// ends: [`sign_in_finished`](Self::sign_in_finished).
    #[zbus(out_args("sign_in", "url"))]
    async fn sign_in(&self, #[zbus(connection)] connection: &Connection) -> Result<(u32, String), Fault> {
        Ok(self.manager.sign_in(connection).await?)
    }

    /// Cancels the sign-in `sign_in`; a number that is not under way is ignored.
    async fn cancel_sign_in(&self, sign_in: u32) {
        self.manager.cancel_sign_in(sign_in).await;
    }

    /// How the sign-in `sign_in` ended (`konedrive_dbus::sign_in`), once for each: the
    /// account it made, or the one that has the drive already; `/` otherwise.
    #[zbus(signal)]
    pub(crate) async fn sign_in_finished(emitter: &SignalEmitter<'_>, sign_in: u32, outcome: &str, message: &str, account: ObjectPath<'_>) -> zbus::Result<()>;

    async fn remove(
        &self,
        account: ObjectPath<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), Fault> {
        self.manager.remove(&account, connection).await?;
        self.list_changed(&emitter).await?;
        Ok(())
    }

    async fn set_client_id(&self, id: &str, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<(), Fault> {
        self.manager.set_client_id(id).await?;
        self.client_id_changed(&emitter).await?;
        Ok(())
    }

    /// Whether every account holds back on a metered connection; written to `config.toml`.
    async fn set_pause_on_metered(&self, on: bool, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<(), Fault> {
        self.manager.set_pause_on_metered(on).await?;
        self.pause_on_metered_changed(&emitter).await?;
        Ok(())
    }

    /// What every account does on battery: `sync`, `power-saver` or `pause`; refused
    /// `InvalidArgs` otherwise. Written to `config.toml`.
    async fn set_on_battery(&self, choice: &str, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<(), Fault> {
        self.manager.set_on_battery(choice).await?;
        self.on_battery_changed(&emitter).await?;
        Ok(())
    }

    #[zbus(property)]
    async fn list(&self) -> Vec<OwnedObjectPath> {
        self.manager.paths()
    }

    #[zbus(property)]
    async fn client_id(&self) -> String {
        self.manager.config.client_id()
    }

    #[zbus(property)]
    async fn pause_on_metered(&self) -> bool {
        self.manager.hold_settings().pause_on_metered
    }

    #[zbus(property)]
    async fn on_battery(&self) -> String {
        self.manager.hold_settings().on_battery.as_str().to_owned()
    }

    #[zbus(property)]
    async fn helper_state(&self) -> String {
        self.manager.hub().state().as_str().to_owned()
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.manager.config.last_error()
    }

    /// This build's version (`konedrive_dbus::version`).
    #[zbus(property(emits_changed_signal = "const"))]
    async fn version(&self) -> String {
        konedrive_dbus::version::VERSION.to_owned()
    }

    /// The full hash of this build's commit, or `unknown`.
    #[zbus(property(emits_changed_signal = "const"))]
    async fn commit(&self) -> String {
        konedrive_dbus::version::COMMIT.to_owned()
    }
}
