//! The name a refused call carries: [`Refusal`].
//!
//! A call the daemon refuses is answered with a D-Bus error: a **name** and a message. The
//! name says which refusal it is, and is what a client decides by; the message is for a
//! person. The names are the contract with the clients (`konedrivectl`, the window's and
//! Dolphin's tables are keyed by them): [`Refusal::name`] is the one place they are written
//! in Rust. The daemon answers through it, and `konedrivectl` reads an answer into it
//! ([`Refusal::from_error`]).

use std::fmt;

use crate::error_name;

/// Every name a call to the daemon is refused under.
///
/// The first twenty-three are the daemon's own, under [`ERROR_PREFIX`](crate::ERROR_PREFIX);
/// then the bus's names the daemon and its object server answer with; and a name this build
/// does not know, kept as it came ([`Refusal::Other`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// A folder to register that is not empty, or that was another account's.
    NotEmpty,
    /// What the folder or its filesystem cannot do; the message says which.
    Unsupported,
    /// The file is open in another program.
    InUse,
    /// No folder is registered.
    NoRoot,
    /// The helper is not connected, and the call needs it.
    NoHelper,
    /// A file of the user's own, not a OneDrive file.
    NotManaged,
    /// The file is not downloaded.
    NotHydrated,
    /// The file was changed here and is not uploaded.
    ModifiedLocally,
    /// A path outside the folder, or one that is no plain file.
    OutsideRoot,
    /// The account has a folder already.
    AlreadyRegistered,
    /// Nobody is signed in.
    NotSignedIn,
    /// A folder filled from a directory that has no directory yet.
    NoSource,
    /// `Conflicts.Dismiss` of a path that names no conflict.
    NoConflict,
    /// A free-up of something a pin keeps on this device; the message names the path and
    /// what pins it.
    NotAllowed,
    /// A folder that is, is inside, or contains another account's folder.
    Overlaps,
    /// A path that names no account.
    NoAccount,
    /// A free-up of a file whose change waits to be uploaded, or the page of an item
    /// OneDrive does not have yet.
    NotUploaded,
    /// Changes wait to be uploaded, and the call would drop them.
    PendingUploads,
    /// OneDrive did not answer a question that needs it now.
    Unreachable,
    /// A folder is recorded and not up, or its sync is not running; the message says why.
    NotUp,
    /// Everything with no name of its own.
    Failed,
    /// The development gate: the account's drive may not be read-write.
    WritesNotAllowed,
    /// The account is read-only, or its token cannot write.
    ModeNotGranted,
    /// The bus's `InvalidArgs`: a label, a client id, a mode or a choice that is not one.
    InvalidArgs,
    /// The bus's `Failed`: what the accounts and `config.toml` refuse with no name of
    /// their own.
    BusFailed,
    /// The bus's answer for an object that is not there: an account removed meanwhile.
    UnknownObject,
    /// The bus's answer for a method that is not there.
    UnknownMethod,
    /// The bus's answer for an interface that is not there.
    UnknownInterface,
    /// What an error of zbus's own goes out as.
    Internal,
    /// A name this build does not know, as it came.
    Other(String),
}

impl Refusal {
    /// Every variant but [`Refusal::Other`].
    pub const ALL: [Refusal; 29] = [
        Self::NotEmpty,
        Self::Unsupported,
        Self::InUse,
        Self::NoRoot,
        Self::NoHelper,
        Self::NotManaged,
        Self::NotHydrated,
        Self::ModifiedLocally,
        Self::OutsideRoot,
        Self::AlreadyRegistered,
        Self::NotSignedIn,
        Self::NoSource,
        Self::NoConflict,
        Self::NotAllowed,
        Self::Overlaps,
        Self::NoAccount,
        Self::NotUploaded,
        Self::PendingUploads,
        Self::Unreachable,
        Self::NotUp,
        Self::Failed,
        Self::WritesNotAllowed,
        Self::ModeNotGranted,
        Self::InvalidArgs,
        Self::BusFailed,
        Self::UnknownObject,
        Self::UnknownMethod,
        Self::UnknownInterface,
        Self::Internal,
    ];

