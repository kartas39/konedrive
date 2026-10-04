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

/// Whether `id` has the form of a root id: what the daemon mints for a folder
/// and the only form the helper registers a root under — a version 4 UUID in
/// its canonical text, 36 characters, `8-4-4-4-12`, hex in either case, the
/// version nibble `4`.
///
/// One definition for both sides: the daemon believes no other value it finds
/// on a folder, and the helper stores and logs no other string a peer sends
/// with `RegisterRoot`.
pub fn is_root_id(id: &str) -> bool {
    if id.len() != 36 {
        return false;
    }
    let fields: Vec<&str> = id.split('-').collect();
    if fields.iter().map(|f| f.len()).ne([8, 4, 4, 4, 12]) {
        return false;
    }
    if !fields.iter().all(|f| f.chars().all(|c| c.is_ascii_hexdigit())) {
        return false;
    }
    fields[2].starts_with('4')
}

/// The daemon's half of the conversation. Every variant that needs an object
/// sends it as an attached descriptor, never as a path: the helper must act on
/// exactly the object the daemon opened.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToHelper {
    Hello { version: u32 },
    /// The attached fd is the root directory. `root_id` must be a root id
    /// ([`is_root_id`]); anything else is refused `EINVAL`. A uid that
    /// already holds as many roots as the helper allows one is refused
    /// `EDQUOT`.
    RegisterRoot { root_id: String },
    /// Any id the helper holds for the peer, whatever its form: a root
    /// registered before the form was checked can still be removed.
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
    /// `name_to_handle_at` gave: at most [`MAX_HANDLE_BYTES`]. The
    /// answer is an `Ack`, carrying the object's descriptor when its errno
    /// is 0: `EPERM` when the object is not the peer's to have, `ESTALE`
    /// when it is gone.
    OpenByHandle { handle_type: i32, handle: Vec<u8> },
}

/// The largest file handle the kernel hands out (`MAX_HANDLE_SZ`), and so
/// the longest `handle` an [`ToHelper::OpenByHandle`] may carry.
pub const MAX_HANDLE_BYTES: usize = 128;

/// What [`ToHelper::validate`] found wrong with a message's own fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Malformed {
    /// A `RegisterRoot` whose `root_id` is not a root id ([`is_root_id`]).
    RootId,
    /// An `OpenByHandle` whose handle the kernel could not have given: a
    /// negative type, no bytes, or more than [`MAX_HANDLE_BYTES`].
    Handle,
}

impl ToHelper {
    /// Whether the fields of this message are what the protocol says they
    /// are, whatever is attached to it and whoever sent it. One definition
    /// for both ends: the helper asks before it acts on a request, and
    /// refuses a malformed one `EINVAL`.
    ///
    /// Two fields are bounded by nothing but the datagram, and say so here:
    /// the `root_id` of an `UnregisterRoot`, which is only ever compared
    /// with the ids the helper holds (see the variant), and the `errno` of
    /// a `HydrateDone`, which the helper clamps ([`clamp_deny_errno`]).
    pub fn validate(&self) -> Result<(), Malformed> {
        match self {
            ToHelper::RegisterRoot { root_id } if !is_root_id(root_id) => Err(Malformed::RootId),
            ToHelper::OpenByHandle { handle_type, handle }
                if *handle_type < 0 || handle.is_empty() || handle.len() > MAX_HANDLE_BYTES =>
            {
                Err(Malformed::Handle)
            }
            ToHelper::Hello { .. }
            | ToHelper::RegisterRoot { .. }
            | ToHelper::UnregisterRoot { .. }
            | ToHelper::MarkDir
            | ToHelper::UnmarkDir
            | ToHelper::MarkFile
            | ToHelper::ClearIgnore
            | ToHelper::HydrateDone { .. }
            | ToHelper::OpenByHandle { .. } => Ok(()),
        }
    }
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

    /// The next message, and the descriptor that came with it: one datagram
    /// taken off the socket ([`receive`](Self::receive)), then read as an `M`.
    /// A datagram that is not an `M` is an error, and its descriptor is
    /// closed.
    pub fn recv<M: for<'de> Deserialize<'de>>(&mut self) -> io::Result<(M, Option<OwnedFd>)> {
        let Datagram { bytes, fd } = self.receive()?;
        let message = serde_json::from_slice(&bytes)?;
        Ok((message, fd))
    }

