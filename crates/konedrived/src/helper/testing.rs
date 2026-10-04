//! Test support of `helper/`: a stand-in for the helper's end of the socket. One accept
//! loop for the daemon's tests that need a link; each test says what its helper answers.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};

use super::HelperLink;

/// A `SOCK_SEQPACKET` listener bound at `path`, as the helper builds its own
/// (`konedrive-helper/src/main.rs`): a `UnixListener` is a stream socket, which `Channel`
/// refuses.
pub(crate) fn seqpacket_listener(path: &Path) -> OwnedFd {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
    bind(fd.as_raw_fd(), &UnixAddr::new(path).unwrap()).unwrap();
    listen(&fd, Backlog::new(16).unwrap()).unwrap();
    fd
}

pub(crate) fn seqpacket_accept(listener: &OwnedFd) -> UnixStream {
    let fd = accept(listener.as_raw_fd()).unwrap();
    // SAFETY: `accept` just returned a descriptor this process alone owns.
    unsafe { UnixStream::from_raw_fd(fd) }
}

/// Reads and acknowledges the daemon's opening `Hello`, as the helper does: `connect`
/// returns only after this exchange.
pub(crate) fn ack_hello(channel: &mut Channel) {
    let (hello, _) = channel.recv::<ToHelper>().unwrap();
    assert!(matches!(hello, ToHelper::Hello { version } if version == PROTOCOL_VERSION), "{hello:?}");
    channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
}

/// A stand-in helper at `path`: accepts one connection, greets, acknowledges the `Hello`,
/// and answers every message after that with the errno `answer` gives for it. `answer` runs
/// on the helper's thread while the daemon waits for the acknowledgement.
///
/// The listener is bound on the caller's thread, before this returns, so the test's
/// `connect` cannot come before the `bind`.
pub(crate) fn fake_helper(path: &Path, mut answer: impl FnMut(&ToHelper, Option<OwnedFd>) -> i32 + Send + 'static) {
    let listener = seqpacket_listener(path);
    std::thread::spawn(move || {
        let mut channel = Channel::new(seqpacket_accept(&listener)).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        ack_hello(&mut channel);
        while let Ok((message, fd)) = channel.recv::<ToHelper>() {
            let errno = answer(&message, fd);
            if channel.send(&ToDaemon::Ack { errno }, None).is_err() {
                break;
            }
        }
    });
}

/// A link to the stand-in helper at `socket_path`.
pub(crate) async fn connected(socket_path: &Path) -> HelperLink {
    HelperLink::connect(socket_path).await.unwrap().0
}
