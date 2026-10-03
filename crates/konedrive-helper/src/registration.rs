use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use konedrive_fs::probe::{probe_dir, ProbeError};
use konedrive_helper::{marks, roots};
use nix::fcntl::{openat2, OFlag, OpenHow, ResolveFlag};

use konedrive_helper::jobs::Owner;

use crate::shared::{lock, Refusal, Shared, ROOTS_FILE};

/// How many of a walk's failures are written out, one line each. A tree can
/// hold as many directories that cannot be covered as its owner likes; the
/// rest are counted in one line.
const FAILURES_SHOWN: usize = 16;

/// Writes a walk's failures: the first [`FAILURES_SHOWN`], and how many more
/// there are.
fn log_failures(failures: &[impl std::fmt::Display]) {
    for failure in failures.iter().take(FAILURES_SHOWN) {
        tracing::error!("  {failure}");
    }
    if failures.len() > FAILURES_SHOWN {
        tracing::error!("  and {} more", failures.len() - FAILURES_SHOWN);
    }
}

/// Opens an absolute path one component at a time, from a held `/`
/// descriptor, refusing to resolve through anything that is not a real
/// directory entry.
///
/// `RESOLVE_BENEATH` cannot be handed a multi-component absolute path in one
/// call, so the descent is explicit: each `openat2` resolves exactly one name
/// against the descriptor of the directory above it, and each one carries
/// `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`. That
/// removes the whole class the previous code left open — it resolved the
/// whole path in one call with only `RESOLVE_NO_MAGICLINKS`, so resolution
/// *did* follow symlinks and only the `(st_dev, st_ino)` comparison
/// afterwards stopped the attack.
///
/// # Why `RESOLVE_NO_XDEV` is not here, and where it is instead
///
/// All four flags belong together, and `marks::walk_and_mark` uses all four —
/// correctly, because below the root a mount point is a boundary the walk
/// must not cross. Getting *to* the root is the opposite case: a sync root
/// normally lives on a filesystem of its own. Measured on this host,
/// descending to `/home/<user>` from `/` with `RESOLVE_NO_XDEV` fails at the
/// first component with **`EXDEV`**, because `/home` is a separate mount —
/// so applying it here would make the helper unable to re-open the ordinary
/// sync root at every startup, which means no interception, which means
/// every placeholder reads as zeros. The cross-device question that actually
/// matters is answered instead by the `(st_dev, st_ino)` check below: the
/// directory reached must be the exact one that was registered, mount points
/// on the way in or not.
fn open_beneath(path: &str) -> io::Result<File> {
    let mut dir = File::open("/")?;
    let how = OpenHow::new()
        .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        .resolve(
            ResolveFlag::RESOLVE_BENEATH
                | ResolveFlag::RESOLVE_NO_SYMLINKS
                | ResolveFlag::RESOLVE_NO_MAGICLINKS,
        );
    for component in path.split('/').filter(|c| !c.is_empty()) {
        if component == "." || component == ".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path} is not a resolved absolute path"),
            ));
        }
        dir = File::from(openat2(dir.as_fd(), component, how)?);
    }
    Ok(dir)
}

/// Re-opens a registered root and proves it is still the same directory.
///
/// This is the check that stops a user having the helper mark directories it
/// was never given. The helper walks as root and reloads roots from a path
/// string, so between registration and the next startup that path can have
/// become something else entirely. Two things answer that: [`open_beneath`]
/// refuses to resolve through a symlink or a magic link at all, and the
/// `(st_dev, st_ino)` comparison then requires that what was reached is the
/// very directory that was registered.
///
/// The second check is deliberately not load-bearing on its own. Measured on
/// ext4, deleting a directory and creating another in the same parent reused
/// the same inode number on the first attempt (Btrfs and XFS did not), so
/// `(dev, ino)` equality is not proof of identity on every filesystem — which
/// is exactly why resolution is no longer allowed to wander.
fn reopen_and_verify(path: &str, dev: u64, ino: u64) -> io::Result<File> {
    let dir = open_beneath(path)?;
    let meta = dir.metadata()?;
    if meta.dev() != dev || meta.ino() != ino {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{path} is now dev={} ino={}, not the dev={dev} ino={ino} it was registered as",
                meta.dev(),
                meta.ino()
            ),
        ));
    }
    Ok(dir)
}

pub(crate) fn open_root(root: &roots::Root) -> io::Result<File> {
    let dir = reopen_and_verify(&root.path, root.dev, root.ino)?;
    if dir.metadata()?.uid() != root.uid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is no longer owned by uid {}", root.path, root.uid),
        ));
    }
    Ok(dir)
}

