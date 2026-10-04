use konedrive_dbus::Refusal;
use zbus::message::{Header, Message};
use zbus::names::ErrorName;

use crate::account::{AccountError, ModeError};
use crate::daemon::manager::ManagerError;
use crate::sync::SyncError;

/// How a call is refused: under a D-Bus error *name* ([`Refusal`]) rather than a
/// sentence (asks for named errors on refusals the user can act
/// on), with the message beside it.
///
/// All of these used to collapse into `org.freedesktop.DBus.Error.Failed`
/// with the reason in the message, which leaves a client — the CLI, the
/// window, a script — nothing to branch on but English prose. "The file was
/// modified locally" and "the file is not downloaded" are exactly the two
/// refusals a named error is for: the user can do something about each, and
/// they are not the same something.
///
/// Every interface refuses through this one type: the folder's and `Files` under the
/// daemon's own names, `SetMode` and `TokenExport` under theirs
/// (`docs/design/writes.md` §11), and `Accounts` and `Account` under the bus's own
/// `InvalidArgs` and `Failed` for a label, a client id, a mode or `config.toml`.
#[derive(Debug)]
pub enum Fault {
    /// Anything zbus itself reports, passed through unchanged; it goes out under
    /// [`Refusal::Internal`].
    ZBus(zbus::Error),
    /// A refusal, and its message.
    Refused(Refusal, String),
}

impl Fault {
    pub(crate) fn refused(refusal: Refusal, message: impl Into<String>) -> Self {
        Fault::Refused(refusal, message.into())
    }
}

impl zbus::DBusError for Fault {
    fn create_reply(&self, call: &Header<'_>) -> zbus::Result<Message> {
        let name = self.name();
        match self {
            Fault::ZBus(error) => {
                let said = match error {
                    zbus::Error::MethodError(_, said, _) => said.clone(),
                    _ => None,
                };
                Message::error(call, name)?.build(&said.unwrap_or_else(|| error.to_string()))
            }
            Fault::Refused(_, message) => Message::error(call, name)?.build(message),
        }
    }

    fn name(&self) -> ErrorName<'_> {
        let refusal = match self {
            Fault::ZBus(_) => &Refusal::Internal,
            Fault::Refused(refusal, _) => refusal,
        };
        match refusal.known_name() {
            Some(name) => ErrorName::from_static_str_unchecked(name),
            // No code here makes a refusal this build does not know; one that is no name at
            // all goes out as having none of its own.
            None => ErrorName::try_from(refusal.name())
                .unwrap_or_else(|_| ErrorName::from_static_str_unchecked(Refusal::Failed.known_name().unwrap_or_default())),
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            Fault::ZBus(error) => error.description(),
            Fault::Refused(_, message) => Some(message),
        }
    }
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", zbus::DBusError::name(self), zbus::DBusError::description(self).unwrap_or("no description"))
    }
}

impl std::error::Error for Fault {}

impl From<zbus::Error> for Fault {
    fn from(error: zbus::Error) -> Self {
        Fault::ZBus(error)
    }
}

pub(crate) type Result<T> = std::result::Result<T, Fault>;

