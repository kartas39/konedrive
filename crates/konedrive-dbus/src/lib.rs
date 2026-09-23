//! D-Bus names and the client proxy for `org.konedrive.Account1`
//! (definition: `dbus/org.konedrive.Account1.xml`).

pub mod testing;

pub const SERVICE_NAME: &str = "org.konedrive.Daemon";
pub const OBJECT_PATH: &str = "/org/konedrive/Daemon";
pub const INTERFACE_NAME: &str = "org.konedrive.Account1";
pub const SYNC_INTERFACE_NAME: &str = "org.konedrive.Sync1";

/// The prefix every named `Sync1` refusal carries (spec §3.1). A client that
/// wants to tell "the file was modified locally" from "the file is not
/// downloaded" matches on `<prefix>.ModifiedLocally` and
/// `<prefix>.NotHydrated` rather than on the message.
pub const ERROR_PREFIX: &str = "org.konedrive.Error";

/// The D-Bus error name a failed call carries, if it carries one.
///
/// `zbus::Error::MethodError` keeps the name and the message apart; matching
/// on the message is what a named error exists to replace.
pub fn error_name(error: &zbus::Error) -> Option<&str> {
    match error {
        zbus::Error::MethodError(name, _, _) => Some(name.as_str()),
        _ => None,
    }
}

#[zbus::proxy(
    interface = "org.konedrive.Account1",
    default_service = "org.konedrive.Daemon",
    default_path = "/org/konedrive/Daemon",
    gen_blocking = false
)]
pub trait Account1 {
    fn set_client_id(&self, id: &str) -> zbus::Result<()>;
    fn begin_sign_in(&self) -> zbus::Result<String>;
    fn cancel_sign_in(&self) -> zbus::Result<()>;
    fn sign_out(&self) -> zbus::Result<()>;
    fn refresh_account_info(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn client_id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn display_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn email(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn quota_used(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn quota_total(&self) -> zbus::Result<u64>;
}

#[zbus::proxy(
    interface = "org.konedrive.Sync1",
    default_service = "org.konedrive.Daemon",
    default_path = "/org/konedrive/Daemon",
    gen_blocking = false
)]
pub trait Sync1 {
    fn register_root(&self, path: &str) -> zbus::Result<()>;
    fn register_root_without_interception(&self, path: &str) -> zbus::Result<()>;
    fn unregister_root(&self) -> zbus::Result<()>;
    fn populate_from_directory(&self, source_dir: &str) -> zbus::Result<u64>;
    fn hydrate(&self, path: &str) -> zbus::Result<()>;
    fn dehydrate(&self, path: &str) -> zbus::Result<()>;
    fn item_state(&self, path: &str) -> zbus::Result<String>;

    #[zbus(property)]
    fn root_path(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn root_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;
}
