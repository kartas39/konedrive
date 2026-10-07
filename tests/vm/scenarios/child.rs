use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION, SOCKET_PATH};
use nix::sys::socket::{
    connect, socket, AddressFamily, SockFlag, SockType, UnixAddr,
};

// ---------------------------------------------------------------------------
// child modes
// ---------------------------------------------------------------------------

/// Opens a file and copies it to stdout, exiting with the errno on failure.
/// This is the only thing in the suite that performs an open meant to be
/// intercepted.
pub(crate) fn child_read(path: &Path) {
    match std::fs::File::open(path) {
        Ok(mut file) => {
            let mut content = Vec::new();
            if let Err(e) = file.read_to_end(&mut content) {
                std::process::exit(e.raw_os_error().unwrap_or(5));
            }
            let _ = std::io::stdout().write_all(&content);
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
        Err(e) => std::process::exit(e.raw_os_error().unwrap_or(5)),
    }
}

/// Opens a file, says so, and keeps the descriptor until stdin closes. A
/// second descriptor on a file is what `F_SETLEASE` refuses, which is how
/// "dehydrate while open is refused" is driven from outside this process.
pub(crate) fn child_hold(path: &Path) {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            println!("error {}", e.raw_os_error().unwrap_or(5));
            let _ = std::io::stdout().flush();
            std::process::exit(e.raw_os_error().unwrap_or(5));
        }
    };
    println!("held");
    let _ = std::io::stdout().flush();
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    drop(file);
    std::process::exit(0);
}

/// Opens `dir/burst-<i>` for every `i` below `count`, all at once, one thread
/// each, and reports what every one of them got. One process rather than
/// `count` processes: several thousand of those would measure the guest's
/// memory, and what is under test is the helper's bounded pool.
pub(crate) fn child_burst(dir: &Path, count: usize, expect: u8) {
    let (tx, rx) = mpsc::channel::<(usize, Result<usize, i32>, u64)>();
    let mut handles = Vec::with_capacity(count);
    for i in 0..count {
        let path = dir.join(format!("burst-{i}"));
        let tx = tx.clone();
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || {
                let started = Instant::now();
                let outcome = match std::fs::File::open(&path) {
                    Ok(mut file) => {
                        let mut content = Vec::new();
                        match file.read_to_end(&mut content) {
                            // `wrong` counts the bytes that are not what the
                            // payload says they should be: an opener that was
                            // let through onto an unfilled placeholder reads
                            // zeros, and that is the outcome worth counting
                            // separately from a clean failure.
                            Ok(_) => Ok(content.iter().filter(|b| **b != expect).count()),
                            Err(e) => Err(e.raw_os_error().unwrap_or(5)),
                        }
                    }
                    Err(e) => Err(e.raw_os_error().unwrap_or(5)),
                };
                let _ = tx.send((i, outcome, started.elapsed().as_millis() as u64));
            })
            .expect("a burst thread");
        handles.push(handle);
    }
    drop(tx);

    let mut ok = 0usize;
    let mut wrong = 0usize;
    let mut errors: BTreeMap<i32, usize> = BTreeMap::new();
    let mut answered = 0usize;
    while let Ok((i, outcome, waited)) = rx.recv() {
        answered += 1;
        match outcome {
            Ok(0) => {
                ok += 1;
                println!("TOOK {i} {waited}");
            }
            Ok(_) => {
                wrong += 1;
                println!("WRONG {i} {waited}");
            }
            Err(errno) => {
                *errors.entry(errno).or_default() += 1;
                println!("FAILED {i} {errno} {waited}");
            }
        }
    }
    for handle in handles {
        let _ = handle.join();
    }
    let errs: Vec<String> = errors.iter().map(|(e, n)| format!("{e}:{n}")).collect();
    println!("BURST answered={answered} ok={ok} wrong={wrong} errs={}", errs.join(","));
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

