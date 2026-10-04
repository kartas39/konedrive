//! The sentence for a refused folder call, by the group of the action and the refusal.
//!
//! Each group of actions that has a sentence of its own for some refusal is a function
//! with a match over every [`Refusal`]; what it has no sentence for gets [`shared`]'s, the
//! one written for whichever action was refused that way first. A refusal added to the
//! enum does not compile here until [`shared`] and every group say what they print for it.

use konedrive_dbus::{Refusal, ERROR_PREFIX};

use super::SyncAction;
use crate::text::files::pinned_parts;
use crate::text::formats::shell_word;

/// What was refused, and what the sentences are made of.
#[derive(Clone, Copy)]
pub(super) struct Told<'a> {
    pub action: SyncAction<'a>,
    /// The daemon's message.
    pub detail: &'a str,
    /// The registered folder, possibly empty.
    pub root: &'a str,
    /// How every command suggested for this account begins.
    pub prefix: &'a str,
}

impl Told<'_> {
    fn path(&self) -> &str {
        self.action.path()
    }

    /// " (/the/folder)", or nothing when no folder is known.
    fn folder(&self) -> String {
        if self.root.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.root)
        }
    }
}

/// The actions that are told the same for every refusal.
enum Group {
    /// `register`, `register-without-interception`.
    Registration,
    Forget,
    /// `account remove`, which forgets the folder first.
    Remove,
    PopulateFrom,
    Refresh,
    Hydrate,
    Dehydrate,
    FreeUpSpace,
    /// `pin`, `unpin`, `free`.
    Pins,
    Open,
    /// What reads or steers the outbox: `outbox`, `pause`, `resume`, `ignore`,
    /// `not-uploaded`, the large delete.
    Uploads,
    /// `thumbnails`, `anyway`.
    Settings,
    /// `skipped`, `activity`, `conflicts`, `dismiss`.
    Lists,
}

fn group(action: SyncAction<'_>) -> Group {
    use SyncAction::*;
    match action {
        Register(_) | RegisterWithoutInterception(_) => Group::Registration,
        Forget => Group::Forget,
        Remove(_) => Group::Remove,
        PopulateFrom(_) => Group::PopulateFrom,
        Refresh => Group::Refresh,
        Hydrate(_) => Group::Hydrate,
        Dehydrate(_) => Group::Dehydrate,
        FreeUpSpace => Group::FreeUpSpace,
        Pin(_) | Unpin(_) | Free(_) => Group::Pins,
        Open(_) => Group::Open,
        Outbox | Pause | Resume | Ignore | NotUploaded | Deletes => Group::Uploads,
        Settings | Anyway => Group::Settings,
        Skipped | Activity | Conflicts | Dismiss(_) => Group::Lists,
    }
}

/// The sentence for `refusal` of `told.action`. An error that carries no name (`None`) has
/// only its own words.
pub(super) fn text(told: &Told<'_>, refusal: Option<&Refusal>) -> String {
    let Some(refusal) = refusal else { return failed(told) };
    match group(told.action) {
        // No sentence of their own: the shared ones were written for a registration, and
        // nothing was written for the lists.
        Group::Registration | Group::Lists => shared(told, refusal),
        Group::Forget => forget(told, refusal),
        Group::Remove => remove(told, refusal),
        Group::PopulateFrom => populate_from(told, refusal),
        Group::Refresh => refresh(told, refusal),
        Group::Hydrate => hydrate(told, refusal),
        Group::Dehydrate => dehydrate(told, refusal),
        Group::FreeUpSpace => free_up_space(told, refusal),
        Group::Pins => pins(told, refusal),
        Group::Open => open(told, refusal),
        Group::Uploads => uploads(told, refusal),
        Group::Settings => settings(told, refusal),
    }
}

/// `Failed`, a name this CLI does not know yet, or an error from the bus itself: the
/// detail is all there is, so it is kept whole.
fn failed(told: &Told<'_>) -> String {
    format!("{} failed: {}", told.action.doing(), told.detail)
}