/// One unreadable subdirectory must never abort a root's walk, and
/// must never pass in silence either. Everything reachable is marked, every
/// failure is named, and the root is flagged degraded.
pub(crate) fn record_walk(shared: &Shared, root: &roots::Root, report: marks::WalkReport) {
    if !report.degraded() {
        tracing::info!("marked {} directories under {}", report.marked, root.path);
        lock(&shared.degraded_roots).remove(&root.root_id);
        return;
    }
    tracing::error!(
        "root {} ({}) is DEGRADED: {} directories marked, {} could not be covered; opens of \
         files in them will not be intercepted",
        root.root_id,
        root.path,
        report.marked,
        report.failures.len()
    );
    log_failures(&report.failures);
    lock(&shared.degraded_roots).insert(root.root_id.clone());
}

pub(crate) fn errno_of(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// Removes a registration **and the marks it put on the tree**.
///
/// Removing the entry alone was worse than doing nothing. Every directory in
/// the tree kept its `FAN_OPEN_PERM | FAN_EVENT_ON_CHILD` mark — confirmed by
/// `/proc/<helper>/fdinfo` being byte-identical across the call — so opens
/// inside it were still intercepted, and `handle_open` then had no daemon to
/// ask, because the uid no longer owns a registered root. A placeholder in an
/// unregistered tree was answered `EIO` in 164 µs. Unregistering a root has to
/// make its tree *uninteresting*, not *unreadable*.
///
/// The marks it put on the tree include the ignore marks on every file that
/// was hydrated there, and those come off too: left behind, one turns a later
/// dehydration that never sent a `ClearIgnore` into a file that reads zeros
/// once the folder is registered again (see `marks::walk_and_unmark`).
///
/// The state file is written before anything is unmarked, and the entry
/// leaves the helper's own list only once that write has succeeded: a restart
/// between the two would re-walk the root and put the marks back, which is a
/// correct, recoverable state. The reverse order — unmark, then fail to save
/// — would leave a root that is registered, walked at every startup, and
/// unmarked in between.
///
/// The write is not made under the roots lock (see [`Shared::roots_saving`]).
pub(crate) fn unregister_root(shared: &Shared, uid: u32, root_id: &str) -> i32 {
    let root = {
        let _saving = lock(&shared.roots_saving);
        let removed = lock(&shared.roots).without(uid, root_id);
        let Some((next, root)) = removed else {
            // Throttled, and the id not as it came: any local user can send
            // this as fast as it likes, with any 64 KiB it likes for an id.
            shared.refusals.report(Refusal::RootRefused, || {
                format!(
                    "uid {uid} tried to unregister root {}, which is not theirs",
                    roots::shown_id(root_id)
                )
            });
            return libc::EPERM;
        };
        if let Err(e) = next.save(Path::new(ROOTS_FILE)) {
            tracing::error!("cannot save {ROOTS_FILE}: {e}");
            return errno_of(&e);
        }
        *lock(&shared.roots) = next;
        root
    };

    // Outside the roots lock: the walk opens and marks its way through a whole
    // tree, and every other thread that wants to know whether a uid has a root
    // would be waiting behind it.
    uncover_root(shared, &root, "unregistered");
    lock(&shared.degraded_roots).remove(&root.root_id);
    0
}

/// Takes the marks off a tree whose registration has just gone: `how` says
/// which way, for the log — unregistered, or displaced by a registration of
/// the same id onto another directory.
///
/// Counted on both sides of the walk (second guard; see
/// `mark_while_hydrated`): whatever a worker or a finishing hydration read
/// before the walk ended, it does not mark behind it.
///
/// Failures here are logged and not returned: the registration *is* gone, the
/// state file already says so, and answering the daemon `EIO` would only
/// invite it to retry a request that would then be refused `EPERM` for a root
/// it no longer owns. What matters is that the condition is named, because a
/// tree left marked without a registration is one where every placeholder open
/// is denied.
fn uncover_root(shared: &Shared, root: &roots::Root, how: &str) {
    shared.unregistrations.bump(root.uid);
    unmark_tree(shared, root, how);
    shared.unregistrations.bump(root.uid);
}

fn unmark_tree(shared: &Shared, root: &roots::Root, how: &str) {
    let id = roots::shown_id(&root.root_id);
    let dir = match open_root(root) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::error!(
                "root {id} ({}) was {how}, but it could not be re-opened to remove its marks \
                 ({e}); if the directory is still there and still marked, opens inside it are \
                 still intercepted and will be denied EIO",
                root.path
            );
            return;
        }
    };
    let report = marks::walk_and_unmark(&shared.marks, dir.as_fd(), &root.path);
    if !report.degraded() {
        tracing::info!("root {id} ({}) {how}; unmarked {} directories", root.path, report.marked);
        return;
    }
    tracing::error!(
        "root {id} ({}) was {how} but {} of its marks could not be removed ({} directories were \
         unmarked); opens in a directory that kept its mark are still intercepted and will be \
         denied EIO, and a file that kept its ignore mark must not be dehydrated without a \
         ClearIgnore",
        root.path,
        report.failures.len(),
        report.marked
    );
    log_failures(&report.failures);
}

