//! Whether a helper is running, told without connecting to it.

use std::os::fd::AsRawFd;
use std::path::Path;

use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, UnixAddr};

/// Whether anything holds a socket bound at the helper's path, and so
/// whether a fanotify group of the helper's can exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperPresence {
    /// Nothing is bound there: no file, or a file left behind by a helper
    /// that has exited. The helper creates its group only moments before it
    /// binds and places its first mark only after, and the group — marks and
    /// all — goes when the process does; so no ignore mark of ours exists.
    Absent,
    /// A socket is bound there: a helper is running, or is starting and
    /// about to walk.
    Present,
    /// The question could not be answered; treated as [`Self::Present`].
    Unknown(String),
}

/// Looks at the helper's socket **without connecting to it**.
///
/// A connection, even one closed at once, is a connection the helper
/// registers as this uid's daemon: while it lived, the uid's hydrations
/// would go to it and be failed when it went. So the probe is a `connect`
/// with the wrong socket type — `SOCK_STREAM` against the helper's
/// `SOCK_SEQPACKET`. The kernel looks the path up, finds the socket bound to
/// that inode, and refuses the type with `EPROTOTYPE` before anything is
/// queued; a file with no socket bound to it is `ECONNREFUSED`, and no file
/// is `ENOENT`. Bound sockets are found by inode, not by network namespace,
/// so the helper's `PrivateNetwork=yes` changes nothing. Measured in the VM
/// suite, "every punch without interception clears the ignore mark first, or
/// is refused while a helper runs".
pub fn helper_presence(path: &Path) -> HelperPresence {
    let probe = || -> nix::Result<()> {
        let fd = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            None,
        )?;
        let addr = UnixAddr::new(path)?;
        connect(fd.as_raw_fd(), &addr)
    };
    match probe() {
        Err(nix::errno::Errno::ENOENT | nix::errno::Errno::ECONNREFUSED) => HelperPresence::Absent,
        Err(nix::errno::Errno::EPROTOTYPE) => HelperPresence::Present,
        // A stream socket bound there accepted, or queued, the probe: not
        // the helper's, but somebody's, and nothing to be sure of.
        Ok(()) | Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINPROGRESS) => {
            HelperPresence::Unknown(format!(
                "{} is a stream socket, not the helper's",
                path.display()
            ))
        }
        Err(e) => HelperPresence::Unknown(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests;
