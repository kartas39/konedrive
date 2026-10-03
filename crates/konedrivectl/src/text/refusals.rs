use konedrive_dbus::{error_name, ERROR_PREFIX, LABEL_RULE};

use super::files::pinned_parts;
use super::formats::{sentence, shell_word};
use crate::is_gone_name;

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
    /// `sync open`: `Files.WebUrl`, with the path.
    Open(&'a str),
    /// `account remove`, with the account's label: `Accounts.Remove` forgets the
    /// folder as `Forget` does, and is refused under the same names.
    Remove(&'a str),
    Outbox,
    Pause,
    Resume,
    Ignore,
    /// `sync thumbnails`: an account's own sync setting.
    Settings,
    /// `sync anyway`.
    Anyway,
    NotUploaded,
    Deletes,
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
            Self::Open(path) => format!("opening {path} in OneDrive"),
            Self::Remove(label) => format!("removing the account {label}"),
            Self::Outbox => "listing the changes waiting to upload".to_owned(),
            Self::Pause => "pausing the sync".to_owned(),
            Self::Resume => "resuming the sync".to_owned(),
            Self::Ignore => "changing the ignore list".to_owned(),
            Self::Settings => "changing the sync settings".to_owned(),
            Self::Anyway => "syncing anyway".to_owned(),
            Self::NotUploaded => "listing what is not uploaded".to_owned(),
            Self::Deletes => "deciding on the large delete".to_owned(),
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
            | Self::Free(path)
            | Self::Open(path) => path,
            Self::Forget
            | Self::Refresh
            | Self::Skipped
            | Self::Activity
            | Self::Conflicts
            | Self::FreeUpSpace
            | Self::Remove(_)
            | Self::Outbox
            | Self::Pause
            | Self::Resume
            | Self::Ignore
            | Self::Settings
            | Self::Anyway
            | Self::NotUploaded
            | Self::Deletes => "",
        }
    }
}

/// What to tell a person when a folder call made for `action` failed.
///
/// Matches the D-Bus error **name** (`konedrive_dbus::error_name`), never the
/// message: every refusal the folder makes arrives under
/// `konedrive_dbus::ERROR_PREFIX`, and each gets a sentence saying what
/// happened to the user's file and what they can do about it. `root` is the
/// registered folder (`Folder.Path`, possibly empty), which two refusals name.
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
/// folder of the account the refusal is about (`Folder.Path`: the chosen
/// account's, or for a path, the folder that holds it), what it shows
/// (`RootSource`), the helper (`Accounts.HelperState`), and for a path, every
/// account's folder. Any of them may be empty.
#[derive(Debug, Default, Clone, Copy)]
pub struct Context<'a> {
    pub root: &'a str,
    pub source: &'a str,
    pub helper: &'a str,
    /// Every account's registered folder, for a path command: a path in none
    /// of them is refused `OutsideRoot` before any account sees it.
    pub folders: &'a [String],
    /// The folder a registration named carries another account's drive
    /// (`user.konedrive.drive`, design §8.3), which the daemon refuses under
    /// `NotEmpty` too.
    pub foreign: bool,
    /// How a suggested command names the account ([`command_prefix`]); empty
    /// for plain `konedrivectl`.
    pub prefix: &'a str,
}

impl Context<'_> {
    fn prefix(&self) -> &str {
        if self.prefix.is_empty() {
            "konedrivectl"
        } else {
            self.prefix
        }
    }
}