/// A second, unprivileged uid with nothing but the 0666 control socket. It
/// tries the three things that would be catastrophic if they worked, and
/// prints what the helper answered. `Command::uid` put it here; nothing in
/// this process is trusted by the helper.
pub(crate) fn child_hostile(root_id: &str) {
    let mut channel = match raw_connect() {
        Ok(channel) => channel,
        Err(e) => {
            println!("CONNECT-FAILED {e}");
            std::process::exit(1);
        }
    };
    // The helper greets unprompted.
    match channel.recv::<ToDaemon>() {
        Ok((ToDaemon::Welcome { version }, _)) if version == PROTOCOL_VERSION => {}
        other => {
            println!("NO-WELCOME {other:?}");
            std::process::exit(1);
        }
    }

    // 1. Force-allow somebody else's suspended opens by guessing request ids.
    //    A request id is a small sequential integer, so guessing is trivial;
    //    what must stop it is the job's recorded owner.
    let mut acks = Vec::new();
    for req_id in 1..=64u64 {
        if channel.send(&ToHelper::HydrateDone { req_id, errno: 0 }, None).is_err() {
            break;
        }
        match channel.recv::<ToDaemon>() {
            Ok((ToDaemon::Ack { errno }, _)) => acks.push(errno),
            _ => break,
        }
    }
    println!("HYDRATEDONE-ACKS {}", acks.len());

    // 2. Unregister the victim's root by id.
    let _ = channel.send(&ToHelper::UnregisterRoot { root_id: root_id.to_owned() }, None);
    println!("UNREGISTER {}", ack_of(&mut channel));

    // 3. Take the mark off the victim's root directory. The directory is
    //    world-readable, and opening a directory raises no event, so getting
    //    the descriptor costs nothing — only the helper's own authorisation
    //    stands between this and every placeholder in that tree becoming
    //    uninterceptable.
    let victim_root = std::env::var("KONEDRIVE_VICTIM_ROOT").unwrap_or_default();
    match std::fs::File::open(&victim_root) {
        Ok(dir) => {
            let _ = channel.send(&ToHelper::UnmarkDir, Some(dir.as_fd()));
            println!("UNMARKDIR {}", ack_of(&mut channel));
        }
        Err(e) => println!("UNMARKDIR open-failed {e}"),
    }
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

/// Sends `count` requests before reading a single reply, then reads them all,
/// then asks once more. What it prints is what the helper did to a peer that
/// is slow to read its `Ack`s: `ACKS <n>` is how many replies
/// arrived before the connection ended or the count was reached, and the last
/// line says whether the connection was still there afterwards.
///
/// The requests are `Hello`s, which the helper answers from its own state and
/// which touch nothing, so any uid may send them and none of them changes
/// anything but the socket.
pub(crate) fn child_pipeline(count: usize) {
    let mut writer = match raw_connect() {
        Ok(channel) => channel,
        Err(e) => {
            println!("CONNECT-FAILED {e}");
            std::process::exit(1);
        }
    };
    let mut reader = match writer.get_ref().try_clone().map(Channel::new) {
        Ok(Ok(channel)) => channel,
        _ => {
            println!("CONNECT-FAILED cannot split the connection");
            std::process::exit(1);
        }
    };
    match reader.recv::<ToDaemon>() {
        Ok((ToDaemon::Welcome { .. }, _)) => {}
        other => {
            println!("NO-WELCOME {other:?}");
            std::process::exit(1);
        }
    }
    let sender = std::thread::spawn(move || {
        let mut sent = 0usize;
        for _ in 0..count {
            if writer.send(&ToHelper::Hello { version: PROTOCOL_VERSION }, None).is_err() {
                break;
            }
            sent += 1;
        }
        (sent, writer)
    });
    // Long enough for every buffer between the two ends to fill: the helper's
    // outbox, the socket in both directions, and whatever the helper's reader
    // is holding.
    std::thread::sleep(Duration::from_secs(2));
    let _ = reader.get_ref().set_read_timeout(Some(Duration::from_secs(10)));
    let mut acks = 0usize;
    let mut refused = 0usize;
    while acks < count {
        match reader.recv::<ToDaemon>() {
            Ok((ToDaemon::Ack { errno }, _)) => {
                acks += 1;
                if errno != 0 {
                    refused += 1;
                }
            }
            Ok((other, _)) => {
                println!("UNEXPECTED {other:?}");
                break;
            }
            Err(e) => {
                println!("ENDED {e}");
                break;
            }
        }
    }
    let (sent, mut writer) = sender.join().expect("the sender thread");
    println!("SENT {sent}");
    println!("ACKS {acks} REFUSED {refused}");
    let alive = writer.send(&ToHelper::Hello { version: PROTOCOL_VERSION }, None).is_ok()
        && matches!(reader.recv::<ToDaemon>(), Ok((ToDaemon::Ack { errno: 0 }, _)));
    println!("{}", if alive { "ALIVE" } else { "DEAD" });
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

/// Opens `count` connections to the helper, one after another, keeping every
/// one of them open, and prints how many were greeted. A connection the
/// helper refuses is closed without a `Welcome`.
pub(crate) fn child_connections(count: usize) {
    let mut held = Vec::new();
    let mut greeted = 0usize;
    for _ in 0..count {
        let Ok(mut channel) = raw_connect() else { continue };
        let _ = channel.get_ref().set_read_timeout(Some(Duration::from_secs(2)));
        if let Ok((ToDaemon::Welcome { .. }, _)) = channel.recv::<ToDaemon>() {
            greeted += 1;
        }
        held.push(channel);
    }
    println!("GREETED {greeted} OF {count}");
    let _ = std::io::stdout().flush();
    drop(held);
    std::process::exit(0);
}

fn ack_of(channel: &mut Channel) -> String {
    match channel.recv::<ToDaemon>() {
        Ok((ToDaemon::Ack { errno }, _)) => format!("errno={errno}"),
        Ok((other, _)) => format!("unexpected={other:?}"),
        Err(e) => format!("no-ack={e}"),
    }
}

pub(crate) fn raw_connect() -> std::io::Result<Channel> {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)?;
    let addr = UnixAddr::new(Path::new(SOCKET_PATH))?;
    connect(std::os::fd::AsRawFd::as_raw_fd(&fd), &addr)?;
    Channel::new(UnixStream::from(fd))
}
