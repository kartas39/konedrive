use nix::sys::socket::{accept, bind};

use super::*;
use crate::helper::testing::seqpacket_listener;

/// probe: whether a socket is bound at the helper's path,
/// told apart without ever connecting to it — a connection would be
/// registered as this uid's daemon while it lived.
#[test]
fn the_probe_tells_a_bound_helper_from_none_without_connecting() {
    let dir = tempfile::tempdir().unwrap();

    let nothing = dir.path().join("nothing.sock");
    assert_eq!(helper_presence(&nothing), HelperPresence::Absent, "no file");

    let stale = dir.path().join("stale.sock");
    drop(seqpacket_listener(&stale));
    assert!(stale.exists(), "a helper that exits leaves its socket file behind");
    assert_eq!(helper_presence(&stale), HelperPresence::Absent, "nothing bound to it");

    let not_a_socket = dir.path().join("file.sock");
    std::fs::write(&not_a_socket, b"").unwrap();
    assert_eq!(helper_presence(&not_a_socket), HelperPresence::Absent);

    let live = dir.path().join("live.sock");
    let listener = seqpacket_listener(&live);
    assert_eq!(helper_presence(&live), HelperPresence::Present);
    // Nothing was queued for the listener to accept.
    nix::fcntl::fcntl(&listener, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK))
        .unwrap();
    let pending = accept(listener.as_raw_fd());
    assert_eq!(pending, Err(nix::errno::Errno::EAGAIN), "the probe connected");

    // Bound and not yet listening — a helper between its bind and its
    // listen — is there too.
    let bound = dir.path().join("bound.sock");
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
        .unwrap();
    bind(fd.as_raw_fd(), &UnixAddr::new(&bound).unwrap()).unwrap();
    assert_eq!(helper_presence(&bound), HelperPresence::Present);

    // Something else's stream socket is nothing to be sure of.
    let stream = dir.path().join("stream.sock");
    let _stream = std::os::unix::net::UnixListener::bind(&stream).unwrap();
    assert!(matches!(helper_presence(&stream), HelperPresence::Unknown(_)));
}
