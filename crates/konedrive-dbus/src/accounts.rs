//! The client proxies of the multiple-accounts contract (definitions:
//! `dbus/org.konedrive.*.xml`, one file per interface).
//!
//! [`AccountsProxy`] and [`FilesProxy`] have a default path,
//! [`ACCOUNTS_PATH`](crate::ACCOUNTS_PATH). The proxies of an account's object —
//! [`AccountProxy`], [`FolderProxy`], [`TransfersProxy`], [`UploadQueueProxy`],
//! [`ConflictsProxy`], [`LocalScanProxy`], [`ActivityLogProxy`] and
//! [`TokenExportProxy`] — have none: each is built with one account's path, an
//! entry of [`AccountsProxy::list`] or [`account_path`](crate::account_path).
//! [`FolderProxies`] builds the six of an account's folder at once:
//!
//! ```no_run
//! # async fn example(conn: &zbus::Connection) -> zbus::Result<()> {
//! use konedrive_dbus::accounts::{AccountsProxy, FolderProxy};
//!
//! let manager = AccountsProxy::new(conn).await?;
//! for path in manager.list().await? {
//!     let folder = FolderProxy::new(conn, path).await?;
//!     println!("{}", folder.path().await?);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! The same object as `Accounts` serves `org.freedesktop.DBus.ObjectManager`:
//! `zbus::fdo::ObjectManagerProxy` at [`ACCOUNTS_PATH`](crate::ACCOUNTS_PATH).

use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::rows::{Change, Conflict, Event, Freed, FreedSpace, KeptBack, KeptBackFiles, KeptBackReason, Transfer};

/// `/org/konedrive/Accounts`: the accounts of this user.
#[zbus::proxy(
    interface = "org.konedrive.Accounts",
    default_service = "org.konedrive.Daemon",
    default_path = "/org/konedrive/Accounts",
    gen_blocking = false
)]
pub trait Accounts {
    /// Adds a signed-out, read-only account with no folder; its object path.
    /// Refused `InvalidArgs` for a label that breaks the rules
    /// (`dbus/org.konedrive.Accounts.xml`).
    fn add(&self, label: &str) -> zbus::Result<OwnedObjectPath>;
    /// Forgets the account's folder as `Folder.Unregister` does, deletes
    /// its token, cache and tree store, and removes the object. Refused
    /// `NoAccount` for a path that names no account.
    fn remove(&self, account: &ObjectPath<'_>) -> zbus::Result<()>;
    /// The Entra application every account signs in with.
    fn set_client_id(&self, id: &str) -> zbus::Result<()>;
    /// Whether every account holds back on a metered connection; written to `config.toml`.
    fn set_pause_on_metered(&self, on: bool) -> zbus::Result<()>;
    /// What every account does on battery: `sync`, `power-saver` or `pause`; refused
    /// `org.freedesktop.DBus.Error.InvalidArgs` otherwise.
    fn set_on_battery(&self, choice: &str) -> zbus::Result<()>;

    /// Every account's object path, in the order the accounts were added.
    #[zbus(property)]
    fn list(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(property)]
    fn client_id(&self) -> zbus::Result<String>;
    /// Whether every account holds back on a metered connection.
    #[zbus(property)]
    fn pause_on_metered(&self) -> zbus::Result<bool>;
    /// What every account does on battery: `sync`, `power-saver` or `pause`.
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<String>;
    /// The privileged helper as the daemon sees it, as [`HelperState`](crate::HelperState)
    /// spells it.
    #[zbus(property)]
    fn helper_state(&self) -> zbus::Result<String>;
    /// Trouble that belongs to no account; empty when there is none.
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;
    /// The daemon's version, as [`version::VERSION`](crate::version::VERSION) is this build's.
    #[zbus(property(emits_changed_signal = "const"))]
    fn version(&self) -> zbus::Result<String>;
    /// The full hash of the daemon's commit, or `unknown`.
    #[zbus(property(emits_changed_signal = "const"))]
    fn commit(&self) -> zbus::Result<String>;
}

/// `/org/konedrive/Accounts`: per-file calls, each routed by path to the
/// account whose folder holds it. A path in no account's folder is refused
/// `OutsideRoot`; `item_state` answers `not-managed`.
#[zbus::proxy(
    interface = "org.konedrive.Files",
    default_service = "org.konedrive.Daemon",
    default_path = "/org/konedrive/Accounts",
    gen_blocking = false
)]
pub trait Files {
    fn hydrate(&self, path: &str) -> zbus::Result<()>;
    fn dehydrate(&self, path: &str) -> zbus::Result<()>;
    fn item_state(&self, path: &str) -> zbus::Result<String>;
    /// "Always keep on this device" for each path; how many files were
    /// queued for download.
    fn pin(&self, paths: &[&str]) -> zbus::Result<u32>;
    /// Unchecking "Always keep on this device": each path's own pin is
    /// removed, files stay downloaded; how many pins were removed. Refused
    /// `NotAllowed` for a path a folder above it pins.
    fn unpin(&self, paths: &[&str]) -> zbus::Result<u32>;
    /// "Free up space" for each path, its own pin taken off first. Refused `NotAllowed` for
    /// a path a folder above it pins.
    fn free_up(&self, paths: &[&str]) -> zbus::Result<Freed>;
    /// The address of the page OneDrive's web interface has for the file or
    /// folder at `path`, or for the drive's root when `path` is an account's
    /// folder itself. Asks OneDrive each time and changes nothing. Refused
    /// `NotUploaded` for an item OneDrive does not have yet, `NotSignedIn`,
    /// and `Unreachable` when OneDrive does not answer.
    fn web_url(&self, path: &str) -> zbus::Result<String>;
}

