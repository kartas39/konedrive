use zbus::{DBusError, fdo};

use crate::sync::SyncError;
use crate::account::ModeError;
use crate::daemon::manager::ManagerError;

/// Every way the folder's interfaces (and `Files`) can refuse, as a D-Bus error *name* rather than a
/// sentence (asks for named errors on refusals the user can act
/// on).
///
/// All of these used to collapse into `org.freedesktop.DBus.Error.Failed`
/// with the reason in the message, which leaves a client — the CLI, the
/// window, a script — nothing to branch on but English prose. "The file was
/// modified locally" and "the file is not downloaded" are exactly the two
/// refusals a named error is for: the user can do something about each, and
/// they are not the same something.
#[derive(Debug, DBusError)]
#[zbus(prefix = "org.konedrive.Error")]
pub enum SyncFault {
    /// Anything zbus itself reports, passed through unchanged.
    #[zbus(error)]
    ZBus(zbus::Error),
    NotEmpty(String),
    Unsupported(String),
    InUse(String),
    NoRoot(String),
    NoHelper(String),
    NotManaged(String),
    NotHydrated(String),
    ModifiedLocally(String),
    OutsideRoot(String),
    AlreadyRegistered(String),
    NotSignedIn(String),
    NoSource(String),
    /// `Conflicts.Dismiss` of a path that names no conflict; the message
    /// names the path.
    NoConflict(String),
    /// A free-up of something "Always keep on this device" keeps here:
    /// `FreeUp` of a path a folder above it pins, or `Dehydrate` of a pinned
    /// file. The message is "<path> is pinned by <folder>: unpin it first".
    NotAllowed(String),
    /// `Register` or `RegisterWithoutInterception` of a folder that
    /// is, is inside, or contains another account's folder; the message
    /// names that account's label.
    Overlaps(String),
    /// `Accounts.Remove` of a path that names no account.
    NoAccount(String),
    /// A free-up of a file whose change waits to be uploaded (write design
    /// §3.8): freeing it up would lose that change. The message names it.
    /// Also `Files.WebUrl` of an item OneDrive does not have yet.
    NotUploaded(String),
    /// `Unregister`, or `Accounts.Remove`, while changes wait to be uploaded: the folder's
    /// record holding them would go. The message says how many.
    PendingUploads(String),
    /// OneDrive did not answer a question that needs it now (`Files.WebUrl`):
    /// no network, or Graph kept refusing. The message says which.
    Unreachable(String),
    /// Everything with no name of its own: an I/O failure, mostly.
    Failed(String),
}

pub(crate) type Result<T> = std::result::Result<T, SyncFault>;

/// Every refusal keeps its own name; only the ones with nothing a caller
/// could act on differently fall through to `Failed` (Ruling: fail loudly
/// rather than report success this component cannot back up).
pub(crate) fn to_fault(error: SyncError) -> SyncFault {
    let message = error.to_string();
    match error {
        SyncError::Overlaps(_) => SyncFault::Overlaps(message),
        SyncError::NotEmpty | SyncError::ForeignFolder => SyncFault::NotEmpty(message),
        SyncError::Unsupported(_) => SyncFault::Unsupported(message),
        SyncError::InUse => SyncFault::InUse(message),
        SyncError::NoRoot => SyncFault::NoRoot(message),
        SyncError::NoHelper => SyncFault::NoHelper(message),
        SyncError::NotManaged => SyncFault::NotManaged(message),
        SyncError::NotHydrated => SyncFault::NotHydrated(message),
        SyncError::ModifiedLocally => SyncFault::ModifiedLocally(message),
        SyncError::OutsideRoot => SyncFault::OutsideRoot(message),
        SyncError::AlreadyRegistered => SyncFault::AlreadyRegistered(message),
        SyncError::NotSignedIn => SyncFault::NotSignedIn(message),
        SyncError::NoSource => SyncFault::NoSource(message),
        SyncError::NoConflict(_) => SyncFault::NoConflict(message),
        SyncError::NotAllowed(_) => SyncFault::NotAllowed(message),
        SyncError::NotUploaded(_) | SyncError::NotInOneDrive(_) => SyncFault::NotUploaded(message),
        SyncError::Unreachable(_) => SyncFault::Unreachable(message),
        SyncError::PendingUploads(_) => SyncFault::PendingUploads(message),
        SyncError::InvalidArgs(_) => SyncFault::ZBus(zbus::Error::FDO(Box::new(zbus::fdo::Error::InvalidArgs(message)))),
        SyncError::Io(_) => SyncFault::Failed(message),
    }
}

/// The named refusals of `SetMode` and `TokenExport` (`docs/design/writes.md` §11).
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

/// How `Accounts` refuses: under the folder's names for what an account's folder refused
/// (`NoHelper`, …) and `NoAccount`, and under the bus's own `InvalidArgs` and `Failed` for a
/// label, a client id or `config.toml`.
#[derive(Debug)]
pub enum ManagerFault {
    Sync(SyncFault),
    Fdo(fdo::Error),
}

impl DBusError for ManagerFault {
    fn create_reply(&self, call: &zbus::message::Header<'_>) -> zbus::Result<zbus::message::Message> {
        match self {
            ManagerFault::Sync(fault) => fault.create_reply(call),
            ManagerFault::Fdo(error) => error.create_reply(call),
        }
    }

    fn name(&self) -> zbus::names::ErrorName<'_> {
        match self {
            ManagerFault::Sync(fault) => fault.name(),
            ManagerFault::Fdo(error) => error.name(),
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            ManagerFault::Sync(fault) => fault.description(),
            ManagerFault::Fdo(error) => error.description(),
        }
    }
}

impl std::fmt::Display for ManagerFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name(), self.description().unwrap_or(""))
    }
}

impl std::error::Error for ManagerFault {}

impl From<zbus::Error> for ManagerFault {
    fn from(error: zbus::Error) -> Self {
        ManagerFault::Sync(SyncFault::ZBus(error))
    }
}

impl From<ManagerError> for ManagerFault {
    fn from(error: ManagerError) -> Self {
        match error {
            ManagerError::InvalidArgs(why) => ManagerFault::Fdo(fdo::Error::InvalidArgs(why)),
            ManagerError::Failed(why) => ManagerFault::Fdo(fdo::Error::Failed(why)),
            ManagerError::NoAccount(path) => ManagerFault::Sync(SyncFault::NoAccount(format!("there is no account {path}"))),
            ManagerError::Sync(error) => ManagerFault::Sync(to_fault(error)),
        }
    }
}
