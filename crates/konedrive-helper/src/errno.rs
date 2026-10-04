//! The helper's one type for "refused, and with which errno".
//!
//! An errno travels three ways out of the helper: in an `Ack` to a daemon,
//! in the response word of a denied open, and into the log. All three used
//! to carry a bare `i32` in which 0 meant success. Inside the helper a
//! refusal is now an [`Errno`] and an outcome a `Result<_, Errno>`; the
//! integer, with its 0, exists only where a message is read or written
//! ([`Errno::from_wire`], [`Errno::to_wire`]).

use std::fmt;
use std::io;

/// An errno the helper refuses something with: the value an `Ack` carries,
/// or an opener is denied with.
///
/// It holds whatever number it was given. A value a daemon reports in
/// `HydrateDone` can be any `i32`, and is made deliverable only where an
/// open is denied with it ([`deliverable`](Self::deliverable)).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Errno(i32);

impl Errno {
    pub const EPERM: Errno = Errno(libc::EPERM);
    pub const EIO: Errno = Errno(libc::EIO);
    pub const EAGAIN: Errno = Errno(libc::EAGAIN);
    pub const EINVAL: Errno = Errno(libc::EINVAL);
    pub const EDQUOT: Errno = Errno(libc::EDQUOT);
    pub const EOPNOTSUPP: Errno = Errno(libc::EOPNOTSUPP);
    pub const ESTALE: Errno = Errno(libc::ESTALE);

    /// The errno of an I/O error, `EIO` for one that has none.
    pub fn of(e: &io::Error) -> Self {
        Errno(e.raw_os_error().unwrap_or(libc::EIO))
    }

    /// The errno of the system call that has just failed on this thread.
    pub fn last() -> Self {
        Self::of(&io::Error::last_os_error())
    }

    /// The number itself.
    pub fn raw(self) -> i32 {
        self.0
    }

    /// The errno the kernel will deliver in a denial for this one: itself if
    /// it is one of those the kernel accepts, `EIO` otherwise
    /// (`konedrive_proto::clamp_deny_errno`).
    pub fn deliverable(self) -> Self {
        Errno(konedrive_proto::clamp_deny_errno(self.0))
    }

    /// What the `errno` of a message says: 0 is success, anything else the
    /// errno of a refusal, as it came.
    pub fn from_wire(errno: i32) -> Result<(), Errno> {
        match errno {
            0 => Ok(()),
            errno => Err(Errno(errno)),
        }
    }

    /// The `errno` a message carries for `outcome`: 0 for success.
    pub fn to_wire<T>(outcome: &Result<T, Errno>) -> i32 {
        match outcome {
            Ok(_) => 0,
            Err(errno) => errno.0,
        }
    }
}

/// The number, as the log has always shown an errno.
impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Compared with the `libc` constant it stands for.
impl PartialEq<i32> for Errno {
    fn eq(&self, other: &i32) -> bool {
        self.0 == *other
    }
}

#[cfg(test)]
mod tests;