/// [`explain_sync_error`], with what [`Context`] adds: a refusal for want
/// of the helper ends with how to start it (HS4), a folder that shows
/// OneDrive is not told to populate itself from a directory (B-M6), a path
/// in no account's folder is told which folders there are, and a folder
/// that was another account's is told so.
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
    use SyncAction::*;
    let refusal = name.and_then(|name| name.strip_prefix(ERROR_PREFIX)).and_then(|rest| rest.strip_prefix('.'));
    let path = action.path();
    let prefix = context.prefix();
    let path_command = matches!(action, Hydrate(_) | Dehydrate(_) | Pin(_) | Unpin(_) | Free(_) | Open(_));
    // An account removed while this command ran: its object is gone.
    if name.is_some_and(is_gone_name) && !path_command {
        let text = refusal_text_as(action, Some(&format!("{ERROR_PREFIX}.NoAccount")), detail, context.root, prefix);
        return text;
    }
    match (refusal, action) {
        // `Files` refuses a path in no account's folder before any account
        // sees it; with no folder at all, that is `NoRoot`'s situation.
        (Some("OutsideRoot"), _) if path_command && context.root.is_empty() => {
            return match context.folders {
                [] => refusal_text_as(action, Some(&format!("{ERROR_PREFIX}.NoRoot")), detail, "", prefix),
                [one] if matches!(action, Open(_)) => format!(
                    "{path} is not inside the sync folder ({one}). Only what is inside it, and the folder \
                     itself, has a page in OneDrive"
                ),
                several if matches!(action, Open(_)) => format!(
                    "{path} is not inside any account's sync folder ({}). Only what is inside one, and the \
                     folder itself, has a page in OneDrive",
                    several.join(", ")
                ),
                [one] => format!(
                    "{path} is not inside the sync folder ({one}). Only what is inside it can be downloaded, \
                     freed up or kept on this device"
                ),
                several => format!(
                    "{path} is not inside any account's sync folder ({}). Only what is inside one can be \
                     downloaded, freed up or kept on this device",
                    several.join(", ")
                ),
            };
        }
        (Some("NotEmpty"), Register(_) | RegisterWithoutInterception(_)) if context.foreign => {
            return format!(
                "{path} holds the files of another OneDrive account's folder, so it cannot be this \
                 account's. Choose an empty folder, or create a new one. If it was this account's own \
                 folder before, sign the account in first (`{prefix} login`), then register it again"
            );
        }
        _ => {}
    }
    let text = refusal_text_as(action, name, detail, context.root, prefix);
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

/// [`explain_sync_error`]'s decision, on the name and message alone — so it
/// can be tested with a name and a message that disagree.
pub fn refusal_text(action: SyncAction<'_>, name: Option<&str>, detail: &str, root: &str) -> String {
    refusal_text_as(action, name, detail, root, "konedrivectl")
}

