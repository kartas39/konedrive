//! A write lease proves no other process has the file open.
//!
//! Taking one also arms a signal: the kernel breaks a lease by notifying its
//! holder with `SIGIO` (see [`silence_sigio`]), and `SIGIO`'s default
//! disposition is to terminate the process. Every lease taken through this
//! module neutralises that first, so holding a lease can never be fatal.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::Once;

/// Held while a file is guaranteed not to be open anywhere else. Released on drop.
#[must_use = "a WriteLease that is immediately dropped releases the lease it just acquired"]
pub struct WriteLease<'a> {
    file: &'a File,
}

impl<'a> WriteLease<'a> {
    /// `Ok(None)` means another descriptor for this file is open right now
    /// (`F_SETLEASE` failed with `EAGAIN`), so no lease can be taken yet —
    /// the caller must not touch the file's contents, but retrying later may
    /// succeed once that other descriptor closes.
    ///
    /// A file owned by another uid fails with `EACCES` instead: only the
    /// file's owner (or a process with `CAP_LEASE`) may place a lease on it
    /// at all, and that is never going to change no matter how many times
    /// the caller retries, so it is reported as a hard error rather than
    /// folded into the same "try again later" `Ok(None)` as `EAGAIN`.
    pub fn take(file: &'a File) -> io::Result<Option<Self>> {
        // Before the process can ever be a lease holder, make
        // sure the kernel's way of telling us so cannot kill it.
        silence_sigio();
        // SAFETY: plain fcntl on a valid descriptor.
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_WRLCK) };
        if rc == 0 {
            return Ok(Some(Self { file }));
        }
        interpret_setlease_failure(io::Error::last_os_error())?;
        Ok(None)
    }
}

/// Whether any process has the file `file` is open on open for writing: a
/// read lease (`F_RDLCK`) is refused `EAGAIN` exactly while the inode has a
/// writer (`check_conflicting_open` in `fs/locks.c` compares the inode's write
/// count; measured in this module's tests). `file` must be open read-only, as a
/// read lease requires, and one of this process's own read-only descriptors
/// does not count. The lease is released before this returns: it is a probe,
/// never held across anything slow.
///
/// A writable shared mapping counts as a writer for as long as it exists. A
/// file owned by another uid fails `EACCES`, as for a write lease.
pub fn open_for_writing(file: &File) -> io::Result<bool> {
    silence_sigio();
    // SAFETY: plain fcntl on a valid descriptor.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_RDLCK) };
    if rc == 0 {
        // SAFETY: as above; releasing a lease this descriptor holds.
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) };
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EAGAIN) => Ok(true),
        _ => Err(hard_setlease_error(error)),
    }
}

/// Makes `SIGIO` harmless for the whole process, once, before the first
/// lease is taken.
///
/// The kernel does not ask a lease holder anything: when another process
/// opens the file, it *signals* the holder — `SIGIO` by default (`fcntl(2)`,
/// "Leases"; `F_SETSIG` would change which signal) — and suspends the opener
/// until the lease is released or `/proc/sys/fs/lease-break-time` runs out.
/// `SIGIO`'s default disposition is **terminate**. So a process that takes a
/// lease and installs nothing dies the moment anybody touches the file, with
/// no error, no unwinding and no chance to release anything — measured: a
/// lease holder with no handler exits `128+29`. For the daemon that would
/// mean losing every in-flight hydration because a thumbnailer looked at one
/// file being dehydrated (a dehydration holds the lease across a punch
/// and an `fsync`).
///
/// It is set to `SIG_IGN` rather than to a handler because there is nothing
/// to do with the notification: the punch is short, the opener is meant to
/// wait for it, and the lease is released by `Drop` a moment later.
/// `SA_RESTART` is set so that, should the disposition ever be changed to a
/// real handler, an interrupted syscall is resumed rather than surfacing as
/// a spurious `EINTR`.
///
/// A disposition somebody else has already chosen is left alone: a process
/// that installed its own `SIGIO` handler (for signal-driven I/O, say) knows
/// something we do not, and only the *default* one is fatal.
fn silence_sigio() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `current` and `wanted` are live, correctly sized,
        // zero-initialised `sigaction` structs; querying with a null `act`
        // never changes the disposition, and the second call only replaces
        // it when it is still the default.
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGIO, std::ptr::null(), &mut current) != 0 {
                return;
            }
            if current.sa_sigaction != libc::SIG_DFL {
                return;
            }
            let mut wanted: libc::sigaction = std::mem::zeroed();
            wanted.sa_sigaction = libc::SIG_IGN;
            wanted.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut wanted.sa_mask);
            libc::sigaction(libc::SIGIO, &wanted, std::ptr::null_mut());
        }
    });
}

/// Turns the errno from a failed `F_SETLEASE` into either "busy, try again
/// later" (`Ok(())`, for `EAGAIN`) or a hard error — rewriting `EACCES` to
/// say what it actually means, since no amount of retrying fixes it.
fn interpret_setlease_failure(error: io::Error) -> io::Result<()> {
    match error.raw_os_error() {
        Some(libc::EAGAIN) => Ok(()),
        _ => Err(hard_setlease_error(error)),
    }
}

/// The error of a failed `F_SETLEASE` that is not "busy": itself, or, for
/// `EACCES`, one that says what it means.
fn hard_setlease_error(error: io::Error) -> io::Error {
    match error.raw_os_error() {
        Some(libc::EACCES) => io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot take a write lease: file is owned by another user",
        ),
        _ => error,
    }
}

impl Drop for WriteLease<'_> {
    fn drop(&mut self) {
        // SAFETY: plain fcntl on a valid descriptor; failure here is not actionable.
        unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) };
    }
}

#[cfg(test)]
mod tests;