/// Says that a registration was refused, and returns the errno it is
/// answered. Throttled: every refusal is a request any local user can send
/// as fast as it likes.
fn refuse(shared: &Shared, uid: u32, refused: &roots::Refused, root_id: &str, path: &str) -> i32 {
    shared.refusals.report(Refusal::RootRefused, || match refused {
        roots::Refused::NotAnId => format!(
            "uid {uid} tried to register a root under {}, which is not a root id",
            roots::shown_id(root_id)
        ),
        roots::Refused::AnotherUsers => {
            format!("uid {uid} tried to register root id {root_id}, which belongs to another user")
        }
        roots::Refused::Overlap(conflict) => format!("{path} cannot be registered: {conflict:?}"),
        roots::Refused::TooMany => format!(
            "uid {uid} already holds {} roots; refusing another",
            roots::MAX_ROOTS_PER_UID
        ),
    });
    refused.errno()
}

/// The id must be a root id, and the directory must be owned by the peer,
/// live on a filesystem that can host placeholders, and neither contain nor
/// sit inside another registered root; and the peer must hold fewer than
/// [`roots::MAX_ROOTS_PER_UID`] roots, or this one already. Only then is it
/// stored, marked, and walked.
pub(crate) fn register_root(shared: &Shared, owner: Owner, root_id: String, dir: File) -> i32 {
    let uid = owner.uid;
    // Before anything else, and before the id is stored, compared or written
    // to the log: it is whatever string the peer sent, up to a whole
    // datagram of it.
    if !konedrive_proto::is_root_id(&root_id) {
        return refuse(shared, uid, &roots::Refused::NotAnId, &root_id, "");
    }
    let meta = match dir.metadata() {
        Ok(meta) => meta,
        Err(e) => return errno_of(&e),
    };
    if !meta.is_dir() || meta.uid() != uid {
        shared.refusals.report(Refusal::RootRefused, || {
            format!("uid {uid} offered a root it does not own, or that is not a directory")
        });
        return libc::EPERM;
    }

    // `root_id` is a string the client picks, so it names an
    // entry without owning one: before anything else, the entry it names must
    // be free or already ours. Without this, any local user could replace
    // another user's registration by reusing its id — and the victim's tree
    // would then go unwalked at the next restart, which is the "serve zeros"
    // outcome this whole component exists to prevent.
    //
    // And a uid that already holds every root it may is refused here, before
    // the directory is resolved and probed for it. Both are asked again by
    // the decision below, which is the one that counts.
    let (previous_owner, held) = {
        let roots = lock(&shared.roots);
        (roots.owner_of(&root_id), roots.held_by(uid))
    };
    if previous_owner.is_some_and(|other| other != uid) {
        return refuse(shared, uid, &roots::Refused::AnotherUsers, &root_id, "");
    }
    if previous_owner.is_none() && held >= roots::MAX_ROOTS_PER_UID {
        return refuse(shared, uid, &roots::Refused::TooMany, &root_id, "");
    }

    let path = match resolve_root_path(&dir, meta.dev(), meta.ino()) {
        Ok(path) => path,
        Err(errno) => return errno,
    };

    // Re-registering a root we already hold skips the write
    // probe. The directory is already marked from the first registration, so
    // creating the probe's temporary file inside it (`O_TMPFILE`, and so
    // nameless since, but still an `open` in a marked directory)
    // can raise a permission event aimed at this very helper (kernel fact 7
    // — `docs/kernel-behavior-7.2/interception.md` §7) while this thread is blocked
    // inside the probe. A worker answers it
    // today, but only because the pool is a separate thread set, and under a
    // saturated pool the probe is denied `EAGAIN` and a perfectly good
    // re-registration fails. The probe told us nothing new anyway: the
    // filesystem was probed when the root was first registered, and the type
    // check below is re-run either way.
    //
    // closes the last two ways into the same hazard: a directory
    // already registered under *another* id, and one lying inside somebody's
    // registered root, are both already marked, and both are about to be
    // refused `EINVAL` by the nesting check — but the probe ran first and so
    // wrote into a marked directory anyway. Asking the same question here,
    // before the probe, costs one lock and removes it. The authoritative
    // nesting check stays where it is, under the lock that inserts; this one
    // only decides whether to probe.
    //
    // `Contains` is deliberately not in the set: a directory that contains a
    // registered root sits *above* every mark, so probing in it is safe.
    let reregistration = previous_owner == Some(uid);
    let already_marked = reregistration
        || matches!(
            lock(&shared.roots).nesting_conflict(&path, meta.dev(), meta.ino()),
            Some(roots::Nesting::SameDirectory(_)) | Some(roots::Nesting::Inside(_))
        );
    let outcome = if already_marked {
        check_filesystem_type(&dir, &path)
    } else {
        check_filesystem(&dir, &path)
    };
    if let Err(errno) = outcome {
        return errno;
    }

    let root = roots::Root { uid, dev: meta.dev(), ino: meta.ino(), path, root_id };
    let displaced = {
        let _saving = lock(&shared.roots_saving);
        // Decided on the registrations as they are now. The checks above ran
        // before `resolve_root_path` and the filesystem checks, all of which
        // do I/O no lock is held across — and in that gap another connection
        // could have claimed this id, or taken the user's last free place.
        // Asking here, with `roots_saving` held so that nothing is registered
        // or unregistered until this is saved and in place, is what makes a
        // refusal airtight rather than merely likely.
        let decided = lock(&shared.roots).with(root.clone());
        let roots::Accepted { roots: next, displaced } = match decided {
            Ok(accepted) => accepted,
            Err(refused) => return refuse(shared, uid, &refused, &root.root_id, &root.path),
        };
        // Saved before it is in place, and not under the roots lock: nobody
        // sees a registration that is not on disk, and a save that fails
        // leaves nothing to put back.
        if let Err(e) = next.save(Path::new(ROOTS_FILE)) {
            tracing::error!("cannot save {ROOTS_FILE}: {e}");
            return errno_of(&e);
        }
        *lock(&shared.roots) = next;
        displaced
    };

    // The id was the user's already, on another directory: a folder replaced
    // at its path by a copy that kept its attributes, or the copy registered
    // beside it. The entry of the old directory is gone, so its tree is
    // unmarked as an unregistration would — left marked, its placeholders are
    // intercepted for a daemon that no longer knows them. Before the walk
    // below, because the two trees may overlap (only *other* roots were
    // compared), and what this takes off the new one the walk puts back. The
    // same directory announced again is not unwalked: its marks are the ones
    // it needs.
    if let Some(old) = displaced.filter(|old| (old.dev, old.ino) != (root.dev, root.ino)) {
        uncover_root(shared, &old, "displaced by a registration of its id onto another directory");
    }

    // A newly registered root is walked, not just marked at the top: it can
    // already have a whole tree in it (the daemon registers a folder the user
    // may have been syncing with something else, or one restored from a
    // backup), and every directory in it needs its own mark or nothing inside
    // it is intercepted.
    record_walk(shared, &root, marks::walk_and_mark(&shared.marks, dir.as_fd(), &root.path));
    0
}

