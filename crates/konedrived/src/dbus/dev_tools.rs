use std::sync::Arc;

use konedrive_dbus::ACCOUNTS_PATH;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

use crate::daemon::manager::AccountManager;
use crate::dbus::accounts::Accounts;
use crate::dbus::fault::Fault;

/// `org.konedrive.DevTools` (`dbus/org.konedrive.DevTools.xml`), at
/// `/org/konedrive/Accounts`: only in a development build (the `dev-tools` feature), as
/// `TokenExport` is. A release build adds an account only by signing in.
pub struct DevTools {
    pub(crate) manager: Arc<AccountManager>,
}

#[zbus::interface(name = "org.konedrive.DevTools")]
impl DevTools {
    /// A signed-out account under `label`, which never has to sign in: for a folder that
    /// shows a local directory.
    #[zbus(out_args("account"))]
    async fn add_account(&self, label: &str, #[zbus(connection)] connection: &Connection) -> Result<OwnedObjectPath, Fault> {
        let account = self.manager.add(label, connection).await?;
        let accounts = connection.object_server().interface::<_, Accounts>(ACCOUNTS_PATH).await?;
        accounts.get().await.list_changed(accounts.signal_emitter()).await?;
        Ok(account.path.clone())
    }
}
