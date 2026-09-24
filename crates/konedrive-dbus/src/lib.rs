//! D-Bus names and the client proxy for `org.konedrive.Account1`
//! (definition: `dbus/org.konedrive.Account1.xml`).

pub mod testing;

pub const SERVICE_NAME: &str = "org.konedrive.Daemon";
pub const OBJECT_PATH: &str = "/org/konedrive/Daemon";
pub const INTERFACE_NAME: &str = "org.konedrive.Account1";
pub const SYNC_INTERFACE_NAME: &str = "org.konedrive.Sync1";

/// The prefix every named `Sync1` refusal carries. A client that
/// wants to tell "the file was modified locally" from "the file is not
/// downloaded" matches on `<prefix>.ModifiedLocally` and
/// `<prefix>.NotHydrated` rather than on the message.
pub const ERROR_PREFIX: &str = "org.konedrive.Error";

/// What to tell a person about the helper in each `Sync1.HelperState` that
/// is not `connected`: what it means and how to start it. One wording for
/// the daemon's `LastError` and the CLI's `Helper:` line alike. `None` for
/// `connected`, and for anything this build does not know.
pub fn helper_advice(state: &str) -> Option<&'static str> {
    match state {
        "not-installed" => Some(
            "the konedrive helper is not installed: files are not kept in step and do not \
             download when opened. Install it: sudo scripts/install-helper.sh (see README)",
        ),
        "stopped" => Some(
            "the konedrive helper is not running: start it with `sudo systemctl start konedrive-helper`",
        ),
        "failed" => Some("the konedrive helper failed: see `systemctl status konedrive-helper`"),
        "unknown" => Some("the konedrive helper is not connected"),
        _ => None,
    }
}

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
    fn refresh(&self) -> zbus::Result<()>;
    fn skipped(&self) -> zbus::Result<Vec<(String, String)>>;
    /// (unix time, kind, full path, detail), newest first.
    fn recent_activity(&self, limit: u32) -> zbus::Result<Vec<(i64, String, String, String)>>;
    /// (unix time, original full path, full path it was moved to).
    fn conflicts(&self) -> zbus::Result<Vec<(i64, String, String)>>;
    fn dismiss_conflict(&self, rescued_path: &str) -> zbus::Result<()>;
    /// (files freed, bytes freed, files kept because they were in use).
    fn free_up_space(&self) -> zbus::Result<(u32, u64, u32)>;
    /// "Always keep on this device" for each path; how many files were
    /// queued for download.
    fn pin(&self, paths: &[&str]) -> zbus::Result<u32>;
    /// Unchecking "Always keep on this device": each path's own pin comes
    /// off, and its files stay; how many pins came off. Refused `NotAllowed`
    /// for a path a folder above it pins.
    fn unpin(&self, paths: &[&str]) -> zbus::Result<u32>;
    /// "Free up space" for each path, its own pin taken off first: (files
    /// freed, bytes freed, files kept because they were in use or changed
    /// here, downloaded files kept by a pin below). Refused `NotAllowed` for
    /// a path a folder above it pins.
    fn free_up(&self, paths: &[&str]) -> zbus::Result<(u32, u64, u32, u32)>;

    #[zbus(signal)]
    fn activity_added(&self, time: i64, kind: String, path: String, detail: String) -> zbus::Result<()>;

    #[zbus(property)]
    fn root_path(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn root_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn root_source(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn items_listed(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn items_placed(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn skipped_count(&self) -> zbus::Result<u64>;
    /// Unix seconds of the last successful check with OneDrive; 0 = never.
    #[zbus(property)]
    fn last_checked(&self) -> zbus::Result<i64>;
    /// What the folder's files take on disk (`st_blocks * 512`).
    #[zbus(property)]
    fn local_bytes(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn conflict_count(&self) -> zbus::Result<u32>;
    /// Files and folders with an "Always keep on this device" pin of their own.
    #[zbus(property)]
    fn pinned_count(&self) -> zbus::Result<u32>;
    /// The privileged helper as the daemon sees it: `connected`,
    /// `not-installed`, `stopped`, `failed` or `unknown` ([`helper_advice`]).
    #[zbus(property)]
    fn helper_state(&self) -> zbus::Result<String>;
    /// Downloads under way: (full path, bytes done, bytes total).
    #[zbus(property)]
    fn transfers(&self) -> zbus::Result<Vec<(String, u64, u64)>>;
}

pub const DEV_INTERFACE_NAME: &str = "org.konedrive.Dev1";

#[zbus::proxy(
    interface = "org.konedrive.Dev1",
    default_service = "org.konedrive.Daemon",
    default_path = "/org/konedrive/Daemon",
    gen_blocking = false
)]
pub trait Dev1 {
    fn access_token(&self) -> zbus::Result<String>;
}