/// The absolute path of a directory we were handed as a descriptor, verified
/// to lead back to that same directory.
///
/// `/proc/self/fd/<n>` is the only way to name a descriptor's path, and its
/// answer cannot be trusted as-is: for an unlinked directory it appends
/// `" (deleted)"`, which stored verbatim would be a path that never resolves.
/// So the answer is checked rather than believed — it must be absolute, it
/// must be representable (a path the helper cannot write into its JSON state
/// is refused at registration rather than mangled into something else), and
/// re-opening it must land on the very same `(dev, ino)`.
fn resolve_root_path(dir: &File, dev: u64, ino: u64) -> Result<String, i32> {
    let link = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let target = match std::fs::read_link(&link) {
        Ok(target) => target,
        Err(e) => {
            tracing::error!("cannot resolve the path of the offered root: {e}");
            return Err(libc::EINVAL);
        }
    };
    let Some(path) = target.to_str() else {
        tracing::error!(
            "the offered root has a path that is not valid UTF-8 ({}); refusing to register it",
            target.display()
        );
        return Err(libc::EINVAL);
    };
    if !path.starts_with('/') {
        tracing::error!("the offered root resolved to {path:?}, which is not an absolute path");
        return Err(libc::EINVAL);
    }

    // The same revalidation the startup walk does. A `" (deleted)"` suffix, a
    // path that goes through a symlink somebody can swap, a directory renamed
    // out from under us — all of them come out here as a mismatch, so the
    // stored path is one that has been shown to lead back to this directory
    // rather than one that was merely reported.
    if let Err(e) = reopen_and_verify(path, dev, ino) {
        tracing::error!("{path} does not lead back to the directory that was offered: {e}");
        return Err(libc::EINVAL);
    }
    Ok(path.to_owned())
}

