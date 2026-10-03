//! Messages between the privileged helper and the user's daemon, framed one
//! per SOCK_SEQPACKET datagram, with at most one file descriptor attached.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::raw::c_void;
use std::os::unix::net::UnixStream;

use nix::sys::socket::{sockopt, ControlMessage, MsgFlags, SockType};
use serde::{Deserialize, Serialize};

/// Both ends refuse any other (the helper's `Hello` check, the daemon's
/// `Welcome` check). 2: `OpenByHandle`, and an `Ack` that may carry a
/// descriptor.
pub const PROTOCOL_VERSION: u32 = 2;
pub const SOCKET_PATH: &str = "/run/konedrive/helper.sock";

/// Room for several descriptors in the control buffer, not just the one
/// message the protocol ever legitimately attaches. This keeps ordinary
/// traffic from ever tripping the truncation path below; it does not remove
/// the need to handle truncation correctly; a peer can always attach more
/// than this fits.
const MAX_CONTROL_FDS: u32 = 8;

/// How many `HydrateRequest`s the helper may have outstanding on one daemon
/// connection at once — handed out, and not yet answered by `HydrateDone` —
/// and, just as binding, how many the daemon must be able to take in
/// **without its reader thread ever stopping**.
///
/// A contract between the two ends, not a tuning knob for either, and the
/// reason is a circular wait the VM suite's burst measured. Every message
/// the daemon sends is answered with an `Ack`, and the `Ack` travels on the
/// same socket, in order, behind whatever requests the helper had already
/// queued. The daemon's hydration loop holds each of its four fill slots
/// until the `Ack` for that fill's `HydrateDone` arrives, and
/// its reader thread stops reading when its request queue is full. With
/// more requests in flight than that queue holds, the reader stops with
/// requests still in the socket ahead of an `Ack`; the slot waiting for that
/// `Ack` never frees; the queue never drains; the reader never resumes. The
/// fills do not slow down — they stop, and 30 s later the daemon's call
/// timeout ends the connection and every opener enrolled on it is denied
/// `EIO` (662 of them in a 3000-open burst, with an instant source).
///
/// The helper therefore never has more than this many requests out on a
/// connection, and the daemon's request queue is exactly this deep, so the
/// requests in flight always fit in it and the reader always gets as far as
/// the next `Ack`. Change one side, change both.
///
/// It is a **credit**, not a limit on users. A new hydration
/// beyond it is enrolled in the helper and held back — its openers stay
/// suspended, exactly as they would behind a request already sent — and each
/// `HydrateDone` that returns a credit sends the oldest one waiting. It used
/// to be refused `EAGAIN` instead, and a desktop thumbnailing a folder of 200
/// photos is not something that should fail two thirds of its opens.
pub const MAX_OUTSTANDING_HYDRATIONS: usize = 64;

/// The errno values the kernel accepts in a `FAN_DENY` response, measured
/// (M2) by sweeping the errno space against a real `FAN_CLASS_PRE_CONTENT`
/// group on Btrfs, ext4 and XFS — see `docs/kernel-behavior-7.2/interception.md` §5.
///
/// Anything outside this set makes `write()` on the group fail with `EINVAL`
/// and leaves the opener suspended **forever**, which is the worst outcome
/// the helper has: worse than a wrong errno, worse than a denial the user did
/// not expect. The daemon reports network failures, so `ENOENT` (a deleted
/// item), `ECONNRESET`, `ETIMEDOUT` and `ECANCELED` are all *expected* inputs
/// here and all outside the set.
///
/// This lives here rather than in the helper because it is a
/// fact about the `errno` that travels in [`ToHelper::HydrateDone`] and ends
/// up in the kernel's response word: both ends of that wire need it, the
/// daemon to produce a deliverable value and the helper to refuse an
/// undeliverable one, and neither is entitled to its own copy.
pub const ACCEPTED_DENY_ERRNOS: [i32; 8] = [
    0,
    libc::EPERM,
    libc::EIO,
    libc::EAGAIN,
    libc::EBUSY,
    libc::ETXTBSY,
    libc::ENOSPC,
    libc::EDQUOT,
];

/// Maps any errno onto one the kernel will actually deliver, defaulting to
/// `EIO` ("something went wrong reading this file"), which is both true and
/// the spec's default (§9).
///
/// This is a **clamp, not a flattening**: `ENOSPC` and `EDQUOT` are in the
/// accepted set and are exactly what a local `pwrite` produces on a full
/// disk or an exhausted quota, and §9 asks for `ENOSPC` by name there.
/// Mapping them to `EIO` would throw away the one piece of information the
/// user can act on.
pub fn clamp_deny_errno(errno: i32) -> i32 {
    if ACCEPTED_DENY_ERRNOS.contains(&errno) {
        errno
    } else {
        libc::EIO
    }
}

/// The daemon's half of the conversation. Every variant that needs an object
/// sends it as an attached descriptor, never as a path: the helper must act on
/// exactly the object the daemon opened.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToHelper {
    Hello { version: u32 },
    /// The attached fd is the root directory.
    RegisterRoot { root_id: String },
    UnregisterRoot { root_id: String },
    /// The attached fd is a directory inside a registered root.
    MarkDir,
    UnmarkDir,
    /// The attached fd is a file that left the root and must stay covered.
    MarkFile,
    /// The attached fd is a file whose ignore mark must go (before dehydration).
    ClearIgnore,
    HydrateDone { req_id: u64, errno: i32 },
    /// A descriptor for the object this file handle names (writes design
    /// §4.6): one of the peer's own, gone from its folder. The attached fd is
    /// a directory of the peer's on the same filesystem, the one the handle
    /// is opened relative to. `handle_type` and `handle` are what
    /// `name_to_handle_at` gave: at most 128 bytes (`MAX_HANDLE_SZ`). The
    /// answer is an `Ack`, carrying the object's descriptor when its errno
    /// is 0: `EPERM` when the object is not the peer's to have, `ESTALE`
    /// when it is gone.
    OpenByHandle { handle_type: i32, handle: Vec<u8> },
}

