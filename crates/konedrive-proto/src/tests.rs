use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

use nix::sys::socket::{socketpair, AddressFamily, SockFlag};

use super::*;

/// A genuine `SOCK_SEQPACKET` pair, wrapped into `std::os::unix::net::UnixStream`
/// only so `Channel` can hold onto a familiar type. `UnixStream::pair()`
/// builds a `SOCK_STREAM` pair, which is the wrong socket type for this
/// protocol — see `two_messages_sent_back_to_back` below for what goes
/// wrong when a stream socket is used instead.
fn seqpacket_pair() -> (UnixStream, UnixStream) {
    let (a, b) = socketpair(AddressFamily::Unix, nix::sys::socket::SockType::SeqPacket, None, SockFlag::empty())
        .unwrap();
    let a: OwnedFd = a;
    let b: OwnedFd = b;
    (UnixStream::from(a), UnixStream::from(b))
}

fn pair() -> (Channel, Channel) {
    let (a, b) = seqpacket_pair();
    (Channel::new(a).unwrap(), Channel::new(b).unwrap())
}

fn open_fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

/// Held by every test here for its whole run. `too_many_descriptors_does_
/// not_leak_fds` counts the process's descriptors, and every other test
/// opens and closes some while it runs, in parallel by default.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn messages_round_trip() {
    let _serial = serial();
    let (mut client, mut server) = pair();
    client
        .send(&ToHelper::Hello { version: PROTOCOL_VERSION }, None)
        .unwrap();
    let (message, fd) = server.recv::<ToHelper>().unwrap();
    assert!(fd.is_none());
    assert!(matches!(message, ToHelper::Hello { version } if version == PROTOCOL_VERSION));
}

#[test]
fn a_descriptor_travels_with_its_message() {
    let _serial = serial();
    use std::io::{Read, Seek, Write};

    let (mut client, mut server) = pair();
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"payload").unwrap();

    client
        .send(&ToHelper::MarkFile, Some(std::os::fd::AsFd::as_fd(&file)))
        .unwrap();
    let (message, fd) = server.recv::<ToHelper>().unwrap();
    assert!(matches!(message, ToHelper::MarkFile));

    let mut received = std::fs::File::from(fd.expect("descriptor"));
    received.rewind().unwrap();
    let mut content = String::new();
    received.read_to_string(&mut content).unwrap();
    assert_eq!(content, "payload", "the received fd must point at the same file");
}

#[test]
fn a_closed_peer_is_reported_as_eof() {
    let _serial = serial();
    let (client, mut server) = pair();
    drop(client);
    let error = server.recv::<ToHelper>().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof, "{error:?}");
}

/// `OpenByHandle` carries its handle as given and the directory as a
/// descriptor; its answer is an `Ack` with the object's descriptor.
#[test]
fn open_by_handle_and_its_answer_carry_their_descriptors() {
    let _serial = serial();
    use std::io::{Read, Seek, Write};

    let (mut daemon, mut helper) = pair();
    let dir = tempfile::tempdir().unwrap();
    let anchor = std::fs::File::open(dir.path()).unwrap();
    let handle = vec![0xab; 128];
    daemon
        .send(
            &ToHelper::OpenByHandle { handle_type: 0x4d, handle: handle.clone() },
            Some(std::os::fd::AsFd::as_fd(&anchor)),
        )
        .unwrap();
    let (message, fd) = helper.recv::<ToHelper>().unwrap();
    assert!(
        matches!(&message, ToHelper::OpenByHandle { handle_type: 0x4d, handle: got } if *got == handle),
        "{message:?}"
    );
    assert!(fd.is_some(), "the directory travels as a descriptor");

    let mut object = tempfile::tempfile().unwrap();
    object.write_all(b"object").unwrap();
    helper.send(&ToDaemon::Ack { errno: 0 }, Some(std::os::fd::AsFd::as_fd(&object))).unwrap();
    let (answer, fd) = daemon.recv::<ToDaemon>().unwrap();
    assert!(matches!(answer, ToDaemon::Ack { errno: 0 }), "{answer:?}");
    let mut received = std::fs::File::from(fd.expect("the object's descriptor"));
    received.rewind().unwrap();
    let mut content = String::new();
    received.read_to_string(&mut content).unwrap();
    assert_eq!(content, "object");
}