/// [`refusal_text`], with every command it suggests for this account begun
/// with `prefix` ([`command_prefix`]).
fn refusal_text_as(action: SyncAction<'_>, name: Option<&str>, detail: &str, root: &str, prefix: &str) -> String {
    use SyncAction::*;
    let refusal = name
        .and_then(|name| name.strip_prefix(ERROR_PREFIX))
        .and_then(|rest| rest.strip_prefix('.'));
    let path = action.path();
    let folder = if root.is_empty() { String::new() } else { format!(" ({root})") };
    match (refusal, action) {
        // `sync open` (issue #53): OneDrive is asked for the address each time.
        // The daemon's message is the cause (no network, a locked secret
        // storage, an answer that cannot be read), shown when there is one.
        (Some("Unreachable"), _) if detail.is_empty() || detail.starts_with(ERROR_PREFIX) => {
            "OneDrive could not be reached".to_owned()
        }
        (Some("Unreachable"), _) => format!("OneDrive could not be reached: {detail}"),
        (Some("NotSignedIn"), Open(_)) => format!(
            "the account is not signed in, so OneDrive cannot be asked for the page of {path}. Sign in \
             with `{prefix} login` and try again"
        ),
        (Some("NotUploaded"), Open(_)) => format!(
            "{path} is not uploaded yet, so it has no page in OneDrive. It can be opened there once it \
             is uploaded (`{prefix} sync outbox`)"
        ),
        (Some("NotManaged"), Open(_)) => format!(
            "{path} is not a OneDrive file: it is a file of your own in the sync folder, so it has no \
             page in OneDrive"
        ),
        (Some("OutsideRoot"), Open(_)) => format!(
            "{path}: only files and folders inside the sync folder{folder}, and the folder itself, have \
             a page in OneDrive — not symbolic links, or anything outside it"
        ),
        (Some("NotSignedIn"), _) => format!(
            "the account is not signed in, and `{prefix} sync register` binds the folder to the \
             account's OneDrive. Sign in first with `{prefix} login` — or, in the developer's mode \
             with local files, use `{prefix} sync register-without-interception {path}`: without \
             the helper, files that are not downloaded read as zeros until you hydrate them"
        ),
        // `Accounts.Remove` forgets the folder first, under Forget's rule.
        (Some("NoHelper"), Remove(label)) => format!(
            "the konedrive helper is not connected, so the account {label} was not removed and \
             nothing was changed. Removing it forgets its folder{folder}, and forgetting a folder \
             registered with the helper has to tell the helper to stop watching it; without that, \
             a file freed up there later could read as zeros from then on. Try again once the \
             helper is back (`konedrivectl sync status` shows when it is)"
        ),
        // The changes waiting to upload would go with the account's folder.
        (Some("PendingUploads"), Remove(label)) => {
            let prefix = format!("konedrivectl --account {}", shell_word(label));
            format!(
                "the account {label} was not removed, and nothing was changed: {detail}. `{prefix} sync \
                 outbox` lists what waits; `{prefix} account mode read-only --force` drops it — the \
                 files stay here as they are, and OneDrive does not get the changes — and the account \
                 can be removed then"
            )
        }
        (Some("PendingUploads"), _) => format!(
            "the sync folder{folder} is still registered, and nothing was changed: {detail}. `{prefix} \
             sync outbox` lists what waits; `{prefix} account mode read-only --force` drops it — the \
             files stay here as they are, and OneDrive does not get the changes — and the folder can \
             be forgotten then"
        ),
        (Some("NoAccount"), Remove(label)) => format!(
            "there is no account {label} any more, so nothing was removed. `konedrivectl account \
             list` shows the accounts there are"
        ),
        (Some("NoAccount"), _) => "that account is gone: it was removed meanwhile. `konedrivectl \
             account list` shows the accounts there are"
            .to_owned(),
        // Design §8.3: the folders of two accounts never nest. The daemon's
        // message names the other account.
        (Some("Overlaps"), _) => format!(
            "{path} cannot be this account's folder: {detail}. The folders of two accounts cannot \
             be one inside the other: choose a folder outside every other account's folder \
             (`konedrivectl account list` shows them)"
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
            "{path} is not in the list of conflicts, so there was nothing to dismiss. `{prefix} \
             sync conflicts` lists them, each under the path where your version is kept"
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
             opens it. Start the helper and try again (`{prefix} sync register-without-interception` \
             is the developer's mode, for a local folder whose files read as zeros until hydrated)"
        ),
        // What a restored folder that is still waiting for its helper
        // answers, among others: asking for the same folder again, in the
        // other mode.
        (Some("AlreadyRegistered"), _) if !root.is_empty() && path == root => format!(
            "{path} is already the sync folder. To register it again another way, run \
             `{prefix} sync forget` first; it leaves the files in the folder as they are"
        ),
        (Some("AlreadyRegistered"), _) => format!(
            "this account already has a sync folder{folder}, and an account keeps only one. To \
             use {path} instead, run `{prefix} sync forget` first; it leaves the files in the \
             old folder as they are. For another OneDrive account, add an account of its own: \
             `konedrivectl account add <label>`"
        ),
        (Some("NotEmpty"), _) => format!(
            "{path} is not empty. A new sync folder has to start empty, so that nothing already \
             in it is mistaken for a OneDrive file: choose an empty folder, or create a new one"
        ),
        // the source overlaps the sync folder.
        (Some("Unsupported"), PopulateFrom(_)) => {
            format!("the sync folder cannot be filled from {path}: {detail}")
        }
        (Some("Unsupported"), Refresh) => format!(
            "this folder is not connected to OneDrive, so there is nothing to ask for: it was \
             registered while signed out and is filled with `{prefix} sync populate-from`"
        ),
        (Some("Unsupported"), Outbox | Pause | Resume | Ignore | NotUploaded | Deletes) => {
            "this folder is not connected to OneDrive, so nothing is uploaded from it".to_owned()
        }
        (Some("Unsupported"), Settings | Anyway) => {
            "this folder is not connected to OneDrive, so it has no sync settings".to_owned()
        }
        // `NoRoot` is the daemon's answer to these both with no folder registered and before
        // the folder's sync has opened its store. Only a registered folder has a path; with
        // none, the arm for every other command says what to do.
        (Some("NoRoot"), Outbox | Pause | Resume | Ignore | NotUploaded | Deletes) if !root.is_empty() => {
            "the folder's sync has not started yet; try again in a moment".to_owned()
        }
        (Some("Unsupported"), _) => {
            let why = detail.strip_prefix(&format!("{path}: ")).unwrap_or(detail);
            format!("{path} cannot be used as the sync folder: {why}")
        }
        (Some("NoRoot"), Forget) => {
            "no sync folder is registered, so there is nothing to forget".to_owned()
        }
        (Some("NoRoot"), _) => format!(
            "no sync folder is registered. Register one first with `{prefix} sync register \
             <folder>` — or, in the developer's mode with local files, `{prefix} sync \
             register-without-interception <folder>`"
        ),
        (Some("NoSource"), _) => format!(
            "KOneDrive does not know where to download {path} from yet. Run `{prefix} sync \
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
        (Some("NotUploaded"), _) => format!(
            "{} is not uploaded yet, so freeing up its space would lose the changes made here. It \
             was left as it is; its space can be freed once it is uploaded (`{prefix} sync outbox`)",
            if path.is_empty() { detail.split(" is not uploaded").next().unwrap_or(detail) } else { path }
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

/// What to tell a person when `TokenExport.ReadOnly()` failed — matched by the
/// D-Bus error name, never the message, the same discipline
/// [`explain_sync_error`] uses for the folder. Being signed out is worth its
/// own sentence, since the fix (`konedrivectl login`) is not what the
/// daemon's own message says; a locked wallet or a network error already
/// says what is wrong on its own, so its text is kept as is.
pub fn explain_dev_error(error: &zbus::Error, prefix: &str) -> String {
    let detail = match error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    };
    dev_refusal_text(error_name(error), &detail, prefix)
}

/// [`explain_dev_error`]'s decision, on the name and message alone — so it
/// can be tested with a name and a message that disagree, the same shape
/// [`refusal_text`] is tested with.
pub fn dev_refusal_text(name: Option<&str>, detail: &str, prefix: &str) -> String {
    let refusal = name.and_then(|name| name.strip_prefix(ERROR_PREFIX)).and_then(|rest| rest.strip_prefix('.'));
    match refusal {
        Some("NotSignedIn") => format!(
            "cannot get an access token: the account is not signed in. Are you signed in? \
             (`{prefix} status` says; `{prefix} login` signs in.)"
        ),
        Some("WritesNotAllowed") => format!(
            "cannot get a read-write access token: while uploads are being developed, only the test \
             accounts listed in write_test_drive_ids in ~/.config/konedrive/config.toml can be \
             read-write, and this account is not one of them. {WITHOUT_READ_WRITE}"
        ),
        Some("ModeNotGranted") => format!(
            "cannot get a read-write access token: the account is read-only. Switch it first: \
             `{prefix} account mode read-write`"
        ),
        _ => format!("cannot get an access token: {detail}"),
    }
}

/// What a refusal of read-write adds: the export without `--read-write` still works.
const WITHOUT_READ_WRITE: &str = "Without --read-write, the export gives a read-only token.";

/// A call on the accounts themselves, for [`explain_account_error`]. (`Remove`
/// forgets a folder, and is a [`SyncAction`].)
#[derive(Debug, Clone, Copy)]
pub enum AccountAction<'a> {
    /// `Accounts.Add`, with the label asked for.
    Add(&'a str),
    /// `Account.SetLabel`: the account's label, and the one asked for.
    Rename(&'a str, &'a str),
    /// `Accounts.SetClientId`: the id given, and the labels of the accounts
    /// signed in or signing in when it was refused.
    SetClientId(&'a str, &'a [String]),
    /// `Account.BeginSignIn`, with the account's label.
    SignIn(&'a str),
    /// `Account.SignOut`, with the account's label.
    SignOut(&'a str),
    /// `Account.SetMode`: the account's label, the mode asked for, and how a command
    /// suggested about the account starts ([`command_prefix`]).
    SetMode(&'a str, &'a str, &'a str),
    /// `Accounts.SetPauseOnMetered` or `SetOnBattery`: a setting every account shares.
    Settings,
}

/// What to tell a person when a call on the accounts failed: `Accounts.Add`,
/// `Accounts.SetClientId`, `Account.SetLabel`, `BeginSignIn` or `SignOut`.
/// These refuse under the bus's own names — `InvalidArgs` for a label or a
/// client id the rules refuse, `Failed` for the rest — so the name decides,
/// and the daemon's message is kept as the reason.
pub fn explain_account_error(action: AccountAction<'_>, error: &zbus::Error) -> String {
    let detail = match error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    };
    account_refusal_text(action, error_name(error), &detail)
}

/// [`explain_account_error`]'s decision, on the name and message alone.
pub fn account_refusal_text(action: AccountAction<'_>, name: Option<&str>, detail: &str) -> String {
    use AccountAction::*;
    let invalid = name == Some("org.freedesktop.DBus.Error.InvalidArgs");
    let failed = name == Some("org.freedesktop.DBus.Error.Failed");
    let ours = name.and_then(|name| name.strip_prefix(ERROR_PREFIX)).and_then(|rest| rest.strip_prefix('.'));
    match action {
        SetMode(label, _, _) if ours == Some("WritesNotAllowed") => format!(
            "{label} was not switched to read-write: while uploads are being developed, only the test \
             accounts listed in write_test_drive_ids in ~/.config/konedrive/config.toml can be, and \
             this account is not one of them. Nothing was changed"
        ),
        SetMode(label, _, prefix) if ours == Some("NotSignedIn") => format!(
            "{label} was not switched to read-write: it is not signed in. Sign in first: `{prefix} login`"
        ),
        SetMode(label, _, prefix) if ours == Some("PendingUploads") => format!(
            "{label} was not switched to read-only: {detail}. `{prefix} account mode read-only --force` \
             switches anyway"
        ),
        SetMode(label, mode, _) => format!("{label} was not switched to {mode}: {detail}"),
        Add(label) | Rename(_, label) if invalid => {
            format!("{label:?} cannot be an account's label: {detail}. {LABEL_RULE}")
        }
        SetClientId(id, _) if invalid => format!(
            "{id:?} is not an Application (client) ID. It is a GUID like \
             00000000-0000-0000-0000-000000000000: copy it from the Overview page of your app \
             registration in the Microsoft Entra admin center"
        ),
        // The daemon refuses a change while any account uses the old id.
        SetClientId(_, busy) if failed && !busy.is_empty() => {
            let (who, verb) = match busy {
                [one] => (one.clone(), "is"),
                several => (several.join(", "), "are"),
            };
            let sign_out: Vec<String> =
                busy.iter().map(|label| format!("`konedrivectl --account {} logout`", shell_word(label))).collect();
            format!(
                "the client ID was not changed: every account signs in with it, so it cannot change \
                 while {who} {verb} signed in or signing in. Sign out first: {}",
                sign_out.join(", ")
            )
        }
        SetClientId(..) => format!("the client ID was not saved: {detail}"),
        Add(label) => format!("the account {label:?} was not added: {detail}"),
        Rename(old, new) => format!("the account {old} was not renamed to {new:?}: {detail}"),
        SignIn(label) => format!("cannot start signing in to {label}: {detail}"),
        SignOut(label) => format!("cannot sign {label} out: {detail}"),
        Settings => format!("the setting was not changed: {detail}"),
    }
}

#[cfg(test)]
mod tests;
