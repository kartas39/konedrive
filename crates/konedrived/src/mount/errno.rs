//! The errno a program is answered with when the disk refused.

use std::io;

use fuser::Errno;

/// The errno of `e` as a number, or `EIO` for an error that carries none: what a download
/// and a fetched range, which run off the kernel's requests, fail with.
pub(super) fn to_raw_errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// The errno of `e` ([`to_raw_errno`]), as a request is answered with it.
pub(super) fn to_errno(e: &io::Error) -> Errno {
    Errno::from_i32(to_raw_errno(e))
}