/// The sentence a refusal has for every action without one of its own.
fn shared(told: &Told<'_>, refusal: &Refusal) -> String {
    let Told { detail, root, prefix, .. } = *told;
    let path = told.path();
    let folder = told.folder();
    match refusal {
        // `sync open` (issue #53): OneDrive is asked for the address each time.
        // The daemon's message is the cause (no network, a locked secret
        // storage, an answer that cannot be read), shown when there is one.
        Refusal::Unreachable if detail.is_empty() || detail.starts_with(ERROR_PREFIX) => {
            "OneDrive could not be reached".to_owned()
        }
        Refusal::Unreachable => format!("OneDrive could not be reached: {detail}"),
        Refusal::NotSignedIn => format!(
            "the account is not signed in, and `{prefix} sync register` binds the folder to the \
             account's OneDrive. Sign in first with `{prefix} login` — or, in the developer's mode \
             with local files, use `{prefix} sync register-without-interception {path}`: without \
             the helper, files that are not downloaded read as zeros until you hydrate them"
        ),
        Refusal::PendingUploads => format!(
            "the sync folder{folder} is still registered, and nothing was changed: {detail}. `{prefix} \
             sync outbox` lists what waits; `{prefix} account mode read-only --force` drops it — the \
             files stay here as they are, and OneDrive does not get the changes — and the folder can \
             be forgotten then"
        ),
        Refusal::NoAccount => "that account is gone: it was removed meanwhile. `konedrivectl \
             account list` shows the accounts there are"
            .to_owned(),
        // Design §8.3: the folders of two accounts never nest. The daemon's
        // message names the other account.
        Refusal::Overlaps => format!(
            "{path} cannot be this account's folder: {detail}. The folders of two accounts cannot \
             be one inside the other: choose a folder outside every other account's folder \
             (`konedrivectl account list` shows them)"
        ),
        Refusal::NoConflict => format!(
            "{path} is not in the list of conflicts, so there was nothing to dismiss. `{prefix} \
             sync conflicts` lists them, each under the path where your version is kept"
        ),
        // HS2: a OneDrive folder is registered with the helper or not at
        // all; without interception is the developer's mode, never OneDrive.
        Refusal::NoHelper => format!(
            "the konedrive helper is not connected, so {path} was not registered: only through \
             the helper is a OneDrive folder kept in step and a file downloaded when something \
             opens it. Start the helper and try again (`{prefix} sync register-without-interception` \
             is the developer's mode, for a local folder whose files read as zeros until hydrated)"
        ),
        // What a restored folder that is still waiting for its helper
        // answers, among others: asking for the same folder again, in the
        // other mode.
        Refusal::AlreadyRegistered if !root.is_empty() && path == root => format!(
            "{path} is already the sync folder. To register it again another way, run \
             `{prefix} sync forget` first; it leaves the files in the folder as they are"
        ),
        Refusal::AlreadyRegistered => format!(
            "this account already has a sync folder{folder}, and an account keeps only one. To \
             use {path} instead, run `{prefix} sync forget` first; it leaves the files in the \
             old folder as they are. For another OneDrive account, add an account of its own: \
             `konedrivectl account add <label>`"
        ),
        Refusal::NotEmpty => format!(
            "{path} is not empty. A new sync folder has to start empty, so that nothing already \
             in it is mistaken for a OneDrive file: choose an empty folder, or create a new one"
        ),
        Refusal::Unsupported => {
            let why = detail.strip_prefix(&format!("{path}: ")).unwrap_or(detail);
            format!("{path} cannot be used as the sync folder: {why}")
        }
        Refusal::NoRoot => format!(
            "no sync folder is registered. Register one first with `{prefix} sync register \
             <folder>` — or, in the developer's mode with local files, `{prefix} sync \
             register-without-interception <folder>`"
        ),
        Refusal::NoSource => format!(
            "KOneDrive does not know where to download {path} from yet. Run `{prefix} sync \
             populate-from <dir>` first; the daemon does not remember that directory across a \
             restart, so run it again after one (files already there are left alone)"
        ),
        Refusal::OutsideRoot => format!(
            "{path} is not a regular file inside the sync folder{folder}. Only files inside it \
             can be downloaded or freed up — not folders, symbolic links, or anything outside it"
        ),
        // A free-up of something "Always keep on this device" keeps here.
        // The daemon names what pins it: "pinned by <path>: unpin it first".
        Refusal::NotAllowed => match pinned_parts(detail).map(|(_, by)| by) {
            Some(by) if by == path => format!(
                "{path} is kept on this device, so its space is not freed up. `konedrivectl sync \
                 free {by}` stops keeping it and frees it up"
            ),
            Some(by) => format!(
                "{path} is kept on this device because the folder {by} is, so its space is not \
                 freed up. To free it, free up the folder first: `konedrivectl sync free {by}` \
                 stops keeping it and frees up what is in it"
            ),
            None => format!("{} was refused: {detail}", told.action.doing()),
        },
        Refusal::NotManaged => format!(
            "{path} is not a OneDrive file: it is a file of your own in the sync folder, so \
             there is nothing to download"
        ),
        Refusal::NotHydrated => format!(
            "{path} is not downloaded, so there is no space to free — it already takes none"
        ),
        Refusal::NotUploaded => format!(
            "{} is not uploaded yet, so freeing up its space would lose the changes made here. It \
             was left as it is; its space can be freed once it is uploaded (`{prefix} sync outbox`)",
            if path.is_empty() { detail.split(" is not uploaded").next().unwrap_or(detail) } else { path }
        ),
        Refusal::ModifiedLocally => format!(
            "{path} was changed here and has not been uploaded, so freeing its space would lose \
             your edits. It was left exactly as it is"
        ),
        Refusal::InUse => format!(
            "{path} is open in another program, so its space cannot be freed right now. Close \
             it there and try again"
        ),
        // A folder that is recorded and not up, or whose sync is not running: the daemon's
        // message is the whole of it, with why.
        Refusal::NotUp if detail.is_empty() || detail.starts_with(ERROR_PREFIX) => {
            format!("the sync folder{folder} is not up, so nothing was done. `{prefix} sync status` shows what it waits for")
        }
        // The daemon's message says why, and whether it tried to bring the folder up.
        Refusal::NotUp => format!(
            "the sync folder{folder} is not up, so the command could not go ahead: {}. `{prefix} sync status` \
             shows its state; `{prefix} sync forget` takes the folder away, and leaves its files as they are",
            detail.strip_prefix("the folder is not up: ").unwrap_or(detail)
        ),
        Refusal::Failed
        | Refusal::WritesNotAllowed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::Internal
        | Refusal::Other(_) => failed(told),
    }
}

