//! The testable part of `konedrivectl`.

use std::time::Duration;

use konedrive_dbus::{error_name, Account1Proxy, Sync1Proxy, ERROR_PREFIX};

pub async fn status_text(proxy: &Account1Proxy<'_>) -> zbus::Result<String> {
    let state = proxy.state().await?;
    let client_id = proxy.client_id().await?;
    let mut out = format!("{:<12}{state}\n", "State:");
    let shown_id = if client_id.is_empty() { "(not set)" } else { client_id.as_str() };
    out.push_str(&format!("{:<12}{shown_id}\n", "Client ID:"));
    if state == "signed-in" {
        out.push_str(&format!(
            "{:<12}{} <{}>\n",
            "Account:",
            proxy.display_name().await?,
            proxy.email().await?
        ));
        out.push_str(&format!(
            "{:<12}{} of {} used\n",
            "Storage:",
            human_bytes(proxy.quota_used().await?),
            human_bytes(proxy.quota_total().await?)
        ));
    }
    let last_error = proxy.last_error().await?;
    if !last_error.is_empty() {
        out.push_str(&format!("{:<12}{last_error}\n", "Last error:"));
    }
    Ok(out)
}

/// The account status's layout, with a wider label column: `Always on this
/// device:` is the longest label.
///
/// `RootState` is `none` on an ordinary machine that has never registered a
/// folder — this prints as an unremarkable "(none)", not an error. `error`
/// means a root is registered but something needs attention (startup
/// recovery could not finish, or could not even run, including a recovery
/// that finished with files it could not fix); `LastError` then carries the
/// detail and is always printed alongside it, so `error` can never be
/// mistaken for `ready` by someone scanning quickly.
///
/// A registered folder also gets an `Opens:` line, in this CLI's own words,
/// saying whether anything fills a file when it is opened. That matters
/// most for `no-interception`, the developer's mode: without the helper, a
/// file that is not downloaded reads as zeros, and that has to be on screen
/// every time, not left to the user's memory or to whatever `LastError`
/// happens to say.
///
/// `Helper:` says how the privileged helper stands (`HelperState`, HS4) and,
/// when it is not connected, how to install, start or look at it — whether
/// or not a folder is registered.
///
/// `Last checked:` is added for a folder that shows OneDrive — "20 s
/// ago", or "never" — and `On this computer:` for any registered folder:
/// what its files take on this disk — and `Always on this device:`, how
/// many files and folders are pinned (`konedrivectl sync pin`).
/// `Conflicts:` says how many local versions were moved out of the way,
/// when there are any: they are not a problem, so `LastError` does not carry
/// them.
pub async fn sync_status_text(proxy: &Sync1Proxy<'_>) -> zbus::Result<String> {
    const W: usize = 24;
    let path = proxy.root_path().await?;
    let state = proxy.root_state().await?;
    let shown = if path.is_empty() { "(none)" } else { path.as_str() };
    let mut out = format!("{:<W$}{shown}\n", "Folder:");
    out.push_str(&format!("{:<W$}{state}\n", "State:"));
    if let Some(opens) = opens_line(&state) {
        out.push_str(&format!("{:<W$}{opens}\n", "Opens:"));
    }
    out.push_str(&format!("{:<W$}{}\n", "Helper:", helper_text(&proxy.helper_state().await?)));
    let last_error = proxy.last_error().await?;
    if !last_error.is_empty() {
        out.push_str(&format!("{:<W$}{last_error}\n", "Last error:"));
    }
    if proxy.root_source().await? == "onedrive" {
        let (listed, placed, skipped) =
            (proxy.items_listed().await?, proxy.items_placed().await?, proxy.skipped_count().await?);
        out.push_str(&format!("{:<W$}{listed} in OneDrive, {placed} in the folder\n", "Items:"));
        if skipped > 0 {
            out.push_str(&format!("{:<W$}{skipped} (see `konedrivectl sync skipped`)\n", "Skipped:"));
        }
        let checked = checked_text(proxy.last_checked().await?, unix_now());
        out.push_str(&format!("{:<W$}{checked}\n", "Last checked:"));
        out.push_str(&format!(
            "{:<W$}not in this phase: the folder is read-only, and nothing is sent to OneDrive\n",
            "Editing:"
        ));
    }
    if !path.is_empty() {
        out.push_str(&format!("{:<W$}{}\n", "On this computer:", human_bytes(proxy.local_bytes().await?)));
        out.push_str(&format!("{:<W$}{}\n", "Always on this device:", proxy.pinned_count().await?));
    }
    let conflicts = proxy.conflict_count().await?;
    if conflicts > 0 {
        out.push_str(&format!("{:<W$}{conflicts} (see `konedrivectl sync conflicts`)\n", "Conflicts:"));
    }
    Ok(out)
}