/// The wrong socket type is rejected at construction rather than
/// producing silent corruption later.
#[test]
fn new_rejects_a_stream_socket() {
    let _serial = serial();
    let (a, _b) = UnixStream::pair().unwrap();
    let error = Channel::new(a).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error:?}");
}

/// `SOCK_SEQPACKET` keeps every `send()` as its own message, so two
/// messages sent with no interleaved `recv()` must both arrive intact
/// and in order. Over a `SOCK_STREAM` pair (what `UnixStream::pair()`
/// builds) the same sequence corrupts: the two payloads coalesce into a
/// single `read()`, the first `recv` fails to parse ("trailing
/// characters" from serde_json) and the second call hangs forever
/// waiting for bytes that already arrived. This is exactly the case the
/// original test suite never covered.
#[test]
fn two_messages_sent_back_to_back() {
    let _serial = serial();
    let (mut client, mut server) = pair();
    client.send(&ToHelper::MarkDir, None).unwrap();
    client.send(&ToHelper::UnmarkDir, None).unwrap();

    let (first, fd1) = server.recv::<ToHelper>().unwrap();
    assert!(fd1.is_none());
    assert!(matches!(first, ToHelper::MarkDir), "{first:?}");

    let (second, fd2) = server.recv::<ToHelper>().unwrap();
    assert!(fd2.is_none());
    assert!(matches!(second, ToHelper::UnmarkDir), "{second:?}");
}

/// Reproduces the descriptor leak: a peer attaches more descriptors than
/// the receiver's control buffer can describe. Before the fix, the
/// kernel still installed the ones that fit into this process's
/// descriptor table, but `recv` never learned their numbers (nix's
/// `cmsgs()` refuses to iterate once `MSG_CTRUNC` is set) and so could
/// never close them — this process's open-descriptor count grew by one
/// every call. `recv` must now either report the whole message as a
/// protocol error or, at minimum, never leave this process holding more
/// open descriptors than before the call.
#[test]
fn too_many_descriptors_does_not_leak_fds() {
    let _serial = serial();
    use std::os::fd::AsFd;

    let (client, mut server) = pair();
    let files: Vec<_> = (0..16).map(|_| tempfile::tempfile().unwrap()).collect();

    let before = open_fd_count();

    // `Channel::send` only ever attaches one descriptor; reach past the
    // public API to attach many, the way a misbehaving or malicious peer
    // connected to the real socket could.
    let encoded = serde_json::to_vec(&ToHelper::MarkFile).unwrap();
    let io_slices = [io::IoSlice::new(&encoded)];
    let raw_fds: Vec<RawFd> = files.iter().map(|f| f.as_fd().as_raw_fd()).collect();
    let control = [ControlMessage::ScmRights(&raw_fds)];
    nix::sys::socket::sendmsg::<()>(
        client.get_ref().as_raw_fd(),
        &io_slices,
        &control,
        MsgFlags::empty(),
        None,
    )
    .unwrap();

    let result = server.recv::<ToHelper>();
    assert!(result.is_err(), "a message with too many attached descriptors must be rejected");

    let after = open_fd_count();
    assert_eq!(
        after, before,
        "recv must not leave extra descriptors open after rejecting a truncated message"
    );
}

/// The one form a root id has: what the daemon mints, and all the helper
/// registers a root under.
#[test]
fn a_root_id_is_a_version_4_uuid_in_its_canonical_text() {
    assert!(is_root_id("1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d"));
    assert!(is_root_id("1C2E4F5A-0B3C-4D5E-8F60-71829A3B4C5D"));
    for bad in [
        "",
        "some-root",
        "1c2e4f5a-0b3c-1d5e-8f60-71829a3b4c5d",
        "1c2e4f5a0b3c4d5e8f6071829a3b4c5d",
        "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d ",
        "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5g",
        "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c\u{e9}",
    ] {
        assert!(!is_root_id(bad), "{bad:?}");
    }
}
