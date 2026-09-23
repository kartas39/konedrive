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

/// Same column layout as the account status.
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
/// most for `no-interception`: a standing project ruling keeps the
/// privileged helper inside a VM and off the user's own machine, so there
/// that mode is the *ordinary* state, and its cost — a file that is not
/// downloaded reads as zeros — has to be on screen every time, not left to
/// the user's memory or to whatever `LastError` happens to say.
pub async fn sync_status_text(proxy: &Sync1Proxy<'_>) -> zbus::Result<String> {
    let path = proxy.root_path().await?;
    let state = proxy.root_state().await?;
    let shown = if path.is_empty() { "(none)" } else { path.as_str() };
    let mut out = format!("{:<12}{shown}\n", "Folder:");
    out.push_str(&format!("{:<12}{state}\n", "State:"));
    if let Some(opens) = opens_line(&state) {
        out.push_str(&format!("{:<12}{opens}\n", "Opens:"));
    }
    let last_error = proxy.last_error().await?;
    if !last_error.is_empty() {
        out.push_str(&format!("{:<12}{last_error}\n", "Last error:"));
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
        }
    }

    fn path(&self) -> &str {
        match self {
            Self::Register(path)
            | Self::RegisterWithoutInterception(path)
            | Self::PopulateFrom(path)
            | Self::Hydrate(path)
            | Self::Dehydrate(path) => path,
            Self::Forget => "",
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
             signed-in OneDrive account. Sign in first with `konedrivectl login` — or, to try \
             the folder with local files and no account, use `konedrivectl sync \
             register-without-interception {path}`, where nothing downloads a file when it is \
             opened and files read as zeros until you hydrate them"
        ),
        // In a folder registered without interception too (Ruling H146): a
        // helper that is running while this daemon has no connection to it
        // may hold a mark on the file that nothing can clear.
        (Some("NoHelper"), Dehydrate(_)) => format!(
            "the konedrive helper is not connected, so nothing was changed. Freeing up {path} \
             must first have the helper take off any mark that lets the file's opens through \
             unchecked, or the emptied file could read as zeros from then on; try again once \
             the daemon is connected to the helper again — it reconnects on its own"
        ),
        // Ruling H137's follow-up: a file that may still carry the helper's
        // ignore mark is only downloaded again once the helper has cleared
        // it, since a download that fails empties the file.
        (Some("NoHelper"), Hydrate(_)) => format!(
            "the konedrive helper is not connected, so nothing was changed. {path} was left \
             half freed up, or is marked downloaded with nothing to show it was, and downloading \
             it again must first have the helper stop letting it through unchecked — a download \
             that failed partway would otherwise leave it reading as zeros. Try again once the \
             helper is back (`konedrivectl sync status` shows when it is)"
        ),
        // Ruling H133: a folder registered with the helper is forgotten
        // through the helper or not at all.
        (Some("NoHelper"), Forget) => format!(
            "the konedrive helper is not connected, so the sync folder{folder} is still \
             registered and nothing was changed. Forgetting it has to tell the helper to stop \
             watching it; without that, a file freed up there later could read as zeros from \
             then on. Try again once the helper is back (`konedrivectl sync status` shows \
             when it is)"
        ),
        (Some("NoHelper"), _) => format!(
            "the konedrive helper is not running, so nothing would download a file when \
             something opens it — files that are not downloaded would read as zeros — and \
             {path} was not registered. Start the helper and try again, or register the folder \
             without it: `konedrivectl sync register-without-interception {path}`, then download \
             files yourself with `konedrivectl sync hydrate <file>`"
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
        // The final review's m10: the source overlaps the sync folder.
        (Some("Unsupported"), PopulateFrom(_)) => {
            format!("the sync folder cannot be filled from {path}: {detail}")
        }
        (Some("Unsupported"), _) => {
            let why = detail.strip_prefix(&format!("{path}: ")).unwrap_or(detail);
            format!("{path} cannot be used as the sync folder: {why}")
        }
        (Some("NoRoot"), Forget) => {
            "no sync folder is registered, so there is nothing to forget".to_owned()
        }
        (Some("NoRoot"), _) => "no sync folder is registered. Register one first with \
             `konedrivectl sync register <folder>` — or `konedrivectl sync \
             register-without-interception <folder>` on a machine without the helper"
            .to_owned(),
        (Some("NoSource"), _) => format!(
            "KOneDrive does not know where to download {path} from yet. Run `konedrivectl sync \
             populate-from <dir>` first; the daemon does not remember that directory across a \
             restart, so run it again after one (files already there are left alone)"
        ),
        (Some("OutsideRoot"), _) => format!(
            "{path} is not a regular file inside the sync folder{folder}. Only files inside it \
             can be downloaded or freed up — not folders, symbolic links, or anything outside it"
        ),
        (Some("NotManaged"), Dehydrate(_)) => format!(
            "{path} is not a OneDrive file: it is a file of your own in the sync folder, and \
             KOneDrive never frees the space of a file it could not download again"
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
    use super::{human_bytes, refusal_text, SyncAction};

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

    /// Ruling H133: a Forget of a folder registered with the helper now
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

    /// Ruling H137's follow-up: `Hydrate` of a file that may carry an ignore
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

    /// The final review's m10: a populate source that overlaps the sync
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
