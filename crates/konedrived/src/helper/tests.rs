use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, Ordering};

use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{
    accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType,
    UnixAddr,
};

use super::*;

/// A `SOCK_SEQPACKET` listener bound at `path`, built the same way the
/// helper builds its own (`konedrive-helper/src/main.rs::listen`).
/// `std::os::unix::net::UnixListener::bind` produces a `SOCK_STREAM`
/// listener, which `Channel::new` now rejects — a test harness built on
/// it would not exercise the same code path production traffic does.
fn seqpacket_listener(path: &std::path::Path) -> OwnedFd {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
        .unwrap();
    let addr = UnixAddr::new(path).unwrap();
    bind(fd.as_raw_fd(), &addr).unwrap();
    sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
    fd
}

fn seqpacket_accept(listener: &OwnedFd) -> UnixStream {
    let fd = accept(listener.as_raw_fd()).unwrap();
    // SAFETY: `accept` just returned a freshly opened descriptor that
    // this process now solely owns.
    unsafe { UnixStream::from_raw_fd(fd) }
}

/// Reads and acknowledges the daemon's opening `Hello`,
/// exactly as the real helper's `apply()` does — every fake helper below
/// must do this before it can see any real request, since `connect()`
/// now blocks on this exchange before it returns.
fn ack_hello(channel: &mut Channel) {
    let (hello, _) = channel.recv::<ToHelper>().unwrap();
    assert!(matches!(hello, ToHelper::Hello { version } if version == PROTOCOL_VERSION), "{hello:?}");
    channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
}

/// A stand-in helper: accepts one connection, greets, acknowledges the
/// handshake `Hello`, and answers every request after that with
/// Ack{0}, recording what it was asked to do.
///
/// The listener is bound here, on the caller's thread, before this
/// function returns — not inside the spawned thread — so that by the
/// time the test goes on to call `HelperLink::connect` the socket path
/// is guaranteed to already exist. Binding it inside the spawned thread
/// instead would race `connect` against `bind`.
fn fake_helper(path: std::path::PathBuf) -> std::sync::mpsc::Receiver<String> {
    let listener = seqpacket_listener(&path);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stream = seqpacket_accept(&listener);
        let mut channel = Channel::new(stream).unwrap();
        channel
            .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
            .unwrap();
        ack_hello(&mut channel);
        while let Ok((message, fd)) = channel.recv::<ToHelper>() {
            tx.send(format!("{message:?} fd={}", fd.is_some())).unwrap();
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        }
    });
    rx
}

#[tokio::test]
async fn sends_a_directory_to_be_marked_with_its_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let seen = fake_helper(socket.clone());
    let (link, _requests) = HelperLink::connect(&socket).await.unwrap();

    let handle = std::fs::File::open(dir.path()).unwrap();
    link.mark_dir(&handle).await.unwrap();

    let line = seen.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(line.starts_with("MarkDir"), "{line}");
    assert!(line.ends_with("fd=true"), "the directory must travel as a descriptor: {line}");
}

/// `OpenByHandle` sends the handle as given with the directory's
/// descriptor, and gets the object's descriptor back on the `Ack`; a
/// refusal is `Refused(errno)`; and a descriptor on an `Ack` that answers
/// an ordinary call does not disturb the pairing of the calls after it.
#[tokio::test]
async fn open_by_handle_returns_the_descriptor_the_ack_carries() {
    use std::io::{Read, Write};

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = seqpacket_listener(&socket);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stream = seqpacket_accept(&listener);
        let mut channel = Channel::new(stream).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        ack_hello(&mut channel);
        let mut object = tempfile::tempfile().unwrap();
        object.write_all(b"moved out").unwrap();
        for answer in [0, libc::ESTALE, 0] {
            let (message, fd) = channel.recv::<ToHelper>().unwrap();
            tx.send(format!("{message:?} fd={}", fd.is_some())).unwrap();
            let attach = (answer == 0).then(|| std::os::fd::AsFd::as_fd(&object));
            channel.send(&ToDaemon::Ack { errno: answer }, attach).unwrap();
        }
    });
    let (link, _requests) = HelperLink::connect(&socket).await.unwrap();
    let root = File::open(dir.path()).unwrap();
    let handle = FileHandle { kind: 0x4d, bytes: vec![1, 2, 3] };

    let object = link.open_by_handle(&root, &handle).await.unwrap();
    let mut content = String::new();
    let mut file = File::from(object);
    std::io::Seek::rewind(&mut file).unwrap();
    file.read_to_string(&mut content).unwrap();
    assert_eq!(content, "moved out");
    let asked = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(asked, "OpenByHandle { handle_type: 77, handle: [1, 2, 3] } fd=true");

    let gone = link.open_by_handle(&root, &handle).await.unwrap_err();
    assert!(matches!(gone, HelperError::Refused(e) if e == libc::ESTALE), "{gone:?}");
    // An ordinary call whose Ack carries a descriptor still just succeeds.
    link.mark_dir(&root).await.unwrap();
}