/// `sync forget`.
fn forget(told: &Told<'_>, refusal: &Refusal) -> String {
    let folder = told.folder();
    match refusal {
        // A folder registered with the helper is forgotten
        // through the helper or not at all.
        Refusal::NoHelper => format!(
            "the konedrive helper is not connected, so the sync folder{folder} is still \
             registered and nothing was changed. Forgetting it has to tell the helper to stop \
             watching it; without that, a file freed up there later could read as zeros from \
             then on. Try again once the helper is back (`konedrivectl sync status` shows \
             when it is)"
        ),
        Refusal::NoRoot => "no sync folder is registered, so there is nothing to forget".to_owned(),
        Refusal::NotEmpty
        | Refusal::Unsupported
        | Refusal::InUse
        | Refusal::NotManaged
        | Refusal::NotHydrated
        | Refusal::ModifiedLocally
        | Refusal::OutsideRoot
        | Refusal::AlreadyRegistered
        | Refusal::NotSignedIn
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NoAccount
        | Refusal::NotUploaded
        | Refusal::PendingUploads
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::WritesNotAllowed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => shared(told, refusal),
    }
}

/// `account remove`: `Accounts.Remove` forgets the folder first, under Forget's rule.
fn remove(told: &Told<'_>, refusal: &Refusal) -> String {
    let SyncAction::Remove(label) = told.action else { return shared(told, refusal) };
    let detail = told.detail;
    let folder = told.folder();
    match refusal {
        Refusal::NoHelper => format!(
            "the konedrive helper is not connected, so the account {label} was not removed and \
             nothing was changed. Removing it forgets its folder{folder}, and forgetting a folder \
             registered with the helper has to tell the helper to stop watching it; without that, \
             a file freed up there later could read as zeros from then on. Try again once the \
             helper is back (`konedrivectl sync status` shows when it is)"
        ),
        // The changes waiting to upload would go with the account's folder.
        Refusal::PendingUploads => {
            let prefix = format!("konedrivectl --account {}", shell_word(label));
            format!(
                "the account {label} was not removed, and nothing was changed: {detail}. `{prefix} sync \
                 outbox` lists what waits; `{prefix} account mode read-only --force` drops it — the \
                 files stay here as they are, and OneDrive does not get the changes — and the account \
                 can be removed then"
            )
        }
        Refusal::NoAccount => format!(
            "there is no account {label} any more, so nothing was removed. `konedrivectl account \
             list` shows the accounts there are"
        ),
        Refusal::NotEmpty
        | Refusal::Unsupported
        | Refusal::InUse
        | Refusal::NoRoot
        | Refusal::NotManaged
        | Refusal::NotHydrated
        | Refusal::ModifiedLocally
        | Refusal::OutsideRoot
        | Refusal::AlreadyRegistered
        | Refusal::NotSignedIn
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NotUploaded
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::WritesNotAllowed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => shared(told, refusal),
    }
}