/// Every refusal keeps its own name; only the ones with nothing a caller
/// could act on differently fall through to `Failed` (Ruling: fail loudly
/// rather than report success this component cannot back up).
pub(crate) fn to_fault(error: SyncError) -> Fault {
    let message = error.to_string();
    let refusal = match error {
        SyncError::Overlaps(_) => Refusal::Overlaps,
        SyncError::NotEmpty | SyncError::ForeignFolder => Refusal::NotEmpty,
        SyncError::Unsupported(_) => Refusal::Unsupported,
        SyncError::InUse => Refusal::InUse,
        SyncError::NoRoot => Refusal::NoRoot,
        SyncError::NoHelper => Refusal::NoHelper,
        SyncError::NotManaged => Refusal::NotManaged,
        SyncError::NotHydrated => Refusal::NotHydrated,
        SyncError::ModifiedLocally => Refusal::ModifiedLocally,
        SyncError::OutsideRoot => Refusal::OutsideRoot,
        SyncError::AlreadyRegistered => Refusal::AlreadyRegistered,
        SyncError::NotSignedIn => Refusal::NotSignedIn,
        SyncError::NoSource => Refusal::NoSource,
        SyncError::NoConflict(_) => Refusal::NoConflict,
        SyncError::NotAllowed(_) => Refusal::NotAllowed,
        SyncError::NotUploaded(_) | SyncError::NotInOneDrive(_) => Refusal::NotUploaded,
        SyncError::Unreachable(_) => Refusal::Unreachable,
        SyncError::PendingUploads(_) => Refusal::PendingUploads,
        // As an error of zbus's own, so under [`Refusal::Internal`], with the bus's name for it
        // at the start of the message (`docs/limitations/D35.md`).
        SyncError::InvalidArgs(_) => return Fault::ZBus(zbus::Error::FDO(Box::new(zbus::fdo::Error::InvalidArgs(message)))),
        SyncError::NotUp(_) => Refusal::NotUp,
        SyncError::Helper(_)
        | SyncError::Config(_)
        | SyncError::Store(_)
        | SyncError::HeldBack(_)
        | SyncError::Removing
        | SyncError::Stopping
        | SyncError::Io(_) => Refusal::Failed,
    };
    Fault::Refused(refusal, message)
}

/// The named refusals of `SetMode` and `TokenExport` (`docs/design/writes.md` §11); a mode
/// that is not one is refused under the bus's own `InvalidArgs`, as `SetLabel` refuses a
/// label.
impl From<ModeError> for Fault {
    fn from(error: ModeError) -> Self {
        match error {
            // The development gate: the account's drive is not in `write_test_drive_ids`.
            ModeError::WritesNotAllowed(why) => Fault::Refused(Refusal::WritesNotAllowed, why),
            // The account's token does not carry `Files.ReadWrite`.
            ModeError::ModeNotGranted(why) => Fault::Refused(Refusal::ModeNotGranted, why),
            // Changes wait to be uploaded, and the switch to read-only was not forced.
            ModeError::PendingUploads(why) => Fault::Refused(Refusal::PendingUploads, why),
            ModeError::NotSignedIn(why) => Fault::Refused(Refusal::NotSignedIn, why),
            ModeError::InvalidMode(why) => Fault::Refused(Refusal::InvalidArgs, why),
            ModeError::Failed(why) => Fault::Refused(Refusal::Failed, why),
        }
    }
}

/// What `TokenExport.ReadWrite` refuses: [`ModeError`]'s names, but a mode that is not one —
/// which nothing there raises — under `Failed`, not the bus's `InvalidArgs`.
#[cfg(feature = "dev-tools")]
pub(crate) fn export_fault(error: ModeError) -> Fault {
    match error {
        ModeError::InvalidMode(why) => Fault::Refused(Refusal::Failed, why),
        other => other.into(),
    }
}

/// How `Accounts` refuses: under the folder's names for what an account's folder refused
/// (`NoHelper`, …) and `NoAccount`, and under the bus's own `InvalidArgs` and `Failed` for a
/// label, a client id or `config.toml`.
impl From<ManagerError> for Fault {
    fn from(error: ManagerError) -> Self {
        match error {
            ManagerError::InvalidArgs(why) => Fault::Refused(Refusal::InvalidArgs, why),
            ManagerError::Failed(why) => Fault::Refused(Refusal::BusFailed, why),
            ManagerError::NoAccount(path) => Fault::Refused(Refusal::NoAccount, format!("there is no account {path}")),
            ManagerError::Sync(error) => to_fault(error),
        }
    }
}

/// How `Account` refuses a sign-in, a sign-out and a label: under the bus's own names.
impl From<AccountError> for Fault {
    fn from(error: AccountError) -> Self {
        let message = error.to_string();
        match error {
            AccountError::InvalidClientId | AccountError::InvalidLabel(_) => Fault::Refused(Refusal::InvalidArgs, message),
            _ => Fault::Refused(Refusal::BusFailed, message),
        }
    }
}

#[cfg(test)]
mod tests;
