use konedrive_dbus::Refusal;

mod account;
mod folder;

#[cfg(test)]
pub use account::{account_refusal_text, dev_refusal_text};
pub use account::{explain_account_error, explain_dev_error, AccountAction};

/// The `sync` subcommand a refusal answered, with the path it named — the
/// context a refusal needs to be explained in terms of *this* user's file.
#[derive(Debug, Clone, Copy)]
pub enum SyncAction<'a> {
    Register(&'a str),
    Forget,
    Hydrate(&'a str),
    Dehydrate(&'a str),
    Refresh,
    Skipped,
    Activity,
    Conflicts,
    Dismiss(&'a str),
    FreeUpSpace,
    /// The paths given, joined with ", " ([`Context::several`] says when they are several).
    Pin(&'a str),
    /// The path refused ([`refused_path`](crate::text::files::refused_path)), or the paths given, joined with ", ".
    Unpin(&'a str),
    /// The path refused ([`refused_path`](crate::text::files::refused_path)), or the paths given, joined with ", ".
    Free(&'a str),
    /// `sync open`: `Files.WebUrl`, with the path.
    Open(&'a str),
    /// `account remove`, with the account's label: `Accounts.Remove` forgets the
    /// folder as `Forget` does, and is refused under the same names.
    Remove(&'a str),
    UploadQueue,
    Pause,
    Resume,
    Ignore,
    /// `sync thumbnails`, `sync free-space`: an account's own sync setting.
    Settings,
    /// `sync anyway`.
    Anyway,
    NotUploaded,
}

impl SyncAction<'_> {
    /// "downloading /path", for the one sentence every refusal without a
    /// name of its own is built from.
    fn doing(&self) -> String {
        match self {
            Self::Register(path) => format!("registering {path}"),
            Self::Forget => "forgetting the sync folder".to_owned(),
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
            Self::UploadQueue => "listing the changes waiting to upload".to_owned(),
            Self::Pause => "pausing the sync".to_owned(),
            Self::Resume => "resuming the sync".to_owned(),
            Self::Ignore => "changing the ignore list".to_owned(),
            Self::Settings => "changing the sync settings".to_owned(),
            Self::Anyway => "syncing anyway".to_owned(),
            Self::NotUploaded => "listing what is not uploaded".to_owned(),
        }
    }

    fn path(&self) -> &str {
        match self {
            Self::Register(path)
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
            | Self::UploadQueue
            | Self::Pause
            | Self::Resume
            | Self::Ignore
            | Self::Settings
            | Self::Anyway
            | Self::NotUploaded => "",
        }
    }
}

/// Whether `error` is a call the daemon did not answer within [`ANSWER_LIMIT`](crate::ANSWER_LIMIT):
/// what zbus ends a call with when its connection's limit ran out.
pub fn is_unanswered(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::InputOutput(io) if io.kind() == std::io::ErrorKind::TimedOut)
}

/// What a command says of a call that [`is_unanswered`].
pub fn unanswered_text() -> String {
    format!("konedrived did not answer within {} seconds", crate::ANSWER_LIMIT.as_secs())
}

/// What a failed call says: the refusal its name names, if it carries a name, and the
/// message — or, of an error with no message, its name; of a call that got no answer,
/// [`unanswered_text`]; of any other error that is no reply, what it says of itself. Every `explain_*` reads an error through this, once.
fn read_error(error: &zbus::Error) -> (Option<Refusal>, String) {
    let detail = match error {
        unanswered if is_unanswered(unanswered) => unanswered_text(),
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    };
    (Refusal::from_error(error), detail)
}

/// What the CLI knows of the daemon besides a refusal, read after it: the
/// folder of the account the refusal is about (`Folder.Path`: the chosen
/// account's, or for a path, the folder that holds it), what it shows
/// (`RootSource`), and for a path, every account's folder. Any of them may be
/// empty.
#[derive(Debug, Default, Clone, Copy)]
pub struct Context<'a> {
    pub root: &'a str,
    pub source: &'a str,
    /// Every account's registered folder, for a path command: a path in none
    /// of them is refused `OutsideRoot` before any account sees it.
    pub folders: &'a [String],
    /// The folder a registration named carries another account's drive
    /// (`user.konedrive.drive`, `docs/design/accounts.md` §6.3), which the daemon refuses under
    /// `NotEmpty` too.
    pub foreign: bool,
    /// How a suggested command names the account ([`command_prefix`](crate::choice::command_prefix)); empty
    /// for plain `konedrivectl`.
    pub prefix: &'a str,
    /// The action's path is several paths joined with ", ", and the refusal
    /// names none of them: a sentence about one file is not said of the list.
    pub several: bool,
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

/// What to tell a person when a folder call made for `action` failed.
///
/// Decides by the D-Bus error **name** ([`Refusal`]), never the
/// message: every refusal the folder makes arrives under
/// `konedrive_dbus::ERROR_PREFIX`, and each gets a sentence saying what
/// happened to the user's file and what they can do about it.
///
/// The daemon's own message is kept only where it is the specific part:
/// `Unsupported` (which filesystem feature is missing) and anything with no
/// name of its own (`Failed`, or an error from the bus itself), where it is
/// all there is.
///
/// [`Context`] adds what the CLI knows besides: a folder that is not
/// connected yet is told so, a path in no account's folder is told which
/// folders there are, and a folder that was another account's is told so.
pub fn explain_sync_error(action: SyncAction<'_>, error: &zbus::Error, context: Context<'_>) -> String {
    let (refusal, detail) = read_error(error);
    text_in(action, refusal.as_ref(), &detail, context)
}

/// [`explain_sync_error`]'s decision, on the name and message alone.
#[cfg(test)]
pub fn refusal_text_in(action: SyncAction<'_>, name: Option<&str>, detail: &str, context: Context<'_>) -> String {
    text_in(action, name.map(Refusal::parse).as_ref(), detail, context)
}

/// [`explain_sync_error`]'s decision, on the refusal read.
fn text_in(action: SyncAction<'_>, refusal: Option<&Refusal>, detail: &str, context: Context<'_>) -> String {
    use SyncAction::*;
    let path = action.path();
    let prefix = context.prefix();
    let told = folder::Told { action, detail, root: context.root, prefix, several: context.several };
    let path_command = matches!(action, Hydrate(_) | Dehydrate(_) | Pin(_) | Unpin(_) | Free(_) | Open(_));
    // An account removed while this command ran: its object is gone.
    if refusal.is_some_and(Refusal::is_gone) && !path_command {
        return folder::text(&told, Some(&Refusal::NoAccount));
    }
    match (refusal, action) {
        // `Files` refuses a path in no account's folder before any account
        // sees it; with no folder at all, that is `NoRoot`'s situation.
        (Some(Refusal::OutsideRoot), _) if path_command && context.root.is_empty() => {
            return match context.folders {
                [] => folder::text(&folder::Told { root: "", ..told }, Some(&Refusal::NoRoot)),
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
        (Some(Refusal::NotEmpty), Register(_)) if context.foreign => {
            return format!(
                "{path} holds the files of another OneDrive account's folder, so it cannot be this \
                 account's. Choose an empty folder, or create a new one. If it was this account's own \
                 folder before, sign the account in first (`{prefix} login`), then register it again"
            );
        }
        _ => {}
    }
    let text = folder::text(&told, refusal);
    match refusal {
        // A folder that shows OneDrive downloads from OneDrive; it has no
        // source yet only while it waits to be brought up.
        Some(Refusal::NoSource) if context.source == "onedrive" => "the folder is not connected yet; try again in a moment".to_owned(),
        _ => text,
    }
}

/// The sentence for `name` refused with `detail`, with nothing [`Context`] adds — so it
/// can be tested with a name and a message that disagree.
#[cfg(test)]
pub fn refusal_text(action: SyncAction<'_>, name: Option<&str>, detail: &str, root: &str) -> String {
    let told = folder::Told { action, detail, root, prefix: "konedrivectl", several: false };
    folder::text(&told, name.map(Refusal::parse).as_ref())
}

#[cfg(test)]
mod tests;
