use std::ffi::OsString;
use std::fs::File;
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;

use crate::sync::SyncService;
use crate::helper::HelperLink;
use crate::hydration::source::LocalDir;
use crate::sync::{RootSource, SyncError};
use crate::hydration::source;

impl SyncService {
    /// Mirrors `source_dir` into the root as placeholders: the
    /// offline stand-in for the real fill. Also remembers `source_dir` as
    /// this service's `ContentSource`, so `Hydrate()` (and any real
    /// interception-driven fill routed through `serve_hydrations`) has
    /// somewhere to fetch bytes from afterwards.
    pub async fn populate_from_directory(&self, source_dir: &Path) -> Result<u64, SyncError> {
        // The mode decides what is asked of the helper below, so it must not
        // change until this is done: the folder's state is held for reading.
        let folder = self.folder.read().await;
        let reg = folder.acted_on().cloned().ok_or(SyncError::NoRoot)?;
        if reg.source == RootSource::OneDrive {
            return Err(SyncError::Unsupported(
                "this folder shows your OneDrive; filling it from a directory is for a folder \
                 registered while signed out"
                    .into(),
            ));
        }
        // a source that overlaps the root is refused
        // — one inside it is a directory of placeholders, which a fill would
        // copy as zeros into a file it then stamps `hydrated`, and one around
        // it would be mirrored into itself. Compared by resolved path, so a
        // symlink to either is seen through; a bind mount is not — but a
        // file reached through one carries konedrive's attributes, and every
        // source file is refused on those, or on leading into the folder,
        // both when it is mirrored and when it is read.
        let named = source_dir.to_path_buf();
        let root = reg.root.path.clone();
        let (source, root) = on_blocking_thread(move || {
            Ok::<_, io::Error>((std::fs::canonicalize(&named)?, std::fs::canonicalize(&root)?))
        })
        .await
        .and_then(|resolved| resolved)
        .map_err(|e| SyncError::Io(format!("{}: {e}", source_dir.display())))?;
        if source.starts_with(&root) || root.starts_with(&source) {
            return Err(SyncError::Unsupported(format!(
                "{} {} the sync folder {}, and a folder cannot be filled from itself: its files \
                 there are placeholders, which would be copied as zeros",
                source.display(),
                if source.starts_with(&root) { "is inside" } else { "contains" },
                root.display()
            )));
        }
        // A root registered without interception is never
        // announced to the helper, and a `MarkDir` is an announcement. Sent
        // whenever a link merely existed, it failed the whole populate on a
        // filesystem where the uid owns no helper root (`EPERM`) and, where
        // it owns one, it landed: the directory was intercepted, and a folder
        // the user asked to leave alone was half intercepted. Both measured
        // in the VM suite.
        let link = if reg.intercepted() { self.link() } else { None };
        let walk = Walk { link: link.as_ref(), root: &root };
        let created = populate_walk(walk, &source, &reg.root.path, Path::new(""))
            .await
            .map_err(|e| match e.get_ref().and_then(|inner| inner.downcast_ref::<RefusedSource>()) {
                Some(RefusedSource(why)) => SyncError::Unsupported(why.clone()),
                None => SyncError::Io(format!("{}: {e}", source_dir.display())),
            })?;
        // The source refuses, when the bytes are read, any file
        // that leads into the folder by then — a symlink swapped since.
        *self.source.lock().unwrap() = Some(self.wiring.sources.directory(LocalDir::new(source).refusing_files_of(root)));
        Ok(created)
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What every level of [`populate_walk`] needs besides where it is: the
/// link to mark new directories through, if any, and the resolved sync
/// folder that no source file may lead into.
#[derive(Clone, Copy)]
struct Walk<'a> {
    link: Option<&'a HelperLink>,
    root: &'a Path,
}

/// A source file refused because it leads into the sync folder:
/// `PopulateFromDirectory` answers `Unsupported` with this text.
#[derive(Debug)]
struct RefusedSource(String);

impl std::fmt::Display for RefusedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RefusedSource {}

/// The recursive half of `populate_from_directory`: mirrors `source` into
/// `dest` as placeholders, marking every newly-created directory before
/// anything is created inside it (invariant M1) and skipping any name that
/// already exists. `relative` accumulates the `item_id` — the entry's path
/// relative to the original `source_dir` — as the walk descends.
///
/// This is the offline test path, not production-hardened infrastructure:
/// unlike `root::recover`, it walks
/// by path rather than by directory descriptor, because its threat model is
/// "a local directory the same user built for a test", not an adversarial
/// or racing filesystem. It still walks a directory the user names, though,
/// and a symlink is never treated as one to recurse into — `entry.file_type`
/// is `lstat`-based and already reports a symlink as a symlink rather than
/// as whatever it points at, but that is std's own default, not a decision
/// this function makes, so it is spelled out below rather than leaned on
/// implicitly: a symlink back at one of its own ancestors is exactly the
/// shape that turns "walk the tree" into recursion with no base case, and
/// nothing here may ever decide to recurse on the strength of what a
/// symlink's target happens to be. A symlink to a regular file is still
/// picked up as one, through the same follow `std::fs::metadata` performs
/// for a plain file's own size and mtime — that follows the link exactly
/// once (the kernel bounds the rest of any chain on its own), and is a
/// read, never a walk decision — unless it leads into the sync folder, or
/// to one of konedrive's own files anywhere (a hardlink to a placeholder):
/// then the whole populate is refused, since a file filled
/// from it would be filled with a placeholder's zeros.
fn populate_walk<'a>(
    walk: Walk<'a>,
    source: &'a Path,
    dest: &'a Path,
    relative: &'a Path,
) -> BoxFuture<'a, io::Result<u64>> {
    Box::pin(async move {
        let mut created = 0u64;
        let listed = source.to_path_buf();
        let entries = on_blocking_thread(move || list_source(&listed)).await??;
        for (name, file_type) in entries {
            let dest_path = dest.join(&name);
            let relative_path = relative.join(&name);
            if file_type.is_dir() {
                // Invariant M1, both halves. The directory is marked
                // **before** anything is created inside it, which is why the
                // mark happens here and the recursion below it. And it is
                // marked whether or not this run is the one that created it:
                // the version this replaces marked only inside
                // `if !dest_path.exists()`, so a crash between `create_dir`
                // and `mark_dir` left a directory that every later run
                // skipped — permanently unmarked, and everything under it
                // permanently uninterceptable, until the helper's next
                // startup walk happened to cover it.
                let target = dest_path.clone();
                let handle = on_blocking_thread(move || ensure_dir(&target)).await??;
                if let Some(link) = walk.link {
                    link.mark_dir(&handle).await.map_err(|e| {
                        io::Error::other(format!("cannot mark {}: {e}", dest_path.display()))
                    })?;
                }
                drop(handle);
                created +=
                    populate_walk(walk, &source.join(&name), &dest_path, &relative_path).await?;
            } else if file_type.is_file() || file_type.is_symlink() {
                let source_path = source.join(&name);
                let dest_dir = dest.to_path_buf();
                // The item id is the entry's path *relative to the source
                // root* — `sub/b.bin`, not `b.bin` — which is exactly the
                // name `LocalDir` resolves a fetch by. A bare file name
                // gives every nested placeholder an id that fetches nothing.
                let item_id = relative_path.to_string_lossy().into_owned();
                created +=
                    on_blocking_thread({
                        let root = walk.root.to_path_buf();
                        move || make_placeholder(&source_path, &root, &dest_dir, &name, &item_id)
                    })
                        .await??;
            }
        }
        Ok(created)
    })
}