/// `sync populate-from`.
fn populate_from(told: &Told<'_>, refusal: &Refusal) -> String {
    match refusal {
        // the source overlaps the sync folder.
        Refusal::Unsupported => format!("the sync folder cannot be filled from {}: {}", told.path(), told.detail),
        Refusal::NotEmpty
        | Refusal::InUse
        | Refusal::NoRoot
        | Refusal::NoHelper
        | Refusal::NotManaged
        | Refusal::NotHydrated
        | Refusal::ModifiedLocally
        | Refusal::OutsideRoot
        | Refusal::AlreadyRegistered
        | Refusal::NotSignedIn
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NoAccount
        | Refusal::NotUploaded
        | Refusal::PendingUploads
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::WritesNotAllowed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => shared(told, refusal),
    }
}

/// `sync refresh`.
fn refresh(told: &Told<'_>, refusal: &Refusal) -> String {
    let prefix = told.prefix;
    match refusal {
        // said of `refresh`, the registration text
        // read "and  was not registered", with no path.
        Refusal::NoHelper => "the konedrive helper is not connected, so the folder is not \
             kept in step with OneDrive until it is, and nothing was asked for"
            .to_owned(),
        Refusal::Unsupported => format!(
            "this folder is not connected to OneDrive, so there is nothing to ask for: it was \
             registered while signed out and is filled with `{prefix} sync populate-from`"
        ),
        Refusal::NotEmpty
        | Refusal::InUse
        | Refusal::NoRoot
        | Refusal::NotManaged
        | Refusal::NotHydrated
        | Refusal::ModifiedLocally
        | Refusal::OutsideRoot
        | Refusal::AlreadyRegistered
        | Refusal::NotSignedIn
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NoAccount
        | Refusal::NotUploaded
        | Refusal::PendingUploads
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::WritesNotAllowed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => shared(told, refusal),
    }
}

/// `sync hydrate`.
fn hydrate(told: &Told<'_>, refusal: &Refusal) -> String {
    let path = told.path();
    match refusal {
        // follow-up: a file that may still carry the helper's
        // ignore mark is only downloaded again once the helper has cleared
        // it, since a download that fails empties the file.
        Refusal::NoHelper => format!(
            "the konedrive helper is not connected, so nothing was changed. {path} was left \
             half freed up, or is marked downloaded with nothing to show it was, and downloading \
             it again must first have the helper stop letting it through unchecked — a download \
             that failed partway would otherwise leave it reading as zeros. Try again once the \
             helper is back (`konedrivectl sync status` shows when it is)"
        ),
        Refusal::ModifiedLocally => format!(
            "{path} was changed here and has not been uploaded, so downloading it again would \
             overwrite your edits. It was left exactly as it is"
        ),
        Refusal::NotEmpty
        | Refusal::Unsupported
        | Refusal::InUse
        | Refusal::NoRoot
        | Refusal::NotManaged
        | Refusal::NotHydrated
        | Refusal::OutsideRoot
        | Refusal::AlreadyRegistered
        | Refusal::NotSignedIn
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NoAccount
        | Refusal::NotUploaded
        | Refusal::PendingUploads
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::WritesNotAllowed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => shared(told, refusal),
    }
}