/// `/org/konedrive/Accounts/<id>`: one Microsoft account.
#[zbus::proxy(
    interface = "org.konedrive.Account",
    default_service = "org.konedrive.Daemon",
    gen_blocking = false
)]
pub trait Account {
    fn begin_sign_in(&self) -> zbus::Result<String>;
    fn cancel_sign_in(&self) -> zbus::Result<()>;
    fn sign_out(&self) -> zbus::Result<()>;
    fn refresh_info(&self) -> zbus::Result<()>;
    /// Same rules as [`AccountsProxy::add`].
    fn set_label(&self, label: &str) -> zbus::Result<()>;
    /// Switches the mode to `read-only` or `read-write`; the URL of the
    /// sign-in the switch needs, empty when it needs none. Refused
    /// `WritesNotAllowed` (the development gate), `NotSignedIn`, or
    /// `PendingUploads` unless `force` (`dbus/org.konedrive.Account.xml`).
    fn set_mode(&self, mode: &str, force: bool) -> zbus::Result<String>;

    /// The last element of the object path.
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn label(&self) -> zbus::Result<String>;
    /// The mode the account runs in: `read-only` or `read-write`.
    #[zbus(property)]
    fn mode(&self) -> zbus::Result<String>;
    /// `signed-out`, `signing-in` or `signed-in`.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn display_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn email(&self) -> zbus::Result<String>;
    /// The account's one quota, whoever read it: bytes used and in all, Graph's
    /// `quota.remaining` less what went up since, and `quota.state`; 0 and empty until read.
    #[zbus(property)]
    fn quota_used(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn quota_total(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn quota_remaining(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn quota_state(&self) -> zbus::Result<String>;
}

/// `/org/konedrive/Accounts/<id>`: that account's folder.
#[zbus::proxy(
    interface = "org.konedrive.Folder",
    default_service = "org.konedrive.Daemon",
    gen_blocking = false
)]
pub trait Folder {
    /// Refused `Overlaps` for a folder that is, is inside, or contains
    /// another account's folder.
    fn register(&self, path: &str) -> zbus::Result<()>;
    fn register_without_interception(&self, path: &str) -> zbus::Result<()>;
    fn unregister(&self) -> zbus::Result<()>;
    fn populate_from_directory(&self, source_dir: &str) -> zbus::Result<u64>;
    fn refresh(&self) -> zbus::Result<()>;
    fn skipped(&self) -> zbus::Result<Vec<(String, String)>>;
    fn free_up_space(&self) -> zbus::Result<FreedSpace>;
    /// Nothing is uploaded, and OneDrive is not asked, for `seconds` — or
    /// until [`resume`](Self::resume) when 0.
    fn pause(&self, seconds: u32) -> zbus::Result<()>;
    fn resume(&self) -> zbus::Result<()>;
    /// Refused `org.freedesktop.DBus.Error.InvalidArgs` for a pattern that
    /// cannot match a name.
    fn set_ignore_patterns(&self, patterns: &[&str]) -> zbus::Result<()>;
    /// Written to `config.toml`; refused `Unsupported` for a folder not connected to OneDrive.
    fn set_thumbnails(&self, on: bool) -> zbus::Result<()>;
    /// Lifts this account's automatic hold until a source or the hold's settings
    /// ([`AccountsProxy::pause_on_metered`], [`AccountsProxy::on_battery`]) change.
    fn sync_anyway(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn path(&self) -> zbus::Result<String>;
    /// `none`, `listing`, `ready`, `no-interception` or `error`.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;
    /// `onedrive`, `local`, or empty for none.
    #[zbus(property)]
    fn source(&self) -> zbus::Result<String>;
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
    /// Files and folders with an "Always keep on this device" pin of their own.
    #[zbus(property)]
    fn pinned_count(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn ignore_patterns(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn paused(&self) -> zbus::Result<bool>;
    /// Unix seconds when the pause ends by itself; 0 until resumed, or not paused.
    #[zbus(property)]
    fn paused_until(&self) -> zbus::Result<i64>;
    /// Whether Graph's thumbnails of images and videos are fetched.
    #[zbus(property)]
    fn thumbnails(&self) -> zbus::Result<bool>;
    /// Why the account holds back by itself: `metered`, `on-battery`, `power-saver`, or empty.
    #[zbus(property)]
    fn held_back(&self) -> zbus::Result<String>;
    /// How changes made in OneDrive arrive: `connected`, `connecting` or `off`.
    #[zbus(property)]
    fn live_changes(&self) -> zbus::Result<String>;
}

/// `/org/konedrive/Accounts/<id>`: what that account's folder moves now.
#[zbus::proxy(
    interface = "org.konedrive.Transfers",
    default_service = "org.konedrive.Daemon",
    gen_blocking = false
)]
pub trait Transfers {
    /// Downloads under way.
    #[zbus(property)]
    fn downloads(&self) -> zbus::Result<Vec<Transfer>>;
    /// Uploads under way.
    #[zbus(property)]
    fn uploads(&self) -> zbus::Result<Vec<Transfer>>;
    /// Bytes a second downloaded and uploaded, the average of the last 3 s.
    #[zbus(property)]
    fn download_speed(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn upload_speed(&self) -> zbus::Result<u64>;
    /// Files downloading and uploading now: the entries of `downloads` and `uploads`.
    #[zbus(property)]
    fn active_downloads(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn active_uploads(&self) -> zbus::Result<u32>;
    /// Every slot of the pool held now, the opens' reserve included (may be above the size).
    #[zbus(property)]
    fn pool_in_use(&self) -> zbus::Result<u32>;
    /// The account's transfer pool now, and its ceiling.
    #[zbus(property)]
    fn pool_size(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn pool_ceiling(&self) -> zbus::Result<u32>;
    /// The large files (100 MiB and up) the sync moves now, each once; files being opened
    /// left out.
    #[zbus(property)]
    fn large_files(&self) -> zbus::Result<u32>;
    /// The streams of large sync transfers under way now, and how many may run at once.
    #[zbus(property)]
    fn large_streams(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn large_stream_limit(&self) -> zbus::Result<u32>;
    /// Seconds left of OneDrive's `Retry-After` wait; 0 when there is none.
    #[zbus(property)]
    fn retry_after(&self) -> zbus::Result<u32>;
    /// The queue totals (issue #16): files left to download and changes left to upload,
    /// their bytes, the bytes done in this run, and the seconds left (0: unknown).
    #[zbus(property)]
    fn download_left_count(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn download_left_bytes(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn download_done_bytes(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn download_time_left(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn upload_left_count(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn upload_left_bytes(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn upload_done_bytes(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn upload_time_left(&self) -> zbus::Result<u32>;
}

/// `/org/konedrive/Accounts/<id>`: the changes made in that account's folder that wait
/// to be uploaded, and what is kept back.
#[zbus::proxy(
    interface = "org.konedrive.UploadQueue",
    default_service = "org.konedrive.Daemon",
    gen_blocking = false
)]
pub trait UploadQueue {
    /// The changes waiting to be uploaded, oldest first, at most `limit` (0 for all).
    fn changes(&self, limit: u32) -> zbus::Result<Vec<Change>>;
    /// The held removals go ahead; how many.
    fn confirm_deletes(&self) -> zbus::Result<u32>;
    /// The held removals are dropped and their items placed again; how many.
    fn restore_deletes(&self) -> zbus::Result<u32>;
    /// What stays on this computer and why.
    fn not_uploaded(&self) -> zbus::Result<Vec<KeptBack>>;
    /// What is kept back, one row per reason, by group: one-action, per-file, never,
    /// waiting, in that order.
    fn not_uploaded_summary(&self) -> zbus::Result<Vec<KeptBackReason>>;
    /// The files kept back for `reason` (as the summary names it), at most
    /// `limit` (0 for all), each with its reason as stored; and how many there are.
    fn not_uploaded_files(&self, reason: &str, limit: u32) -> zbus::Result<KeptBackFiles>;

    /// Changes waiting to be uploaded, and the size of what they send.
    #[zbus(property)]
    fn pending_count(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn pending_bytes(&self) -> zbus::Result<u64>;
    /// Changes that need the user before they can go up.
    #[zbus(property)]
    fn blocked_count(&self) -> zbus::Result<u32>;
    /// Removals held by the mass-delete guard: `confirm_deletes` or
    /// `restore_deletes` decides them.
    #[zbus(property)]
    fn held_count(&self) -> zbus::Result<u32>;
    /// OneDrive is full: no content goes up until a quota read finds space.
    #[zbus(property)]
    fn quota_full(&self) -> zbus::Result<bool>;
    /// While full: the changes that send content, and their size.
    #[zbus(property)]
    fn quota_waiting_count(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn quota_waiting_bytes(&self) -> zbus::Result<u64>;
    /// Files refused as too big for the space left.
    #[zbus(property)]
    fn too_big_count(&self) -> zbus::Result<u32>;
}

/// `/org/konedrive/Accounts/<id>`: the local versions that account's folder kept.
#[zbus::proxy(
    interface = "org.konedrive.Conflicts",
    default_service = "org.konedrive.Daemon",
    gen_blocking = false
)]
pub trait Conflicts {
    fn list(&self) -> zbus::Result<Vec<Conflict>>;
    fn dismiss(&self, rescued_path: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn count(&self) -> zbus::Result<u32>;
    /// What a copy of a file changed on both sides is named after.
    #[zbus(property)]
    fn machine_name(&self) -> zbus::Result<String>;
}

/// `/org/konedrive/Accounts/<id>`: the Full local scan of that account's folder.
#[zbus::proxy(
    interface = "org.konedrive.LocalScan",
    default_service = "org.konedrive.Daemon",
    gen_blocking = false
)]
pub trait LocalScan {
    /// `running`, `idle`, or `none` for a read-only folder.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    /// Why it runs: start, read-write, helper-back, overflow, ignore-list, periodic.
    #[zbus(property)]
    fn reason(&self) -> zbus::Result<String>;
    /// Unix seconds when it started.
    #[zbus(property)]
    fn started(&self) -> zbus::Result<i64>;
    /// Directories and files seen so far.
    #[zbus(property)]
    fn directories(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn files(&self) -> zbus::Result<u64>;
    /// About how many items it will see (the base's count, not the disk's).
    #[zbus(property)]
    fn expected(&self) -> zbus::Result<u64>;
    /// Unix seconds when the last scan finished (0: none yet), and how long it took.
    #[zbus(property)]
    fn finished(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn took(&self) -> zbus::Result<u32>;
}

/// `/org/konedrive/Accounts/<id>`: what happened in that account's folder.
#[zbus::proxy(
    interface = "org.konedrive.ActivityLog",
    default_service = "org.konedrive.Daemon",
    gen_blocking = false
)]
pub trait ActivityLog {
    /// The last `limit` events, newest first.
    fn recent(&self, limit: u32) -> zbus::Result<Vec<Event>>;

    #[zbus(signal)]
    fn added(&self, time: i64, kind: String, path: String, detail: String) -> zbus::Result<()>;
}

/// The six proxies of one account's folder, on one path.
#[derive(Clone)]
pub struct FolderProxies<'a> {
    pub folder: FolderProxy<'a>,
    pub transfers: TransfersProxy<'a>,
    pub queue: UploadQueueProxy<'a>,
    pub conflicts: ConflictsProxy<'a>,
    pub scan: LocalScanProxy<'a>,
    pub activity: ActivityLogProxy<'a>,
}

impl FolderProxies<'static> {
    /// The proxies of the folder of the account at `path`, caching properties as zbus does.
    pub async fn new(conn: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<Self> {
        Self::build(conn, path, zbus::proxy::CacheProperties::Lazily).await
    }

    /// As [`new`](Self::new), each property read from the daemon every time.
    pub async fn uncached(conn: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<Self> {
        Self::build(conn, path, zbus::proxy::CacheProperties::No).await
    }

    async fn build(
        conn: &zbus::Connection,
        path: OwnedObjectPath,
        cache: zbus::proxy::CacheProperties,
    ) -> zbus::Result<Self> {
        Ok(Self {
            folder: FolderProxy::builder(conn).path(path.clone())?.cache_properties(cache).build().await?,
            transfers: TransfersProxy::builder(conn).path(path.clone())?.cache_properties(cache).build().await?,
            queue: UploadQueueProxy::builder(conn).path(path.clone())?.cache_properties(cache).build().await?,
            conflicts: ConflictsProxy::builder(conn).path(path.clone())?.cache_properties(cache).build().await?,
            scan: LocalScanProxy::builder(conn).path(path.clone())?.cache_properties(cache).build().await?,
            activity: ActivityLogProxy::builder(conn).path(path)?.cache_properties(cache).build().await?,
        })
    }
}

/// `/org/konedrive/Accounts/<id>`: development only.
#[zbus::proxy(
    interface = "org.konedrive.TokenExport",
    default_service = "org.konedrive.Daemon",
    gen_blocking = false
)]
pub trait TokenExport {
    /// An access token of this account that can change nothing, whatever
    /// its mode. Refused `NotSignedIn` when there is none.
    fn read_only(&self) -> zbus::Result<String>;
    /// The test-account harness's token, which can change files. Refused
    /// `WritesNotAllowed` for an account the development gate does not let
    /// through, `ModeNotGranted` for one that is not read-write.
    fn read_write(&self) -> zbus::Result<String>;
}

#[cfg(test)]
mod tests;