/// The helper's descriptor is read-only; the owner reopens it for writing
/// through `/proc/self/fd`, and gets the same file, wherever it lives.
#[test]
fn a_read_only_descriptor_is_reopened_for_writing_on_the_same_file() {
    use std::io::{Read, Write};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f");
    std::fs::write(&path, b"old").unwrap();
    let read_only: OwnedFd = File::open(&path).unwrap().into();
    let mut writable = reopen_for_writing(&read_only).unwrap();
    writable.write_all(b"new").unwrap();
    let mut content = String::new();
    File::open(&path).unwrap().read_to_string(&mut content).unwrap();
    assert_eq!(content, "new");
    let not_a_file: OwnedFd = File::open(dir.path()).unwrap().into();
    assert!(reopen_for_writing(&not_a_file).is_err());
}

#[tokio::test]
async fn a_refusal_from_the_helper_becomes_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = seqpacket_listener(&socket);
    std::thread::spawn(move || {
        let stream = seqpacket_accept(&listener);
        let mut channel = Channel::new(stream).unwrap();
        channel
            .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
            .unwrap();
        ack_hello(&mut channel);
        let _ = channel.recv::<ToHelper>().unwrap();
        channel.send(&ToDaemon::Ack { errno: libc::EPERM }, None).unwrap();
    });
    let (link, _requests) = HelperLink::connect(&socket).await.unwrap();
    let handle = std::fs::File::open(dir.path()).unwrap();
    let error = link.mark_dir(&handle).await.unwrap_err();
    assert!(matches!(error, HelperError::Refused(e) if e == libc::EPERM), "{error:?}");
}

#[tokio::test]
async fn hydrate_requests_arrive_on_the_channel() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = seqpacket_listener(&socket);
    std::thread::spawn(move || {
        let stream = seqpacket_accept(&listener);
        let mut channel = Channel::new(stream).unwrap();
        channel
            .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
            .unwrap();
        ack_hello(&mut channel);
        let payload = tempfile::tempfile().unwrap();
        channel
            .send(
                &ToDaemon::HydrateRequest { req_id: 7 },
                Some(std::os::fd::AsFd::as_fd(&payload)),
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
    });
    let (_link, mut requests) = HelperLink::connect(&socket).await.unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.req_id, 7);
}

/// The requirement leaves to the implementer: a call already
/// sent and awaiting its `Ack` must not hang forever if the helper goes
/// away mid-flight. The peer here accepts, greets, acknowledges the
/// handshake, and then closes without ever acknowledging the `MarkDir`
/// it receives.
#[tokio::test]
async fn a_dropped_connection_fails_every_outstanding_call() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = seqpacket_listener(&socket);
    std::thread::spawn(move || {
        let stream = seqpacket_accept(&listener);
        let mut channel = Channel::new(stream).unwrap();
        channel
            .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
            .unwrap();
        ack_hello(&mut channel);
        let _ = channel.recv::<ToHelper>().unwrap();
        // No Ack: the connection is simply dropped here.
    });
    let (link, _requests) = HelperLink::connect(&socket).await.unwrap();
    let handle = std::fs::File::open(dir.path()).unwrap();
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(5), link.mark_dir(&handle))
            .await
            .expect("must not hang forever waiting for a reply that will never come");
    assert!(matches!(result, Err(HelperError::NotRunning)), "{result:?}");
}