/// `sync dehydrate`.
fn dehydrate(told: &Told<'_>, refusal: &Refusal) -> String {
    let path = told.path();
    match refusal {
        // In a folder registered without interception too: a
        // helper that is running while this daemon has no connection to it
        // may hold a mark on the file that nothing can clear.
        Refusal::NoHelper => format!(
            "the konedrive helper is not connected, so nothing was changed. Freeing up {path} \
             must first have the helper take off any mark that lets the file's opens through \
             unchecked, or the emptied file could read as zeros from then on; try again once \
             the daemon is connected to the helper again — it reconnects on its own"
        ),
        Refusal::NotManaged => format!(
            "{path} is not a OneDrive file: it is a file of your own in the sync folder, and \
             KOneDrive never frees the space of a file it could not download again"
        ),
        Refusal::NotEmpty
        | Refusal::Unsupported
        | Refusal::InUse
        | Refusal::NoRoot
        | Refusal::NotHydrated
        | Refusal::ModifiedLocally
        | Refusal::OutsideRoot
        | Refusal::AlreadyRegistered
        | Refusal::NotSignedIn
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NoAccount
        | Refusal::NotUploaded
        | Refusal::PendingUploads
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::WritesNotAllowed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => shared(told, refusal),
    }
}

/// A file that may still carry the helper's ignore mark is only freed up once the helper
/// has cleared it: `free-up-space`, and `free`.
fn nothing_more_freed() -> String {
    "the konedrive helper is not connected, so nothing more \
     was freed up. Freeing up a file must first have the helper take off any mark that lets \
     the file's opens through unchecked, or the emptied file could read as zeros from then \
     on; try again once the daemon is connected to the helper again — it reconnects on its \
     own"
        .to_owned()
}

/// `sync free-up-space`.
fn free_up_space(told: &Told<'_>, refusal: &Refusal) -> String {
    match refusal {
        Refusal::NoHelper => nothing_more_freed(),
        Refusal::NotEmpty
        | Refusal::Unsupported
        | Refusal::InUse
        | Refusal::NoRoot
        | Refusal::NotManaged
        | Refusal::NotHydrated
        | Refusal::ModifiedLocally
        | Refusal::OutsideRoot
        | Refusal::AlreadyRegistered
        | Refusal::NotSignedIn
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NoAccount
        | Refusal::NotUploaded
        | Refusal::PendingUploads
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::WritesNotAllowed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => shared(told, refusal),
    }
}

/// `sync pin`, `unpin` and `free`.
fn pins(told: &Told<'_>, refusal: &Refusal) -> String {
    let path = told.path();
    let folder = told.folder();
    match (refusal, told.action) {
        (Refusal::NoHelper, SyncAction::Free(_)) => nothing_more_freed(),
        (Refusal::NoHelper, _) => shared(told, refusal),
        (Refusal::OutsideRoot, _) => format!(
            "{path}: only files and folders inside the sync folder{folder} can be kept on this \
             device or freed up — not symbolic links, or anything outside it"
        ),
        (Refusal::NotAllowed, SyncAction::Unpin(_)) => match pinned_parts(told.detail) {
            Some((_, by)) => format!(
                "{path} is kept on this device because the folder {by} is, so it cannot stop being \
                 kept on its own. `konedrivectl sync unpin {by}` stops keeping the folder"
            ),
            None => shared(told, refusal),
        },
        (Refusal::NotAllowed, _) => shared(told, refusal),
        (Refusal::NotManaged, _) => format!(
            "{path}: a file of your own in the sync folder is not a OneDrive file, so there is \
             nothing to keep on this device or to free up"
        ),
        (
            Refusal::NotEmpty
            | Refusal::Unsupported
            | Refusal::InUse
            | Refusal::NoRoot
            | Refusal::NotHydrated
            | Refusal::ModifiedLocally
            | Refusal::AlreadyRegistered
            | Refusal::NotSignedIn
            | Refusal::NoSource
            | Refusal::NoConflict
            | Refusal::Overlaps
            | Refusal::NoAccount
            | Refusal::NotUploaded
            | Refusal::PendingUploads
            | Refusal::Unreachable
            | Refusal::Failed
            | Refusal::WritesNotAllowed
            | Refusal::ModeNotGranted
            | Refusal::InvalidArgs
            | Refusal::BusFailed
            | Refusal::UnknownObject
            | Refusal::UnknownMethod
            | Refusal::UnknownInterface
            | Refusal::NotUp
            | Refusal::Internal
            | Refusal::Other(_),
            _,
        ) => shared(told, refusal),
    }
}