/// Runs one blocking step of the populate walk off the reactor: `read_dir`,
/// `stat`, `create_dir`, `openat` and `linkat` are all
/// blocking syscalls, and this runs from a zbus dispatch task.
async fn on_blocking_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| io::Error::other(format!("the populate task failed: {e}")))
}

/// One source directory's entries, in a stable order. `file_type` is
/// `lstat`-based, so a symlink is reported as a symlink rather than as
/// whatever it points at.
fn list_source(source: &Path) -> io::Result<Vec<(OsString, std::fs::FileType)>> {
    let mut entries: Vec<_> = std::fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    entries
        .into_iter()
        .map(|e| Ok((e.file_name(), e.file_type()?)))
        .collect()
}

/// The destination directory, created if it is not there yet, opened as a
/// directory. `O_NOFOLLOW` so a symlink planted under that name is never
/// what gets marked and populated.
fn ensure_dir(path: &Path) -> io::Result<File> {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let flags = nix::fcntl::OFlag::O_RDONLY
        | nix::fcntl::OFlag::O_DIRECTORY
        | nix::fcntl::OFlag::O_NOFOLLOW
        | nix::fcntl::OFlag::O_CLOEXEC;
    let fd = nix::fcntl::open(path, flags, nix::sys::stat::Mode::empty())
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    Ok(File::from(fd))
}

/// Mirrors one source file as a placeholder; returns how many were created
/// (0 when the name is already taken, or the source is not a regular file).
fn make_placeholder(
    source_path: &Path,
    root: &Path,
    dest_dir: &Path,
    name: &OsString,
    item_id: &str,
) -> io::Result<u64> {
    if dest_dir.join(name).exists() {
        return Ok(0);
    }
    // A real regular file's own metadata, or — for a symlink — its
    // target's: `fs::metadata` follows, which is exactly what is wanted
    // here and never what decided whether to recurse. A symlink whose
    // target is a directory, that is dangling, or that points at anything
    // else, is left alone: `is_file()` on the target is the only thing that
    // turns a symlink into a placeholder.
    let Ok(meta) = std::fs::metadata(source_path) else {
        return Ok(0);
    };
    if !meta.is_file() {
        return Ok(0);
    }
    // A file that leads into the folder — a symlink to a
    // placeholder there, a hardlink to one — would be filled from that
    // placeholder's zeros. Refused before anything is created for it.
    if let Some(why) = source::refused_source_path(source_path, root)? {
        return Err(io::Error::other(RefusedSource(format!(
            "{} cannot be a source file: {why}, and a file filled from it would be filled with a \
             placeholder's zeros",
            source_path.display()
        ))));
    }
    let dir_handle = File::open(dest_dir)?;
    konedrive_fs::placeholder::create_placeholder(
        &dir_handle,
        &name.to_string_lossy(),
        item_id,
        meta.len(),
        meta.modified()?,
    )?;
    Ok(1)
}
