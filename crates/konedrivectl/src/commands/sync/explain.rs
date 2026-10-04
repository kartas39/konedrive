//! What the `sync` commands share: a path made absolute, a refusal explained in terms of the
//! user's own file, and the check that a folder a command touched does not need attention.

use anyhow::{anyhow, bail, Context as _};
use konedrive_dbus::accounts::FolderProxies;
use konedrive_dbus::Refusal;
use konedrivectl::text::files::refused_path;
use konedrivectl::text::folder::needs_attention_text;
use konedrivectl::text::refusals::{explain_sync_error_in, Context, SyncAction};

use crate::daemon::{Chosen, Daemon, Folders};

/// Passes a folder call's result through, turning a refusal into what the person running
/// this should read (`explain_sync_error_in`): matched by the D-Bus error name, and said in
/// terms of their own file.
pub(super) async fn explained<T>(
    daemon: &Daemon,
    chosen: &Chosen,
    proxy: &FolderProxies<'_>,
    action: SyncAction<'_>,
    result: zbus::Result<T>,
) -> anyhow::Result<T> {
    let error = match result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    // Two refusals name the registered folder, one depends on what it shows, and one ends
    // with how to start the helper. If even reading them fails, they simply do not; the
    // refusal is the thing to report.
    let root = proxy.folder.path().await.unwrap_or_default();
    let source = proxy.folder.source().await.unwrap_or_default();
    let helper = daemon.manager.helper_state().await.unwrap_or_default();
    let foreign = match action {
        SyncAction::Register(path) | SyncAction::RegisterWithoutInterception(path) => carries_a_drive(path),
        _ => false,
    };
    let prefix = chosen.prefix();
    let context = Context { root: &root, source: &source, helper: &helper, folders: &[], foreign, prefix: &prefix };
    Err(anyhow!("{}", explain_sync_error_in(action, &error, context)))
}

/// Passes a `Files` call's result on `paths` through, as [`explained`] does. `Files` finds
/// the account by the path, so the refusal is explained against the folder that holds the
/// path it is about — the one a `NotAllowed` names, else the first in no folder, else the
/// first given — and, for a path in none, against every account's folder.
pub(super) async fn explained_paths<T>(
    daemon: &Daemon,
    action: PathAction,
    paths: &[String],
    result: zbus::Result<T>,
) -> anyhow::Result<T> {
    let error = match result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    let folders = daemon.folders().await.unwrap_or_default();
    let refused = refused_path(&error).map(str::to_owned).or_else(|| {
        let outside = Refusal::from_error(&error) == Some(Refusal::OutsideRoot);
        paths.iter().find(|path| outside && folders.holder(path).is_none()).cloned()
    });
    let named = refused.clone().unwrap_or_else(|| paths.join(", "));
    let about = refused.as_deref().or(paths.first().map(String::as_str)).unwrap_or_default();
    let held_by = folders.holder(about);
    let root = held_by.map(|f| f.root.clone()).unwrap_or_default();
    let source = match held_by {
        Some(folder) => folder.sync.folder.source().await.unwrap_or_default(),
        None => String::new(),
    };
    let helper = daemon.manager.helper_state().await.unwrap_or_default();
    let roots = folders.roots();
    let prefix = folders.prefix(held_by);
    let context = Context { root: &root, source: &source, helper: &helper, folders: &roots, foreign: false, prefix: &prefix };
    Err(anyhow!("{}", explain_sync_error_in(action.about(&named), &error, context)))
}

/// A `Files` call, for [`explained_paths`].
#[derive(Clone, Copy)]
pub(super) enum PathAction {
    Hydrate,
    Dehydrate,
    Pin,
    Unpin,
    Free,
    Open,
}

impl PathAction {
    /// The call, about `named`: the path its refusal is about, or the paths given.
    fn about(self, named: &str) -> SyncAction<'_> {
        match self {
            PathAction::Hydrate => SyncAction::Hydrate(named),
            PathAction::Dehydrate => SyncAction::Dehydrate(named),
            PathAction::Pin => SyncAction::Pin(named),
            PathAction::Unpin => SyncAction::Unpin(named),
            PathAction::Free => SyncAction::Free(named),
            PathAction::Open => SyncAction::Open(named),
        }
    }
}

/// Whether the folder at `path` carries a drive (`user.konedrive.drive`, design §8.3): a
/// registration of it refused `NotEmpty` was refused because it is another account's
/// folder. An empty value is no drive, as the daemon reads it. The link itself, if it is
/// one: the daemon refuses links anyway.
fn carries_a_drive(path: &str) -> bool {
    let (Ok(path), Ok(name)) = (std::ffi::CString::new(path), std::ffi::CString::new("user.konedrive.drive")) else {
        return false;
    };
    // SAFETY: both are NUL-terminated strings that live across the call; a null buffer of
    // size 0 asks only for the value's size.
    unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) > 0 }
}

/// `Some(detail)` when the folder's state is `error` after the call just made: `LastError`
/// is the detail. Every command that changes something checks this after its own call went
/// through: any of them can run while the folder is unhealthy already (a helper that
/// dropped, a recovery that could not finish), and that must show in the command's exit
/// status though the thing it asked for was done.
pub(super) async fn folder_trouble(proxy: &FolderProxies<'_>) -> anyhow::Result<Option<String>> {
    if proxy.folder.state().await? == "error" {
        Ok(Some(proxy.folder.last_error().await?))
    } else {
        Ok(None)
    }
}

/// Fails with the detail when [`folder_trouble`] finds one. For the commands whose own
/// success line is true anyway (a folder really was forgotten) while the folder as a whole
/// may still need attention.
pub(super) async fn fail_if_folder_unhealthy(proxy: &FolderProxies<'_>) -> anyhow::Result<()> {
    if let Some(detail) = folder_trouble(proxy).await? {
        bail!("{}", needs_attention_text(&proxy.folder.path().await.unwrap_or_default(), &detail));
    }
    Ok(())
}

/// [`fail_if_folder_unhealthy`] for every folder that holds one of `paths`.
pub(super) async fn fail_if_holders_unhealthy(folders: &Folders, paths: &[String]) -> anyhow::Result<()> {
    let mut checked: Vec<&str> = Vec::new();
    for folder in paths.iter().filter_map(|path| folders.holder(path)) {
        if checked.contains(&folder.root.as_str()) {
            continue;
        }
        checked.push(&folder.root);
        if let Some(detail) = folder_trouble(&folder.sync).await? {
            bail!("{}", needs_attention_text(&folder.root, &detail));
        }
    }
    Ok(())
}

/// `path` as the daemon takes it: absolute. The directory the name is in is resolved; the
/// name itself is not, so that a symbolic link given as the last component reaches the
/// daemon as the link. The daemon opens every path it is handed with `O_NOFOLLOW` and
/// refuses a link as a folder to register; resolved here, `sync register <link>` would
/// register the link's target instead.
pub(super) fn absolute_str(path: &str) -> anyhow::Result<String> {
    let given = std::path::Path::new(path);
    std::fs::symlink_metadata(given).with_context(|| format!("no such path: {path}"))?;
    let absolute = match (given.parent(), given.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() { std::path::Path::new(".") } else { parent };
            std::fs::canonicalize(parent).with_context(|| format!("no such path: {path}"))?.join(name)
        }
        // `/`, `.`, `..` and the like: no last name to keep.
        _ => std::fs::canonicalize(given).with_context(|| format!("no such path: {path}"))?,
    };
    absolute.to_str().map(str::to_owned).context("non-UTF-8 path")
}