/// Strengthened after a test
/// showed the original version passed even with `pending`'s pop end
/// mutated from front to back (a genuine cross-wiring bug): checking
/// only receive order and that both calls returned `Ok(())` cannot
/// distinguish "paired correctly" from "paired backwards", because the
/// fake helper answered every request with the same errno. This version
/// acks the two calls with *different* errnos (0, then `EACCES`), so a
/// swapped pairing produces a wrong `Result` at the call site — not just
/// a wrong receive order at the helper.
///
/// `tokio::join!` polls its futures in argument order on a
/// single-threaded runtime, and each `call()` sends its message
/// synchronously before its first `.await` point, so `mark_dir`'s
/// message reaches the wire before `unmark_dir`'s.
#[tokio::test]
async fn two_concurrent_callers_do_not_cross_wire() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = seqpacket_listener(&socket);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stream = seqpacket_accept(&listener);
        let mut channel = Channel::new(stream).unwrap();
        channel
            .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
            .unwrap();
        ack_hello(&mut channel);
        for errno in [0, libc::EACCES] {
            let (message, fd) = channel.recv::<ToHelper>().unwrap();
            tx.send(format!("{message:?} fd={}", fd.is_some())).unwrap();
            channel.send(&ToDaemon::Ack { errno }, None).unwrap();
        }
    });
    let (link, _requests) = HelperLink::connect(&socket).await.unwrap();
    let handle = std::fs::File::open(dir.path()).unwrap();

    let (mark_result, unmark_result) = tokio::join!(link.mark_dir(&handle), link.unmark_dir(&handle));

    let first = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    let second = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(first.starts_with("MarkDir"), "{first}");
    assert!(second.starts_with("UnmarkDir"), "{second}");

    // The pairing proof: each call must get back *its own* answer, not
    // just "some" answer. A pairing bug that pops `pending` from the
    // wrong end swaps these two results between the callers.
    assert!(matches!(mark_result, Ok(())), "{mark_result:?}");
    assert!(
        matches!(unmark_result, Err(HelperError::Refused(e)) if e == libc::EACCES),
        "{unmark_result:?}"
    );
}

/// Fix 3, item 2: an unsolicited `HydrateRequest` arriving on the wire
/// before the `Ack` for an outstanding call must not disturb that call's
/// pairing — the reader thread tells the two apart by message type, not
/// by position, so both resolve correctly regardless of what is
/// interleaved between them.
#[tokio::test]
async fn a_hydrate_request_interleaved_before_the_ack_still_resolves_both() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = seqpacket_listener(&socket);
    std::thread::spawn(move || {
        let stream = seqpacket_accept(&listener);
        let mut channel = Channel::new(stream).unwrap();
        channel
            .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
            .unwrap();
        ack_hello(&mut channel);

        let (message, _) = channel.recv::<ToHelper>().unwrap();
        assert!(matches!(message, ToHelper::MarkDir), "{message:?}");
        let payload = tempfile::tempfile().unwrap();
        channel
            .send(
                &ToDaemon::HydrateRequest { req_id: 99 },
                Some(std::os::fd::AsFd::as_fd(&payload)),
            )
            .unwrap();
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
    });
    let (link, mut requests) = HelperLink::connect(&socket).await.unwrap();
    let handle = std::fs::File::open(dir.path()).unwrap();

    let (mark_result, request) = tokio::join!(
        link.mark_dir(&handle),
        tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv()),
    );
    mark_result.unwrap();
    let request = request.unwrap().unwrap();
    assert_eq!(request.req_id, 99);
}

/// Fix 3, item 3 (and the direct proof of): a helper that
/// accepts, greets, reads a request, and then goes silent must make the
/// call fail with `HelperError::Timeout` rather than hang. The call
/// timeout is injected as a few milliseconds via `connect_with_timeout`
/// so this test does not have to sleep the real 30 s bound.
///: `UnregisterRoot` walks the whole
/// tree as `RegisterRoot` does — every directory, and now every file's
/// ignore mark — so it gets the same long bound. With the ordinary one,
/// a large tree's unregistration timed out, and a timeout ends the
/// connection and every hydration in flight on it.
#[tokio::test]
async fn an_unregistration_gets_the_walks_bound_not_the_ordinary_one() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = seqpacket_listener(&socket);
    std::thread::spawn(move || {
        let stream = seqpacket_accept(&listener);
        let mut channel = Channel::new(stream).unwrap();
        channel
            .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
            .unwrap();
        ack_hello(&mut channel);
        // A walk that takes longer than an ordinary call may.
        let _ = channel.recv::<ToHelper>().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(600));
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(10));
    });
    let (link, _requests) = HelperLink::connect_with_timeout(
        &socket,
        std::time::Duration::from_millis(200),
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();

    let result = link.unregister_root("some-root").await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn a_stuck_helper_times_out_rather_than_hanging_forever() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = seqpacket_listener(&socket);
    std::thread::spawn(move || {
        let stream = seqpacket_accept(&listener);
        let mut channel = Channel::new(stream).unwrap();
        channel
            .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
            .unwrap();
        ack_hello(&mut channel);
        // Reads the request and then simply never answers it — connected,
        // not closed, not crashed.
        let _ = channel.recv::<ToHelper>().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(10));
    });
    let (link, _requests) = HelperLink::connect_with_timeout(
        &socket,
        std::time::Duration::from_millis(200),
        std::time::Duration::from_millis(200),
    )
    .await
    .unwrap();
    let handle = std::fs::File::open(dir.path()).unwrap();

    let result =
        tokio::time::timeout(std::time::Duration::from_secs(5), link.mark_dir(&handle))
            .await
            .expect("must not hang forever waiting on a helper that never answers");
    assert!(matches!(result, Err(HelperError::Timeout)), "{result:?}");
}

