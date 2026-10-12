//! The errno a program is answered with when the disk refused.

use std::io;

use fuser::Errno;

/// The errno a program is answered when it asks for content of OneDrive while OneDrive
/// asks the account to wait (`status::throttle_wait`): the read, the open for writing or the
/// truncation fails at once, and succeeds when asked again after the wait.
///
/// `EAGAIN` was chosen for its message, "Resource temporarily unavailable", which is what
/// a program shows the person. What programs do on it — whether one asks again in a loop
/// — was not measured.
// TODO: check in the VM that the kernel hands this errno of a read through the page cache to the program as it is (`file_reads.sh`, `read_while_onedrive_asks_to_wait_fails_at_once_and_works_after`) and fix if needed
pub(super) const THROTTLE_WAIT_ERRNO: i32 = libc::EAGAIN;

/// The errno of `e` as a number, or `EIO` for an error that carries none: what a download
/// and a fetched range, which run off the kernel's requests, fail with.
pub(super) fn to_raw_errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// The errno of `e` ([`to_raw_errno`]), as a request is answered with it.
pub(super) fn to_errno(e: &io::Error) -> Errno {
    Errno::from_i32(to_raw_errno(e))
}
