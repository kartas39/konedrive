//! konedrive-helper: the only privileged part. It knows nothing about OneDrive.

mod connection;
mod events;
mod pool;
mod registration;
mod shared;

use konedrive_helper::{jobs, marks, roots};

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use konedrive_proto::SOCKET_PATH;
use nix::sys::socket::{
    bind, connect, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType,
    UnixAddr,
};

use connection::serve;
use events::event_loop;
use registration::{check_filesystem_type, open_root, record_walk};
use shared::{
    lock, Refusals, Registry, Shared, Unregistrations, EVENT_QUEUE_DEPTH, EVENT_WORKERS,
    FLUSH_EVERY, ROOTS_FILE,
};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    let shared = Arc::new(Shared {
        marks: Arc::new(marks::Marks::new()?),
        roots: Mutex::new(load_roots()),
        roots_saving: Mutex::new(()),
        jobs: Mutex::new(jobs::Jobs::default()),
        daemons: Mutex::new(Registry::default()),
        daemon_arrived: Condvar::new(),
        degraded_roots: Mutex::new(HashSet::new()),
        daemon_waiters: Mutex::new(HashMap::new()),
        refusals: Refusals::new(),
        unregistrations: Unregistrations::new(),
        connections: Arc::new(Mutex::new(HashMap::new())),
    });
    // The last count of a burst of refusals is written by this thread, a
    // moment after its interval ends, since no further refusal may come to
    // write it.
    let flushing = Arc::clone(&shared);
    std::thread::Builder::new().name("konedrive-log".into()).spawn(move || loop {
        std::thread::sleep(FLUSH_EVERY);
        flushing.refusals.flush();
    })?;

    // Everything that can fail and end the process comes before the first
    // mark. Marking first and then failing to build
    // the pool or bind the socket exited with the group open over marked
    // trees, and every open suspended in the meantime was released by the
    // kernel as allowed — onto placeholders nobody had filled. The workers
    // exist before anything can connect, so that the
    // first daemon to arrive never finds the event loop with nowhere to hand
    // work; nothing is accepted before the walk is done.
    let pool = pool::Pool::new(Arc::clone(&shared), EVENT_WORKERS, EVENT_QUEUE_DEPTH)?;
    let socket = listen()?;
    let (walked, walk_done) = std::sync::mpsc::channel::<()>();
    let accepting = Arc::clone(&shared);
    std::thread::Builder::new().name("konedrive-accept".into()).spawn(move || {
        if walk_done.recv().is_ok() {
            serve(accepting, socket);
        }
    })?;

    // Cover every registered root before anyone can open anything in it.
    // Sorted so that which of two overlapping roots wins is the same on every
    // boot rather than whatever order the map iterated in.
    let mut registered: Vec<roots::Root> = lock(&shared.roots).iter().cloned().collect();
    registered.sort_by(|a, b| a.root_id.cmp(&b.root_id));
    let mut covered = roots::Roots::default();
    for root in &registered {
        if let Some(conflict) = overlap_with(&covered, root) {
            // The checks that ran at registration are re-run
            // here, because the stored path is only a hint and what it leads
            // to can have changed since. Two roots that now overlap cannot
            // both be marked — an event in the shared part would belong to
            // neither daemon in particular — so the later one is left alone
            // and said so, loudly.
            tracing::error!(
                "root {} ({}) now overlaps root {conflict} and will NOT be covered; opens inside \
                 it are not intercepted until it is re-registered",
                roots::shown_id(&root.root_id),
                roots::shown_path(&root.path),
                conflict = roots::shown_id(&conflict)
            );
            lock(&shared.degraded_roots).insert(root.root_id.clone());
            continue;
        }
        if cover_root(&shared, root) {
            covered.insert(root.clone());
        }
    }

    // Nothing that can fail stands between the walk and the event loop but
    // the loop itself (§6.5).
    let _ = walked.send(());
    event_loop(&shared, &pool)
}

