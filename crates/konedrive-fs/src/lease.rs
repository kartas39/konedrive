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
        // Ruling H72: before the process can ever be a lease holder, make
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

/// Makes `SIGIO` harmless for the whole process, once, before the first
/// lease is taken (Ruling H72).
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
/// file being dehydrated (spec §8 steps 3–5 hold the lease across a punch
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
        Some(libc::EACCES) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot take a write lease: file is owned by another user",
        )),
        _ => Err(error),
    }
}

impl Drop for WriteLease<'_> {
    fn drop(&mut self) {
        // SAFETY: plain fcntl on a valid descriptor; failure here is not actionable.
        unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) };
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

    /// Ruling H72. The kernel breaks a lease by signalling its holder, and
    /// the signal it uses (`SIGIO`) terminates the process by default. This
    /// is the whole failure, end to end: take a lease, have another process
    /// open the file — which is what every lease this crate takes exists to
    /// notice — and stay alive long enough to release it.
    ///
    /// Without [`silence_sigio`] this does not fail, it *kills the test
    /// binary*: `error: test failed ... signal: 29, SIGIO`, taking every
    /// other test in the process with it. That is exactly what would happen
    /// to the daemon, which holds this lease across the punch and the fsync
    /// of a whole file (spec §8 steps 3–5).
    #[test]
    fn a_lease_break_does_not_kill_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![7u8; 4096]).unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        let lease = WriteLease::take(&file).unwrap().expect("expected to get the lease");

        // Somebody else opening the file is what breaks the lease. The
        // kernel signals the holder — this process — and suspends the opener
        // until the lease is released (or `/proc/sys/fs/lease-break-time`
        // expires), so it is released once the signal has had its chance.
        //
        // The opener is a thread rather than a child process because an
        // earlier version of this test spawned `cat` and made the other two
        // lease tests in this crate fail intermittently — about one workspace
        // run in twenty. **Why** was never established. This comment used to
        // claim that `fork`/`posix_spawn` duplicates the descriptor table and
        // that every duplicate raises a `struct file` reference count
        // `F_SETLEASE` refuses on; that is false, and measuring it says so:
        // 0 failures in 2000 `posix_spawn`s, 0 in 3000 `fork`+`_exit`s and 0
        // in 3000 `fork`+`exec`s, against a control where a genuine second
        // `open()` gives `EAGAIN` every time. `check_conflicting_open()`
        // compares `inode->i_readcount`/`i_writecount`, which only
        // `do_dentry_open` raises — one per `struct file`, not per descriptor
        // — while `fork` duplicates the fd *table* and raises `f_count`,
        // which the lease check never reads. The flakiness was real; its
        // cause is still unknown, so the threads stay.
        //
        // A thread shares the descriptor table, and leases have no
        // same-process exemption: this open breaks the lease like any other.
        let opened = path.clone();
        let opener = std::thread::spawn(move || File::open(&opened).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(500));
        drop(lease);

        opener.join().expect("the opener should have proceeded once the lease was released");
        // Reaching this line at all is the assertion: the lease break did not
        // terminate us.
    }

    /// The same guarantee stated directly, so a regression is a readable
    /// failure rather than a killed test binary: once anything in this
    /// process has taken a lease, `SIGIO` must no longer carry its default
    /// (terminate) disposition.
    #[test]
    fn taking_a_lease_leaves_sigio_unable_to_terminate_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"data").unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        let _lease = WriteLease::take(&file).unwrap().expect("expected to get the lease");

        // SAFETY: querying the current disposition with a null `act` never
        // changes it; `current` is a live, correctly sized `sigaction`.
        let current = unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            assert_eq!(libc::sigaction(libc::SIGIO, std::ptr::null(), &mut current), 0);
            current
        };
        assert_ne!(
            current.sa_sigaction,
            libc::SIG_DFL,
            "SIGIO still has its default disposition (terminate); a lease break would kill us"
        );
    }

    #[test]
    fn taken_when_nobody_else_has_the_file_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"data").unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        let lease = WriteLease::take(&file).unwrap();
        assert!(lease.is_some(), "expected to get the lease");
    }

    #[test]
    fn refused_while_another_descriptor_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"data").unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        let _other = File::open(&path).unwrap();
        assert!(WriteLease::take(&file).unwrap().is_none(), "expected refusal");
    }

    // `EACCES` (file owned by another uid) only comes out of a real
    // `F_SETLEASE` call against a file this process does not own, which
    // needs a second, differently-uid'd file — i.e. root — to set up. That
    // is not available here (no sudo), so the errno-to-outcome mapping
    // itself is tested directly instead, with the same real errno values
    // the kernel would report.

    #[test]
    fn eagain_means_retry_later() {
        interpret_setlease_failure(io::Error::from_raw_os_error(libc::EAGAIN))
            .expect("EAGAIN must not be a hard error");
    }

    #[test]
    fn eacces_is_reported_as_owned_by_another_user_not_as_a_retryable_refusal() {
        let error = interpret_setlease_failure(io::Error::from_raw_os_error(libc::EACCES))
            .expect_err("EACCES must be a hard error, not Ok(None)");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error:?}");
        assert!(
            error.to_string().contains("another user"),
            "error text should say the file is owned by another user: {error}"
        );
    }
}