/// The helper's half.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToDaemon {
    Welcome { version: u32 },
    /// `errno` is 0 on success. Only the answer to `OpenByHandle` carries a
    /// descriptor, and only on success.
    Ack { errno: i32 },
    /// The attached fd is the event fd of a suspended open.
    HydrateRequest { req_id: u64 },
}

/// One message per datagram, at most one descriptor attached.
#[derive(Debug)]
pub struct Channel {
    socket: UnixStream,
}

impl Channel {
    /// Fails unless `socket` is a genuine `SOCK_SEQPACKET` socket.
    ///
    /// `Channel::recv` relies on the kernel's per-`send()` datagram framing
    /// to keep messages apart; handed a `SOCK_STREAM` socket instead, it
    /// would silently corrupt data the moment two messages are sent back to
    /// back (see the `two_messages_sent_back_to_back` test). Rejecting the
    /// wrong socket type here, once, is cheaper than debugging that later.
    pub fn new(socket: UnixStream) -> io::Result<Self> {
        let kind = nix::sys::socket::getsockopt(&socket, sockopt::SockType)?;
        if kind != SockType::SeqPacket {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Channel requires a SOCK_SEQPACKET socket, got {kind:?}"),
            ));
        }
        Ok(Self { socket })
    }

    pub fn get_ref(&self) -> &UnixStream {
        &self.socket
    }

    pub fn send<M: Serialize>(&mut self, message: &M, fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
        let encoded = serde_json::to_vec(message)?;
        let io_slices = [io::IoSlice::new(&encoded)];
        let fds = fd.map(|fd| [fd.as_raw_fd()]);
        let control: Vec<ControlMessage> = match &fds {
            Some(fds) => vec![ControlMessage::ScmRights(fds)],
            None => Vec::new(),
        };
        loop {
            match nix::sys::socket::sendmsg::<()>(
                self.socket.as_raw_fd(),
                &io_slices,
                &control,
                MsgFlags::empty(),
                None,
            ) {
                Ok(_) => return Ok(()),
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn recv<M: for<'de> Deserialize<'de>>(&mut self) -> io::Result<(M, Option<OwnedFd>)> {
        let mut buffer = vec![0u8; 64 * 1024];
        let mut control =
            vec![0u8; unsafe { libc::CMSG_SPACE(MAX_CONTROL_FDS * mem::size_of::<RawFd>() as u32) } as usize];

        let mut iov = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast::<c_void>(),
            iov_len: buffer.len(),
        };
        // SAFETY: zero-initialising `msghdr` is valid; every field is either
        // a plain integer or a pointer we set explicitly below before it is
        // read.
        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast::<c_void>();
        msg.msg_controllen = control.len();

        let received = loop {
            // SAFETY: `msg` points at `iov` and `control`, both of which are
            // live local buffers for the duration of this call; the fd is a
            // valid, open socket owned by `self.socket`.
            let rc = unsafe { libc::recvmsg(self.socket.as_raw_fd(), &mut msg, 0) };
            if rc >= 0 {
                break rc as usize;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        };

        // Walk whatever control data the kernel actually wrote, regardless
        // of `MSG_CTRUNC`. The records that did fit are complete, well-formed
        // cmsg entries describing descriptors the kernel has *already*
        // installed into this process's descriptor table — installing them
        // is not conditional on the caller's buffer being big enough to
        // describe them all. nix's safe `RecvMsg::cmsgs()` refuses to
        // iterate at all once `MSG_CTRUNC` is set, which is exactly what let
        // those descriptors leak: this process learned nothing about fds the
        // kernel had already installed, so it could never close them.
        let mut fds: Vec<RawFd> = Vec::new();
        // SAFETY: `msg` was just filled in by a successful `recvmsg`, so
        // its cmsg chain, if any, lives inside `control`, which is still
        // alive and unmoved here.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                let hdr = &*cmsg;
                if hdr.cmsg_level == libc::SOL_SOCKET && hdr.cmsg_type == libc::SCM_RIGHTS {
                    let payload_len = hdr.cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    let count = payload_len / mem::size_of::<RawFd>();
                    let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                    for i in 0..count {
                        fds.push(data.add(i).read_unaligned());
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }

        let close_all = |fds: &[RawFd]| {
            for &fd in fds {
                // SAFETY: each of these descriptors was just installed into
                // this process by the kernel via `recvmsg` above and has not
                // been handed to any owner (no `OwnedFd` wraps it yet), so
                // closing it here cannot double-close or affect anyone else.
                unsafe {
                    libc::close(fd);
                }
            }
        };

        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            close_all(&fds);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control message truncated: peer attached more descriptors than fit",
            ));
        }
        if fds.len() > 1 {
            // The protocol never legitimately attaches more than one fd;
            // a peer that does is misbehaving and gets dropped.
            close_all(&fds);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "peer attached more than one descriptor",
            ));
        }

        if received == 0 {
            close_all(&fds);
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
        }

        // SAFETY: this raw fd came straight out of the kernel's cmsg data
        // for this call and has not been given to anything else yet, so this
        // `OwnedFd` becomes its sole owner.
        let fd = fds.into_iter().next().map(|raw| unsafe { OwnedFd::from_raw_fd(raw) });
        let message = serde_json::from_slice(&buffer[..received])?;
        Ok((message, fd))
    }
}

#[cfg(test)]
mod tests;