/// What happens when something opens a file in a folder in `state`, for the
/// states where that is known: `ready` (the helper intercepts and fills) and
/// `no-interception` (nothing does). `error` can mean either — `LastError`
/// says which — and `none` has no folder to talk about.
fn opens_line(state: &str) -> Option<&'static str> {
    match state {
        "ready" => Some("intercepted: a file is downloaded when something opens it"),
        "no-interception" => Some(
            "NOT intercepted: a file that is not downloaded reads as zeros until you run \
             `konedrivectl sync hydrate <file>`",
        ),
        "listing" => Some("the folder is being filled with your OneDrive's items"),
        _ => None,
    }
}

/// The `sync` subcommand a refusal answered, with the path it named — the
/// context a refusal needs to be explained in terms of *this* user's file.
#[derive(Debug, Clone, Copy)]
pub enum SyncAction<'a> {
    Register(&'a str),
    RegisterWithoutInterception(&'a str),
    Forget,
    PopulateFrom(&'a str),
    Hydrate(&'a str),
    Dehydrate(&'a str),
    Refresh,
    Skipped,
    Activity,
    Conflicts,
    Dismiss(&'a str),
    FreeUpSpace,
    /// The paths given, joined with ", ".
    Pin(&'a str),
    /// The path refused ([`refused_path`]), or the paths given, joined with ", ".
    Unpin(&'a str),
    /// The path refused ([`refused_path`]), or the paths given, joined with ", ".
    Free(&'a str),
}

impl SyncAction<'_> {
    /// "downloading /path", for the one sentence every refusal without a
    /// name of its own is built from.
    fn doing(&self) -> String {
        match self {
            Self::Register(path) => format!("registering {path}"),
            Self::RegisterWithoutInterception(path) => {
                format!("registering {path} without interception")
            }
            Self::Forget => "forgetting the sync folder".to_owned(),
            Self::PopulateFrom(dir) => format!("filling the sync folder from {dir}"),
            Self::Hydrate(path) => format!("downloading {path}"),
            Self::Dehydrate(path) => format!("freeing up {path}"),
            Self::Refresh => "asking OneDrive for changes".to_owned(),
            Self::Skipped => "listing what is skipped".to_owned(),
            Self::Activity => "reading what happened".to_owned(),
            Self::Conflicts => "listing the conflicts".to_owned(),
            Self::Dismiss(path) => format!("dismissing the conflict {path}"),
            Self::FreeUpSpace => "freeing up space".to_owned(),
            Self::Pin(paths) => format!("keeping {paths} on this device"),
            Self::Unpin(paths) => format!("no longer keeping {paths} on this device"),
            Self::Free(paths) => format!("freeing up {paths}"),
        }
    }

    fn path(&self) -> &str {
        match self {
            Self::Register(path)
            | Self::RegisterWithoutInterception(path)
            | Self::PopulateFrom(path)
            | Self::Hydrate(path)
            | Self::Dehydrate(path)
            | Self::Dismiss(path)
            | Self::Pin(path)
            | Self::Unpin(path)
            | Self::Free(path) => path,
            Self::Forget | Self::Refresh | Self::Skipped | Self::Activity | Self::Conflicts | Self::FreeUpSpace => "",
        }
    }
}

/// What to tell a person when a `Sync1` call made for `action` failed.
///
/// Matches the D-Bus error **name** (`konedrive_dbus::error_name`), never the
/// message: every refusal `Sync1` makes arrives under
/// `konedrive_dbus::ERROR_PREFIX`, and each gets a sentence saying what
/// happened to the user's file and what they can do about it. `root` is the
/// registered folder (`RootPath`, possibly empty), which two refusals name.
///
/// The daemon's own message is kept only where it is the specific part:
/// `Unsupported` (which filesystem feature is missing) and anything with no
/// name of its own (`Failed`, or an error from the bus itself), where it is
/// all there is.
pub fn explain_sync_error(action: SyncAction<'_>, error: &zbus::Error, root: &str) -> String {
    let detail = match error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    };
    refusal_text(action, error_name(error), &detail, root)
}

/// What the CLI knows of the daemon besides a refusal, read after it: the
/// registered folder (`RootPath`), what it shows (`RootSource`) and the
/// helper (`HelperState`). Any of them may be empty.
#[derive(Debug, Default, Clone, Copy)]
pub struct Context<'a> {
    pub root: &'a str,
    pub source: &'a str,
    pub helper: &'a str,
}

/// [`explain_sync_error`], with what [`Context`] adds: a refusal for want
/// of the helper ends with how to start it (HS4), and a folder that shows
/// OneDrive is not told to populate itself from a directory (B-M6).
pub fn explain_sync_error_in(action: SyncAction<'_>, error: &zbus::Error, context: Context<'_>) -> String {
    let detail = match error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    };
    refusal_text_in(action, error_name(error), &detail, context)
}

/// [`explain_sync_error_in`]'s decision, on the name and message alone.
pub fn refusal_text_in(action: SyncAction<'_>, name: Option<&str>, detail: &str, context: Context<'_>) -> String {
    let refusal = name.and_then(|name| name.strip_prefix(ERROR_PREFIX)).and_then(|rest| rest.strip_prefix('.'));
    let text = refusal_text(action, name, detail, context.root);
    match (refusal, konedrive_dbus::helper_advice(context.helper)) {
        // A folder that shows OneDrive downloads from OneDrive; it has no
        // source yet only while it waits to be brought up.
        (Some("NoSource"), _) if context.source == "onedrive" => {
            "the folder is not connected yet; try again in a moment".to_owned()
        }
        (Some("NoHelper"), Some(advice)) => format!("{text}. {}", sentence(advice)),
        _ => text,
    }
}

/// `text` with its first letter in capitals, to stand as a sentence of its own.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The `Helper:` line's text (HS4): `HelperState`, and what to do about it
/// when the helper is not connected — the same words `LastError` uses.
pub fn helper_text(state: &str) -> String {
    match konedrive_dbus::helper_advice(state) {
        Some(advice) => format!("{state} — {advice}"),
        None => state.to_owned(),
    }
}

/// [`explain_sync_error`]'s decision, on the name and message alone — so it
/// can be tested with a name and a message that disagree.
pub fn refusal_text(action: SyncAction<'_>, name: Option<&str>, detail: &str, root: &str) -> String {
    use SyncAction::*;
    let refusal = name
        .and_then(|name| name.strip_prefix(ERROR_PREFIX))
        .and_then(|rest| rest.strip_prefix('.'));
    let path = action.path();
    let folder = if root.is_empty() { String::new() } else { format!(" ({root})") };
    match (refusal, action) {
        (Some("NotSignedIn"), _) => format!(
            "nobody is signed in, and `konedrivectl sync register` binds the folder to the \
             signed-in OneDrive account. Sign in first with `konedrivectl login` — or, in the \
             developer's mode with local files and no account, use `konedrivectl sync \
             register-without-interception {path}`: without the helper, files that are not \
             downloaded read as zeros until you hydrate them"
        ),
        // In a folder registered without interception too: a
        // helper that is running while this daemon has no connection to it
        // may hold a mark on the file that nothing can clear.
        (Some("NoHelper"), Dehydrate(_)) => format!(
            "the konedrive helper is not connected, so nothing was changed. Freeing up {path} \
             must first have the helper take off any mark that lets the file's opens through \
             unchecked, or the emptied file could read as zeros from then on; try again once \
             the daemon is connected to the helper again — it reconnects on its own"
        ),
        // follow-up: a file that may still carry the helper's
        // ignore mark is only downloaded again once the helper has cleared
        // it, since a download that fails empties the file.
        (Some("NoHelper"), FreeUpSpace | Free(_)) => "the konedrive helper is not connected, so nothing more \
             was freed up. Freeing up a file must first have the helper take off any mark that lets \
             the file's opens through unchecked, or the emptied file could read as zeros from then \
             on; try again once the daemon is connected to the helper again — it reconnects on its \
             own"
            .to_owned(),
        (Some("NoConflict"), _) => format!(
            "{path} is not in the list of conflicts, so there was nothing to dismiss. `konedrivectl \
             sync conflicts` lists them, each under the path it was moved to"
        ),
        (Some("NoHelper"), Hydrate(_)) => format!(
            "the konedrive helper is not connected, so nothing was changed. {path} was left \
             half freed up, or is marked downloaded with nothing to show it was, and downloading \
             it again must first have the helper stop letting it through unchecked — a download \
             that failed partway would otherwise leave it reading as zeros. Try again once the \
             helper is back (`konedrivectl sync status` shows when it is)"
        ),
        // A folder registered with the helper is forgotten
        // through the helper or not at all.
        (Some("NoHelper"), Forget) => format!(
            "the konedrive helper is not connected, so the sync folder{folder} is still \
             registered and nothing was changed. Forgetting it has to tell the helper to stop \
             watching it; without that, a file freed up there later could read as zeros from \
             then on. Try again once the helper is back (`konedrivectl sync status` shows \
             when it is)"
        ),
        // said of `refresh`, the registration text
        // read "and  was not registered", with no path.
        (Some("NoHelper"), Refresh) => "the konedrive helper is not connected, so the folder is not \
             kept in step with OneDrive until it is, and nothing was asked for"
            .to_owned(),
        // HS2: a OneDrive folder is registered with the helper or not at
        // all; without interception is the developer's mode, never OneDrive.
        (Some("NoHelper"), _) => format!(
            "the konedrive helper is not connected, so {path} was not registered: only through \
             the helper is a OneDrive folder kept in step and a file downloaded when something \
             opens it. Start the helper and try again (`konedrivectl sync register-without-interception` \
             is the developer's mode, for a local folder whose files read as zeros until hydrated)"
        ),
        // What a restored folder that is still waiting for its helper
        // answers, among others: asking for the same folder again, in the
        // other mode.
        (Some("AlreadyRegistered"), _) if !root.is_empty() && path == root => format!(
            "{path} is already the sync folder. To register it again another way, run \
             `konedrivectl sync forget` first; it leaves the files in the folder as they are"
        ),
        (Some("AlreadyRegistered"), _) => format!(
            "a sync folder is already registered{folder}, and KOneDrive keeps only one. To use \
             {path} instead, run `konedrivectl sync forget` first; it leaves the files in the \
             old folder as they are"
        ),
        (Some("NotEmpty"), _) => format!(
            "{path} is not empty. A new sync folder has to start empty, so that nothing already \
             in it is mistaken for a OneDrive file: choose an empty folder, or create a new one"
        ),
        // the source overlaps the sync folder.
        (Some("Unsupported"), PopulateFrom(_)) => {
            format!("the sync folder cannot be filled from {path}: {detail}")
        }
        (Some("Unsupported"), Refresh) => "this folder is not connected to OneDrive, so there is \
             nothing to ask for: it was registered while signed out and is filled with \
             `konedrivectl sync populate-from`"
            .to_owned(),
        (Some("Unsupported"), _) => {
            let why = detail.strip_prefix(&format!("{path}: ")).unwrap_or(detail);
            format!("{path} cannot be used as the sync folder: {why}")
        }
        (Some("NoRoot"), Forget) => {
            "no sync folder is registered, so there is nothing to forget".to_owned()
        }
        (Some("NoRoot"), _) => "no sync folder is registered. Register one first with \
             `konedrivectl sync register <folder>` — or, in the developer's mode with local \
             files, `konedrivectl sync register-without-interception <folder>`"
            .to_owned(),
        (Some("NoSource"), _) => format!(
            "KOneDrive does not know where to download {path} from yet. Run `konedrivectl sync \
             populate-from <dir>` first; the daemon does not remember that directory across a \
             restart, so run it again after one (files already there are left alone)"
        ),
        (Some("OutsideRoot"), Pin(_) | Unpin(_) | Free(_)) => format!(
            "{path}: only files and folders inside the sync folder{folder} can be kept on this \
             device or freed up — not symbolic links, or anything outside it"
        ),
        (Some("OutsideRoot"), _) => format!(
            "{path} is not a regular file inside the sync folder{folder}. Only files inside it \
             can be downloaded or freed up — not folders, symbolic links, or anything outside it"
        ),
        // A free-up of something "Always keep on this device" keeps here.
        // The daemon names what pins it: "pinned by <path>: unpin it first".
        (Some("NotAllowed"), _) => match (pinned_parts(detail).map(|(_, by)| by), action) {
            (Some(by), Unpin(_)) => format!(
                "{path} is kept on this device because the folder {by} is, so it cannot stop being \
                 kept on its own. `konedrivectl sync unpin {by}` stops keeping the folder"
            ),
            (Some(by), _) if by == path => format!(
                "{path} is kept on this device, so its space is not freed up. `konedrivectl sync \
                 free {by}` stops keeping it and frees it up"
            ),
            (Some(by), _) => format!(
                "{path} is kept on this device because the folder {by} is, so its space is not \
                 freed up. To free it, free up the folder first: `konedrivectl sync free {by}` \
                 stops keeping it and frees up what is in it"
            ),
            (None, _) => format!("{} was refused: {detail}", action.doing()),
        },
        (Some("NotManaged"), Dehydrate(_)) => format!(
            "{path} is not a OneDrive file: it is a file of your own in the sync folder, and \
             KOneDrive never frees the space of a file it could not download again"
        ),
        (Some("NotManaged"), Pin(_) | Unpin(_) | Free(_)) => format!(
            "{path}: a file of your own in the sync folder is not a OneDrive file, so there is \
             nothing to keep on this device or to free up"
        ),
        (Some("NotManaged"), _) => format!(
            "{path} is not a OneDrive file: it is a file of your own in the sync folder, so \
             there is nothing to download"
        ),
        (Some("NotHydrated"), _) => format!(
            "{path} is not downloaded, so there is no space to free — it already takes none"
        ),
        (Some("ModifiedLocally"), Hydrate(_)) => format!(
            "{path} was changed here and has not been uploaded, so downloading it again would \
             overwrite your edits. It was left exactly as it is"
        ),
        (Some("ModifiedLocally"), _) => format!(
            "{path} was changed here and has not been uploaded, so freeing its space would lose \
             your edits. It was left exactly as it is"
        ),
        (Some("InUse"), _) => format!(
            "{path} is open in another program, so its space cannot be freed right now. Close \
             it there and try again"
        ),
        // `Failed`, a name this CLI does not know yet, or an error from the
        // bus itself: the detail is all there is, so it is kept whole.
        _ => format!("{} failed: {detail}", action.doing()),
    }
}

/// What each `Skipped()` reason means to the person whose file it is. The
/// same sentences, word for word, as `whyText` in `app/synccontroller.cpp`
/// (each wrapped there in `i18n(...)`); `skip_reason_text_matches_the_windows_wording`
/// below pins each one, and `the_window_uses_the_same_sentences` (in
/// `tests/sync_cli.rs`) checks the C++ source directly, so the two cannot
/// drift apart unnoticed.
pub fn skip_reason_text(reason: &str) -> &'static str {
    match reason {
        "name-too-long" => "The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two).",
        "personal-vault" => "The Personal Vault is locked separately and is not synced.",
        "shared" => "A shared folder added to your OneDrive; shared folders are not synced yet.",
        "onenote" => "A OneNote notebook, which is not a file.",
        "reserved-name" => "The name begins with .konedrive-, which konedrive keeps for itself.",
        _ => "It is neither a file nor a folder konedrive can show.",
    }
}

/// What to tell a person when `Dev1.AccessToken()` failed — matched by the
/// D-Bus error name, never the message, the same discipline
/// [`explain_sync_error`] uses for `Sync1`. Being signed out is worth its
/// own sentence, since the fix (`konedrivectl login`) is not what the
/// daemon's own message says; a locked wallet or a network error already
/// says what is wrong on its own, so its text is kept as is.
pub fn explain_dev_error(error: &zbus::Error) -> String {
    let detail = match error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    };
    dev_refusal_text(error_name(error), &detail)
}

/// [`explain_dev_error`]'s decision, on the name and message alone — so it
/// can be tested with a name and a message that disagree, the same shape
/// [`refusal_text`] is tested with.
pub fn dev_refusal_text(name: Option<&str>, detail: &str) -> String {
    let refusal = name.and_then(|name| name.strip_prefix(ERROR_PREFIX)).and_then(|rest| rest.strip_prefix('.'));
    match refusal {
        Some("NotSignedIn") => {
            "cannot get an access token: nobody is signed in. Are you signed in? \
             (`konedrivectl status` says.)"
                .to_owned()
        }
        _ => format!("cannot get an access token: {detail}"),
    }
}

/// Writes `data` to `path` as a brand-new file, atomically and privately.
///
/// A temporary file is created in the same directory as `path`
/// (`O_CREAT | O_EXCL | O_NOFOLLOW`, mode 0600 from the instant it exists —
/// so the name can only be *this* call's own new file, never an existing
/// one and never a symlink), written, `fsync`ed, then renamed over `path`.
/// `rename(2)` replaces whatever `path` names — a symlink there included —
/// by swapping the directory entry to the new inode, rather than writing
/// through whatever `path` used to point to; so a symlink at `path` is
/// *replaced*, never followed and never written through, and anyone who
/// already had the old `path` open keeps reading the old inode's content,
/// completely untouched, for as long as they hold it open. The temporary
/// file is removed on any failure along the way, so a half-written one is
/// never left where an unrelated later read could find it.
///
/// This is what `dev export-access-token` uses to write the token: the
/// symlink case matters because `--out` names a path the person running the
/// command chose, which could already be a symlink (by accident, or by
/// something else's doing) to a file they did not mean to touch.
pub fn write_secret_atomically(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name in the given path")
    })?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp_path = dir.join(&tmp_name);

    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // O_CREAT | O_EXCL: this name is ours alone.
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&tmp_path)
        .and_then(|mut file| {
            file.write_all(data)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp_path, path));

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