    /// Takes one datagram off the socket, as it came: nothing in it is
    /// looked at.
    ///
    /// An error for a closed peer and for more than one descriptor attached;
    /// every descriptor the kernel installed for a refused datagram is
    /// closed.
    fn receive(&mut self) -> io::Result<Datagram> {
        let mut buffer = vec![0u8; MAX_MESSAGE_BYTES];
        let mut control = ControlBuffer::new();

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
        msg.msg_control = control.bytes.as_mut_ptr().cast::<c_void>();
        msg.msg_controllen = control.bytes.len();

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

        // Owned from here on, so that every way out of this function closes
        // the ones it does not hand over.
        let fds = attached_descriptors(&msg);

        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control message truncated: peer attached more descriptors than fit",
            ));
        }
        if fds.len() > 1 {
            // The protocol never legitimately attaches more than one fd;
            // a peer that does is misbehaving and gets dropped.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "peer attached more than one descriptor",
            ));
        }
        if received == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
        }

        buffer.truncate(received);
        Ok(Datagram { bytes: buffer, fd: fds.into_iter().next() })
    }
}

/// The room [`Channel::recv`] has for one datagram. Every message of the
/// protocol is far shorter.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// One datagram as it came off the socket: its bytes, and the descriptor
/// attached to it.
struct Datagram {
    bytes: Vec<u8>,
    fd: Option<OwnedFd>,
}

/// How many bytes of control data one `recvmsg` has room for: the
/// `SCM_RIGHTS` record of [`MAX_CONTROL_FDS`] descriptors.
const CONTROL_BYTES: usize =
    // SAFETY: `CMSG_SPACE` is arithmetic on its argument.
    unsafe { libc::CMSG_SPACE(MAX_CONTROL_FDS * mem::size_of::<RawFd>() as u32) } as usize;

/// The control buffer of one `recvmsg`, aligned as the `cmsghdr`s the kernel
/// writes into it are: `CMSG_FIRSTHDR` and `CMSG_NXTHDR` hand out pointers
/// into it that are read as `cmsghdr`, which a plain byte buffer is aligned
/// for only by its allocator's habit.
#[repr(C)]
struct ControlBuffer {
    _align: [libc::cmsghdr; 0],
    bytes: [u8; CONTROL_BYTES],
}

impl ControlBuffer {
    fn new() -> Self {
        Self { _align: [], bytes: [0; CONTROL_BYTES] }
    }
}

/// Every descriptor the kernel attached to the datagram `msg` describes,
/// each owned.
///
/// Walks whatever control data the kernel actually wrote, regardless of
/// `MSG_CTRUNC`. The records that did fit are complete, well-formed cmsg
/// entries describing descriptors the kernel has *already* installed into
/// this process's descriptor table — installing them is not conditional on
/// the caller's buffer being big enough to describe them all. nix's safe
/// `RecvMsg::cmsgs()` refuses to iterate at all once `MSG_CTRUNC` is set,
/// which is exactly what let those descriptors leak: this process learned
/// nothing about fds the kernel had already installed, so it could never
/// close them.
fn attached_descriptors(msg: &libc::msghdr) -> Vec<OwnedFd> {
    let mut fds = Vec::new();
    // SAFETY: `msg` was just filled in by a successful `recvmsg`, so its
    // cmsg chain, if any, lives inside the caller's `ControlBuffer`, which
    // is alive, unmoved and aligned for a `cmsghdr`. Each descriptor number
    // in an `SCM_RIGHTS` record came straight from the kernel for this call
    // and has been given to nothing else, so the `OwnedFd` made of it is its
    // sole owner.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            let hdr = &*cmsg;
            if hdr.cmsg_level == libc::SOL_SOCKET && hdr.cmsg_type == libc::SCM_RIGHTS {
                let payload_len = hdr.cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = payload_len / mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
    }
    fds
}

#[cfg(test)]
mod tests;