/// A helper that cannot read its registrations must still come up: with no
/// interception every placeholder in every sync folder reads as zeros, so
/// starting with nothing registered is strictly better than not starting.
/// `Roots::load` already moves a corrupt file aside; this covers the rest.
fn load_roots() -> roots::Roots {
    match roots::Roots::load(Path::new(ROOTS_FILE)) {
        Ok(roots) => roots,
        Err(e) => {
            tracing::error!(
                "cannot read {ROOTS_FILE}: {e}; starting with no registered roots — each daemon \
                 will re-register on its next connection"
            );
            roots::Roots::default()
        }
    }
}

/// Whether `root` overlaps anything already covered this startup: the
/// nesting rule (`docs/design/hydration.md` §11) applies at every boot, not
/// only at registration, because what a stored path leads to can change in
/// between.
fn overlap_with(covered: &roots::Roots, root: &roots::Root) -> Option<String> {
    covered.nesting_conflict(&root.path, root.dev, root.ino).map(|conflict| match conflict {
        roots::Nesting::Inside(id) | roots::Nesting::Contains(id) | roots::Nesting::SameDirectory(id) => id,
    })
}

/// Re-opens, re-checks and walks one registered root. Returns whether it is
/// now covered, so the caller knows whether to compare later roots against it.
fn cover_root(shared: &Shared, root: &roots::Root) -> bool {
    let dir = match open_root(root) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::error!("root {} is not covered: {e}", roots::shown_id(&root.root_id));
            lock(&shared.degraded_roots).insert(root.root_id.clone());
            return false;
        }
    };
    // The filesystem check is re-run too — a root can have been
    // moved onto a filesystem that cannot host placeholders since it was
    // registered. Only the `fstatfs` half: the feature probe writes a file,
    // and writing into every user's sync folder on every boot is both
    // unnecessary (it was probed at registration) and, once this root is
    // marked, exactly the self-interception hazard is about.
    if let Err(unusable) = check_filesystem_type(&dir, &root.path) {
        tracing::error!(
            "root {} is on a filesystem konedrive cannot use, and is not covered: {} (errno {})",
            roots::shown_id(&root.root_id),
            unusable.why,
            unusable.errno
        );
        lock(&shared.degraded_roots).insert(root.root_id.clone());
        return false;
    }
    record_walk(shared, root, marks::walk_and_mark(&shared.marks, dir.as_fd(), &root.path));
    true
}

/// Binds the control socket. This must be a genuine `SOCK_SEQPACKET` socket,
/// not the `SOCK_STREAM` that `std::os::unix::net::UnixListener` produces:
/// `konedrive_proto::Channel` frames exactly one message per datagram and
/// relies on that framing coming from the socket type itself. Two messages
/// sent back to back over a stream socket can coalesce into a single
/// `read()` — the first `recv` then fails to parse ("trailing characters")
/// and the second call blocks forever, since nothing else is ever going to
/// arrive. A `SOCK_SEQPACKET` socket keeps each `send()` as its own `recv()`,
/// so this cannot happen. (`Channel::new` now rejects the wrong socket type
/// outright; std has no `SOCK_SEQPACKET` listener, so the socket is built
/// directly with nix.)
fn listen() -> anyhow::Result<OwnedFd> {
    let path = Path::new(SOCKET_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let addr = UnixAddr::new(path)?;

    // A leftover socket file is not proof that nothing is listening on it, and
    // unlinking it unconditionally is how a second helper silently takes the
    // socket away from a running first one: the first keeps its bound
    // descriptor and its marks, and goes on owning every suspended open, while
    // every daemon now talks to the second, which has no idea those events
    // exist. Connecting is the only way to tell the two apart.
    if path.exists() {
        let probe = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)?;
        match connect(probe.as_raw_fd(), &addr) {
            Ok(()) => anyhow::bail!(
                "another konedrive-helper is already listening on {SOCKET_PATH}; refusing to \
                 take the socket away from it"
            ),
            Err(e) => tracing::info!("{SOCKET_PATH} is stale ({e}); replacing it"),
        }
        std::fs::remove_file(path)?;
    }

    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)?;
    bind(fd.as_raw_fd(), &addr)?;
    sock_listen(&fd, Backlog::new(16)?)?;
    // Anyone may connect; every request is authorised by SO_PEERCRED plus the
    // ownership rules in roots.rs.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o666))?;
    Ok(fd)
}