/// When the folder was last checked with OneDrive, as `sync status` says it
///: "20 s ago", "5 min ago", "3 h ago", "2 d ago", or "never"
/// for 0. `last` and `now` are unix seconds.
pub fn checked_text(last: i64, now: i64) -> String {
    if last == 0 {
        return "never".to_owned();
    }
    match now - last {
        ago if ago < 0 => "just now".to_owned(),
        ago @ 0..=59 => format!("{ago} s ago"),
        ago @ 60..=3_599 => format!("{} min ago", ago / 60),
        ago @ 3_600..=86_399 => format!("{} h ago", ago / 3_600),
        ago => format!("{} d ago", ago / 86_400),
    }
}

/// Unix seconds now.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `2026-09-24 10:00:05`, in this machine's time zone (`TZ` as the C library
/// reads it).
pub fn local_time(at: i64) -> String {
    let seconds = at as libc::time_t;
    // SAFETY: `tm` is plain data that `localtime_r` fills in; both pointers
    // are to live locals for the length of the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return at.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// `sync activity`: one line per event, newest first — time, kind, full
/// path, and the detail in parentheses when there is one.
pub fn activity_text(events: &[(i64, String, String, String)]) -> String {
    if events.is_empty() {
        return "Nothing has happened yet.\n".to_owned();
    }
    let mut out = String::new();
    for (at, kind, path, detail) in events {
        let detail = if detail.is_empty() { String::new() } else { format!("  ({detail})") };
        out.push_str(&format!("{}  {kind:<10}  {path}{detail}\n", local_time(*at)));
    }
    out
}

/// `sync transfers`: one line per download under way — path, how far, and
/// the whole size.
pub fn transfers_text(transfers: &[(String, u64, u64)]) -> String {
    if transfers.is_empty() {
        return "Nothing is downloading.\n".to_owned();
    }
    let mut out = String::new();
    for (path, done, total) in transfers {
        let percent = if *total == 0 { 0 } else { done.saturating_mul(100) / total };
        out.push_str(&format!("{path}  {percent}%  {}\n", human_bytes(*total)));
    }
    out
}

/// `sync conflicts`: each local version moved out of the way, where it was
/// and where it is now, and when.
pub fn conflicts_text(conflicts: &[(i64, String, String)]) -> String {
    if conflicts.is_empty() {
        return "No conflicts.\n".to_owned();
    }
    let mut out = String::new();
    for (at, original, rescued) in conflicts {
        out.push_str(&format!("{original}\n    moved to {rescued} on {}\n", local_time(*at)));
    }
    out
}

/// `sync free-up-space`: "Freed N files (X). M files were in use and kept."
pub fn free_up_text(files: u32, bytes: u64, busy: u32) -> String {
    if files == 0 && busy == 0 {
        return "Nothing was downloaded, so there was nothing to free up.".to_owned();
    }
    let mut out = format!("Freed {files} {} ({}).", if files == 1 { "file" } else { "files" }, human_bytes(bytes));
    if busy > 0 {
        let were = if busy == 1 { "file was" } else { "files were" };
        out.push_str(&format!(" {busy} {were} in use and kept."));
    }
    out
}

/// `sync pin`: that the paths are kept on this device, and how many of their
/// files are downloading now.
pub fn pin_text(queued: u32) -> String {
    match queued {
        0 => "Kept on this device. Everything in it is here already.".to_owned(),
        1 => "Kept on this device. 1 file is downloading (`konedrivectl sync transfers`).".to_owned(),
        n => format!("Kept on this device. {n} files are downloading (`konedrivectl sync transfers`)."),
    }
}

/// The path a `NotAllowed` refusal is about, and what pins it: the daemon
/// says exactly "<path> is pinned by <folder>: unpin it first", and the
/// folder is the path or one above it — which tells the two apart even when
/// a name holds " is pinned by " itself.
fn pinned_parts(detail: &str) -> Option<(&str, &str)> {
    const BY: &str = " is pinned by ";
    let both = detail.strip_suffix(": unpin it first")?;
    both.match_indices(BY)
        .map(|(at, _)| (&both[..at], &both[at + BY.len()..]))
        .find(|(path, by)| std::path::Path::new(path).starts_with(by))
}

/// The one path a `NotAllowed` refusal of a call on several is about.
/// `None` for any other error.
pub fn refused_path(error: &zbus::Error) -> Option<&str> {
    let zbus::Error::MethodError(name, Some(detail), _) = error else { return None };
    if name.as_str().strip_prefix(ERROR_PREFIX) != Some(".NotAllowed") {
        return None;
    }
    pinned_parts(detail).map(|(path, _)| path)
}

/// `sync unpin`: how many pins came off; the files stay.
pub fn unpin_text(unpinned: u32) -> String {
    match unpinned {
        0 => "Nothing here had a pin of its own, so nothing changed.".to_owned(),
        1 => "No longer kept on this device. What is downloaded stays; `konedrivectl sync free` frees it.".to_owned(),
        n => format!(
            "{n} items are no longer kept on this device. What is downloaded stays; `konedrivectl sync free` frees it."
        ),
    }
}

/// `sync free`: what was freed, what was kept because it was in use or
/// changed here (FreeUp's `busy` counts both), and the downloaded files a pin
/// of their own — or of a folder below the one freed — kept.
pub fn free_text(files: u32, bytes: u64, busy: u32, pinned: u32) -> String {
    if files == 0 && busy == 0 && pinned == 0 {
        return "Nothing here was downloaded, so there was nothing to free up.".to_owned();
    }
    let mut out = if files == 0 {
        "Nothing was freed up.".to_owned()
    } else {
        format!("Freed {files} {} ({}).", if files == 1 { "file" } else { "files" }, human_bytes(bytes))
    };
    if busy > 0 {
        let were = if busy == 1 { "file was" } else { "files were" };
        out.push_str(&format!(" {busy} {were} in use or changed here, and kept."));
    }
    if pinned > 0 {
        let were = if pinned == 1 { "file was" } else { "files were" };
        out.push_str(&format!(" {pinned} {were} kept: a file or folder below is kept on this device."));
    }
    out
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Polls `proxy` until the account leaves the `signing-in` state, then reports the outcome.
///
/// Polling (rather than watching the `StateChanged` signal) sidesteps a coalescing hazard:
/// the daemon publishes state through a `tokio::watch` channel, so a fast transition (e.g.
/// `signing-in` -> `signed-in` completing almost immediately, as with an SSO session) can be
/// collapsed and observed only as its final value. A waiter that only starts counting once it
/// has *seen* `signing-in` on the signal stream could then wait forever for a change that
/// already happened. `BeginSignIn` sets the state to `signing-in` before it replies, so the
/// very first poll here is guaranteed to observe either `signing-in` or the terminal state -
/// there is no window in which the relevant transition can be missed.
pub async fn wait_for_sign_in(proxy: &Account1Proxy<'_>) -> anyhow::Result<()> {
    loop {
        let state = proxy.state().await?;
        if state != "signing-in" {
            return if state == "signed-in" {
                Ok(())
            } else {
                let last_error = proxy.last_error().await?;
                if last_error.is_empty() {
                    anyhow::bail!("sign-in was cancelled");
                } else {
                    anyhow::bail!("sign-in did not complete: {last_error}");
                }
            };
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{dev_refusal_text, human_bytes, refusal_text, skip_reason_text, write_secret_atomically, SyncAction};

    /// Pins every `Skipped()` reason's sentence — the same wording as the
    /// window's `whyText` (`app/synccontroller.cpp`), which is what keeps a
    /// user reading the same explanation from `konedrivectl sync skipped`
    /// and from the window regardless of which one they happen to use.
    #[test]
    fn skip_reason_text_matches_the_windows_wording() {
        assert_eq!(
            skip_reason_text("name-too-long"),
            "The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two)."
        );
        assert_eq!(
            skip_reason_text("personal-vault"),
            "The Personal Vault is locked separately and is not synced."
        );
        assert_eq!(
            skip_reason_text("shared"),
            "A shared folder added to your OneDrive; shared folders are not synced yet."
        );
        assert_eq!(skip_reason_text("onenote"), "A OneNote notebook, which is not a file.");
        assert_eq!(
            skip_reason_text("reserved-name"),
            "The name begins with .konedrive-, which konedrive keeps for itself."
        );
        assert_eq!(
            skip_reason_text("unsupported"),
            "It is neither a file nor a folder konedrive can show."
        );
        assert_eq!(
            skip_reason_text("something-nobody-invented-yet"),
            "It is neither a file nor a folder konedrive can show."
        );
    }

    /// Being signed out gets its own sentence, distinct from a locked wallet
    /// or a network error, which keep the daemon's own message: the fix for
    /// each is different, so folding them into one generic sentence would
    /// hide which one applies.
    #[test]
    fn dev_refusal_text_distinguishes_signed_out_from_other_failures() {
        let signed_out = dev_refusal_text(Some("org.konedrive.Error.NotSignedIn"), "nobody is signed in");
        assert!(signed_out.to_lowercase().contains("signed in"), "{signed_out}");

        let locked = dev_refusal_text(Some("org.konedrive.Error.Failed"), "secret storage is locked");
        assert!(locked.contains("secret storage is locked"), "{locked}");
        assert!(
            !locked.to_lowercase().contains("are you signed in"),
            "a locked wallet is not the same thing as being signed out: {locked}"
        );

        let network = dev_refusal_text(
            Some("org.konedrive.Error.Failed"),
            "Microsoft rejected the token refresh: invalid_client: bad request",
        );
        assert!(network.contains("Microsoft rejected the token refresh"), "{network}");

        // A name from outside `org.konedrive.Error` is not mistaken for
        // `NotSignedIn` just because the detail happens to mention signing in.
        let bus_error = dev_refusal_text(Some("org.freedesktop.DBus.Error.NoReply"), "no reply");
        assert!(!bus_error.to_lowercase().contains("are you signed in"), "{bus_error}");
    }

    #[test]
    fn write_secret_atomically_creates_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("token");
        write_secret_atomically(&out, b"AT-1").unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "AT-1");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
        // No temporary file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "token")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// The heart of I1: `rename(2)` replaces the symlink itself, so its
    /// target is never opened, truncated, or written through.
    #[test]
    fn write_secret_atomically_replaces_a_symlink_without_touching_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real-file");
        std::fs::write(&target, b"do not touch").unwrap();
        let link = dir.path().join("out-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_secret_atomically(&link, b"AT-2").unwrap();

        assert!(
            !std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
            "the link must be replaced by a regular file, not written through"
        );
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "AT-2");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "do not touch", "the old target is untouched");
    }

    /// The other half of I1: an fd opened before the export keeps reading
    /// the old inode's content — `rename(2)` never truncates it in place.
    #[test]
    fn write_secret_atomically_does_not_disturb_a_reader_of_the_old_file() {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("token");
        std::fs::write(&out, b"old-content").unwrap();
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut held_open = std::fs::File::open(&out).unwrap();

        write_secret_atomically(&out, b"AT-3").unwrap();

        assert_eq!(std::fs::read_to_string(&out).unwrap(), "AT-3");
        assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
        let mut still_reads = String::new();
        held_open.read_to_string(&mut still_reads).unwrap();
        assert_eq!(still_reads, "old-content", "an fd opened before the export keeps its own inode");
    }

    /// The name decides, never the message. A refusal named
    /// `ModifiedLocally` whose message happens to read like `NotHydrated`'s
    /// must still be explained as edits that would be lost — matching on
    /// the prose is exactly what a named error exists to replace.
    #[test]
    fn a_refusal_is_explained_by_its_name_not_its_message() {
        let text = refusal_text(
            SyncAction::Dehydrate("/r/doc.bin"),
            Some("org.konedrive.Error.ModifiedLocally"),
            "the file is not downloaded",
            "/r",
        );
        assert!(text.contains("lose your edits"), "{text}");
        assert!(!text.contains("no space to free"), "{text}");

        let text = refusal_text(
            SyncAction::Dehydrate("/r/doc.bin"),
            Some("org.konedrive.Error.NotHydrated"),
            "the file was modified locally",
            "/r",
        );
        assert!(text.contains("no space to free"), "{text}");
        assert!(!text.contains("lose your edits"), "{text}");
    }

    /// A name from outside `org.konedrive.Error` — the bus's own, say — is
    /// not mistaken for one of ours even when its last component matches.
    #[test]
    fn only_names_under_the_konedrive_prefix_are_ours() {
        let text = refusal_text(
            SyncAction::Hydrate("/r/doc.bin"),
            Some("org.freedesktop.DBus.Error.InUse"),
            "something else entirely",
            "/r",
        );
        assert_eq!(text, "downloading /r/doc.bin failed: something else entirely");
    }

    /// A Forget of a folder registered with the helper now
    /// needs the helper, and is refused `NoHelper` without one. The generic
    /// `NoHelper` text was written for registering — "… and  was not
    /// registered … `register-without-interception `" with an empty path —
    /// which reads as nonsense after `sync forget`, and says nothing about
    /// the folder still being registered.
    #[test]
    fn a_forget_refused_for_want_of_the_helper_says_the_folder_is_still_registered() {
        let text = refusal_text(
            SyncAction::Forget,
            Some("org.konedrive.Error.NoHelper"),
            "the konedrive helper is not connected",
            "/home/u/OneDrive",
        );
        assert!(text.contains("/home/u/OneDrive"), "{text}");
        assert!(text.contains("still registered"), "{text}");
        assert!(text.contains("once the helper is back"), "{text}");
        assert!(!text.contains("was not registered"), "{text}");
        assert!(!text.contains("register-without-interception"), "{text}");
    }

    /// follow-up: `Hydrate` of a file that may carry an ignore
    /// mark — one a cancelled "free up space" left half done, or one labelled
    /// downloaded with nothing to prove it — needs the helper to clear that
    /// mark first, and is refused `NoHelper` without one. The generic text
    /// talks about registering a folder.
    #[test]
    fn a_download_refused_for_want_of_the_helper_says_nothing_changed() {
        let text = refusal_text(
            SyncAction::Hydrate("/home/u/OneDrive/doc.bin"),
            Some("org.konedrive.Error.NoHelper"),
            "the konedrive helper is not connected",
            "/home/u/OneDrive",
        );
        assert!(text.contains("/home/u/OneDrive/doc.bin"), "{text}");
        assert!(text.contains("nothing was changed"), "{text}");
        assert!(text.contains("once the helper is back"), "{text}");
        assert!(!text.contains("was not registered"), "{text}");
    }

    /// a populate source that overlaps the sync
    /// folder is refused `Unsupported`, whose text was written for a folder
    /// that cannot be registered.
    #[test]
    fn a_populate_source_refused_as_unsupported_is_not_called_a_sync_folder() {
        let text = refusal_text(
            SyncAction::PopulateFrom("/home/u/OneDrive/src"),
            Some("org.konedrive.Error.Unsupported"),
            "/home/u/OneDrive/src is inside the sync folder /home/u/OneDrive, and a folder \
             cannot be filled from itself",
            "/home/u/OneDrive",
        );
        assert!(!text.contains("cannot be used as the sync folder"), "{text}");
        assert!(text.contains("cannot be filled from itself"), "{text}");
    }

    /// The same folder asked for again — which is what a restored folder
    /// waiting for its helper now answers `AlreadyRegistered` to — is not
    /// "use this one instead".
    #[test]
    fn registering_the_folder_that_is_already_registered_says_so() {
        let text = refusal_text(
            SyncAction::RegisterWithoutInterception("/home/u/OneDrive"),
            Some("org.konedrive.Error.AlreadyRegistered"),
            "a sync root is already registered; forget it first",
            "/home/u/OneDrive",
        );
        assert!(text.contains("already the sync folder"), "{text}");
        assert!(!text.contains("instead"), "{text}");

        let text = refusal_text(
            SyncAction::RegisterWithoutInterception("/home/u/Other"),
            Some("org.konedrive.Error.AlreadyRegistered"),
            "a sync root is already registered; forget it first",
            "/home/u/OneDrive",
        );
        assert!(text.contains("To use /home/u/Other instead"), "{text}");
    }

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