/// A helper that accepts a connection and then sends
/// nothing — never even greets — must not hang `connect()` forever
/// either. Same short-bound injection as the call-timeout test above.
#[tokio::test]
async fn connect_times_out_rather_than_hanging_when_the_helper_never_greets() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = seqpacket_listener(&socket);
    std::thread::spawn(move || {
        let _stream = seqpacket_accept(&listener);
        // Accepted, then silence: no `Welcome`, ever.
        std::thread::sleep(std::time::Duration::from_secs(10));
    });

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        HelperLink::connect_with_timeout(
            &socket,
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(200),
        ),
    )
    .await
    .expect("must not hang forever waiting on a helper that never greets");
    let error = match result {
        Err(e) => e,
        Ok(_) => panic!("a helper that never greets must fail the connection"),
    };
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
}

/// The concrete failure the demonstrated: before
/// this fix, `connect()` read `Welcome` with an unbounded, raw blocking
/// `recvmsg` directly on the calling thread. Awaited on a
/// current-thread runtime against a helper that accepts and then sends
/// nothing, that starved every other task on the runtime — a concurrent
/// 50 ms ticker made zero progress. Now the only blocking recv in this
/// path lives on the dedicated reader thread, so a concurrent task on
/// the same runtime keeps making progress the whole time `connect()` is
/// waiting. `#[tokio::test]` defaults to a current-thread runtime, which
/// is what makes this test meaningful: on a multi-thread runtime the old
/// bug would not have shown up here at all.
#[tokio::test]
async fn connect_does_not_starve_the_runtime_while_waiting_on_a_silent_helper() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = seqpacket_listener(&socket);
    std::thread::spawn(move || {
        let _stream = seqpacket_accept(&listener);
        std::thread::sleep(std::time::Duration::from_secs(10));
    });

    let ticks = Arc::new(AtomicU32::new(0));
    let ticker = {
        let ticks = Arc::clone(&ticks);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                ticks.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    let _ = HelperLink::connect_with_timeout(
        &socket,
        std::time::Duration::from_millis(500),
        std::time::Duration::from_millis(500),
    )
    .await;

    ticker.abort();
    let seen = ticks.load(Ordering::Relaxed);
    assert!(seen >= 5, "the ticker made only {seen} ticks while connect() ran; the runtime was starved");
}

/// Made deterministic rather than diagnosed through `/proc`
/// (which the used only as a diagnostic, not as something to
/// assert on in the committed suite): cancelling `connect()` from
/// outside — here, via an outer `tokio::time::timeout` far shorter than
/// the connection's own 30 s handshake bound — must still shut the
/// connection down promptly, not leak the reader thread blocked in
/// `recv()` forever.
///
/// The fake helper never greets, so `connect_with_timeout` is certainly
/// still awaiting `Welcome` when the outer timeout fires and drops it.
/// The helper then blocks in its own `recv`; the only thing that can
/// ever unblock it is our side's connection actually being shut down
/// (`shutdown(Shutdown::Both)` propagates to the peer, which then reads
/// EOF). Once it does, the helper reports that back over a plain
/// channel. Asserting that signal arrives within a short bound after
/// the outer timeout fires is the proof: nothing but a prompt,
/// unconditional shutdown on our side — the `ShutdownOnDrop` guard's
/// destructor running even though `connect_with_timeout` was dropped
/// mid-`.await` rather than returning normally — could make the
/// helper's blocked `recv` return at all.
#[tokio::test]
async fn cancelling_connect_still_shuts_the_connection_down() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = seqpacket_listener(&socket);
    let (eof_tx, eof_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stream = seqpacket_accept(&listener);
        let mut channel = Channel::new(stream).unwrap();
        // Never greets: `connect_with_timeout` will still be waiting on
        // `Welcome` when the outer timeout below cancels it.
        let outcome = channel.recv::<ToHelper>();
        let _ = eof_tx.send(outcome.is_err());
    });

    let outer = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        HelperLink::connect_with_timeout(
            &socket,
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
        ),
    )
    .await;
    assert!(outer.is_err(), "the outer timeout should have cancelled connect() mid-handshake");

    let saw_eof = eof_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the helper's recv must unblock promptly once connect() is cancelled and dropped");
    assert!(saw_eof, "the helper's recv should fail with EOF, not succeed");
}

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