/// `sync open`: `Files.WebUrl`.
fn open(told: &Told<'_>, refusal: &Refusal) -> String {
    let prefix = told.prefix;
    let path = told.path();
    let folder = told.folder();
    match refusal {
        Refusal::NotSignedIn => format!(
            "the account is not signed in, so OneDrive cannot be asked for the page of {path}. Sign in \
             with `{prefix} login` and try again"
        ),
        Refusal::NotUploaded => format!(
            "{path} is not uploaded yet, so it has no page in OneDrive. It can be opened there once it \
             is uploaded (`{prefix} sync outbox`)"
        ),
        Refusal::NotManaged => format!(
            "{path} is not a OneDrive file: it is a file of your own in the sync folder, so it has no \
             page in OneDrive"
        ),
        Refusal::OutsideRoot => format!(
            "{path}: only files and folders inside the sync folder{folder}, and the folder itself, have \
             a page in OneDrive — not symbolic links, or anything outside it"
        ),
        Refusal::NotEmpty
        | Refusal::Unsupported
        | Refusal::InUse
        | Refusal::NoRoot
        | Refusal::NoHelper
        | Refusal::NotHydrated
        | Refusal::ModifiedLocally
        | Refusal::AlreadyRegistered
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NoAccount
        | Refusal::PendingUploads
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::WritesNotAllowed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => shared(told, refusal),
    }
}

/// `sync outbox`, `pause`, `resume`, `ignore`, `not-uploaded` and the large delete.
fn uploads(told: &Told<'_>, refusal: &Refusal) -> String {
    match refusal {
        Refusal::Unsupported => "this folder is not connected to OneDrive, so nothing is uploaded from it".to_owned(),
        // A pattern that can match no name (`sync ignore add`): the daemon's message is the
        // pattern and what is wrong with it.
        Refusal::InvalidArgs if matches!(told.action, SyncAction::Ignore) => format!("the ignore list was not changed: {}", told.detail),
        Refusal::NoRoot
        | Refusal::NotEmpty
        | Refusal::InUse
        | Refusal::NoHelper
        | Refusal::NotManaged
        | Refusal::NotHydrated
        | Refusal::ModifiedLocally
        | Refusal::OutsideRoot
        | Refusal::AlreadyRegistered
        | Refusal::NotSignedIn
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NoAccount
        | Refusal::NotUploaded
        | Refusal::PendingUploads
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::WritesNotAllowed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => shared(told, refusal),
    }
}

/// `sync thumbnails` and `sync anyway`.
fn settings(told: &Told<'_>, refusal: &Refusal) -> String {
    match refusal {
        Refusal::Unsupported => "this folder is not connected to OneDrive, so it has no sync settings".to_owned(),
        Refusal::NotEmpty
        | Refusal::InUse
        | Refusal::NoRoot
        | Refusal::NoHelper
        | Refusal::NotManaged
        | Refusal::NotHydrated
        | Refusal::ModifiedLocally
        | Refusal::OutsideRoot
        | Refusal::AlreadyRegistered
        | Refusal::NotSignedIn
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NoAccount
        | Refusal::NotUploaded
        | Refusal::PendingUploads
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::WritesNotAllowed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => shared(told, refusal),
    }
}