    /// The name of a variant this build knows; `None` for [`Refusal::Other`].
    pub fn known_name(&self) -> Option<&'static str> {
        Some(match self {
            Self::NotEmpty => "org.konedrive.Error.NotEmpty",
            Self::Unsupported => "org.konedrive.Error.Unsupported",
            Self::InUse => "org.konedrive.Error.InUse",
            Self::NoRoot => "org.konedrive.Error.NoRoot",
            Self::NoHelper => "org.konedrive.Error.NoHelper",
            Self::NotManaged => "org.konedrive.Error.NotManaged",
            Self::NotHydrated => "org.konedrive.Error.NotHydrated",
            Self::ModifiedLocally => "org.konedrive.Error.ModifiedLocally",
            Self::OutsideRoot => "org.konedrive.Error.OutsideRoot",
            Self::AlreadyRegistered => "org.konedrive.Error.AlreadyRegistered",
            Self::NotSignedIn => "org.konedrive.Error.NotSignedIn",
            Self::NoSource => "org.konedrive.Error.NoSource",
            Self::NoConflict => "org.konedrive.Error.NoConflict",
            Self::NotAllowed => "org.konedrive.Error.NotAllowed",
            Self::Overlaps => "org.konedrive.Error.Overlaps",
            Self::NoAccount => "org.konedrive.Error.NoAccount",
            Self::NotUploaded => "org.konedrive.Error.NotUploaded",
            Self::PendingUploads => "org.konedrive.Error.PendingUploads",
            Self::Unreachable => "org.konedrive.Error.Unreachable",
            Self::NotUp => "org.konedrive.Error.NotUp",
            Self::Failed => "org.konedrive.Error.Failed",
            Self::WritesNotAllowed => "org.konedrive.Error.WritesNotAllowed",
            Self::ModeNotGranted => "org.konedrive.Error.ModeNotGranted",
            Self::InvalidArgs => "org.freedesktop.DBus.Error.InvalidArgs",
            Self::BusFailed => "org.freedesktop.DBus.Error.Failed",
            Self::UnknownObject => "org.freedesktop.DBus.Error.UnknownObject",
            Self::UnknownMethod => "org.freedesktop.DBus.Error.UnknownMethod",
            Self::UnknownInterface => "org.freedesktop.DBus.Error.UnknownInterface",
            Self::Internal => "org.freedesktop.zbus.Error",
            Self::Other(_) => return None,
        })
    }

    /// The D-Bus error name; of [`Refusal::Other`], the name as it came.
    pub fn name(&self) -> &str {
        match self {
            Self::Other(name) => name,
            known => known.known_name().unwrap_or_default(),
        }
    }

    /// What `name` names; a name this build does not know is [`Refusal::Other`].
    pub fn parse(name: &str) -> Self {
        Self::ALL.iter().find(|refusal| refusal.known_name() == Some(name)).cloned().unwrap_or_else(|| Self::Other(name.to_owned()))
    }

    /// The refusal a failed call or a failed read of a property carries, if it carries a
    /// name ([`error_name`], or the name of the bus's own error a property read comes back
    /// as): an error that is no reply of the daemon's (the connection, a timeout zbus made
    /// itself) has none.
    pub fn from_error(error: &zbus::Error) -> Option<Self> {
        use zbus::DBusError;
        match error {
            zbus::Error::FDO(inner) => match inner.as_ref() {
                zbus::fdo::Error::ZBus(error) => Self::from_error(error),
                named => Some(Self::parse(named.name().as_str())),
            },
            other => error_name(other).map(Self::parse),
        }
    }

    /// Whether `error` says that the object called is not there ([`Refusal::is_gone`]): an
    /// account removed while a command ran.
    pub fn says_gone(error: &zbus::Error) -> bool {
        Self::from_error(error).is_some_and(|refusal| refusal.is_gone())
    }

    /// Whether this is the bus's answer for an object, interface or method that is not
    /// there: an account removed while the call was made.
    pub fn is_gone(&self) -> bool {
        matches!(self, Self::UnknownObject | Self::UnknownMethod | Self::UnknownInterface)
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests;
