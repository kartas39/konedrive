//! The errno a program is answered with when the disk refused.

use std::io;

use fuser::Errno;

/// The errno of `e`, or `EIO` for an error that carries none.
pub(super) fn to_errno(e: &io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}