/// Two checks, because they fail for different reasons and only one
/// of them works inside the helper's sandbox.
///
/// The filesystem type comes from `fstatfs` on the descriptor itself and is
/// authoritative: a network filesystem is refused outright, because a file
/// that can change on another machine cannot be intercepted here at all.
///
/// The feature probe writes a temporary file, and the helper's unit runs with
/// `ProtectHome=read-only`, so it can legitimately be refused permission to
/// write into a directory that is otherwise perfectly good. A genuine "this
/// filesystem does not implement hole punching / user xattrs" answer is fatal;
/// being refused the write is logged and the probe skipped — the daemon runs
/// its own probe in the user's own context (§10), and refusing every
/// registration because of our own sandbox would be worse than the check is
/// worth.
fn check_filesystem(dir: &File, path: &str) -> Result<(), i32> {
    check_filesystem_type(dir, path)?;

    // Probed through `/proc/self/fd/<n>` rather than by path, so the probe
    // lands in the exact directory we were handed and cannot be redirected by
    // swapping a component of the path.
    let through_fd = format!("/proc/self/fd/{}", dir.as_raw_fd());
    match probe_dir(Path::new(&through_fd)) {
        Ok(()) => Ok(()),
        Err(ProbeError::Missing { feature, .. }) => {
            tracing::error!("{path}: the filesystem does not support {feature}");
            Err(libc::EOPNOTSUPP)
        }
        Err(ProbeError::Unusable {
            why,
            errno: Some(libc::EROFS) | Some(libc::EACCES) | Some(libc::EPERM),
            ..
        }) => {
            tracing::warn!(
                "{path}: the helper's own sandbox stopped the feature probe ({why}); relying on \
                 the filesystem type check and the daemon's own probe instead"
            );
            Ok(())
        }
        Err(e) => {
            tracing::error!("{path}: {e}");
            Err(libc::EIO)
        }
    }
}

/// The half of §10's check that writes nothing.
///
/// `fstatfs` on the descriptor itself, so it is authoritative and cannot be
/// defeated by the helper's own sandbox, and it raises no fanotify event —
/// which is why this is the only half that runs at startup and on a
/// re-registration.
pub(crate) fn check_filesystem_type(dir: &File, path: &str) -> Result<(), i32> {
    // SAFETY: `dir` is an open descriptor and `buf` is a live, correctly sized
    // `statfs` that `fstatfs` fills in.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(dir.as_raw_fd(), &mut buf) } != 0 {
        let e = io::Error::last_os_error();
        tracing::error!("cannot identify the filesystem under {path}: {e}");
        return Err(errno_of(&e));
    }
    if let Some((_, name)) =
        REFUSED_FILESYSTEMS.iter().find(|(magic, _)| *magic == buf.f_type as i64)
    {
        tracing::error!(
            "{path} is on {name}, which konedrive cannot host placeholders on: its files can \
             change without any open on this machine, so interception would miss them"
        );
        return Err(libc::EOPNOTSUPP);
    }
    Ok(())
}

/// Filesystems a sync root may never live on. A denylist rather
/// than an allowlist: the hard requirements are sparse files with hole
/// punching and `user.*` xattrs, and `probe_dir` measures those directly on
/// whatever filesystem is actually there, so the type check only has to catch
/// the cases a probe cannot — a filesystem whose files can change behind our
/// back, which no local test can detect.
/// Magic numbers as `include/uapi/linux/magic.h` defines them.
const REFUSED_FILESYSTEMS: &[(i64, &str)] = &[
    (0x6969, "NFS"),
    (0xff53_4d42, "SMB/CIFS"),
    (0xfe53_4d42, "SMB2"),
    (0x6573_5546, "FUSE"),
    (0x4d44, "FAT"),
    (0x2011_bab0, "exFAT"),
    (0x5346_414f, "AFS"),
    (0x6b41_4653, "kAFS"),
    (0x00c3_6400, "Ceph"),
];

#[cfg(test)]
mod tests;
