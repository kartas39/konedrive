//! The sentence for a refused folder call, by the group of the action and the refusal.
//!
//! Each group of actions that has a sentence of its own for some refusal is a function
//! with a match over every [`Refusal`]; what it has no sentence for gets [`shared`]'s, the
//! one written for whichever action was refused that way first. A refusal added to the
//! enum does not compile here until [`shared`] and every group say what they print for it.
//!
//! The operations on a file that Dolphin's menu has too (`pin`, `unpin`, `free`, `open`)
//! are not written here: their sentences are the catalogue's (`konedrive-text`, its
//! `files`), printed as they are written there, and [`shared`] takes from it the
//! sentences it has in common with them. Only where the catalogue has none for the
//! command line does such an operation get [`shared`]'s. A sentence about one file is said
//! of one path; of several paths none of which the refusal names, the catalogue's list form.

use konedrive_dbus::{Refusal, ERROR_PREFIX};
use konedrive_text::files::{self, Operation};
use konedrive_text::Client;

use super::SyncAction;
use crate::text::files::pinned_parts;

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
    /// The action's path is several paths, and the refusal names none of them.
    pub several: bool,
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

    /// The operation of Dolphin's menu this action is, if it is one.
    fn operation(&self) -> Option<Operation> {
        match self.action {
            SyncAction::Pin(_) => Some(Operation::Keep),
            SyncAction::Unpin(_) => Some(Operation::Unpin),
            SyncAction::Free(_) => Some(Operation::FreeUp),
            SyncAction::Open(_) => Some(Operation::OpenOnline),
            _ => None,
        }
    }

    /// The catalogue's sentence for `refusal` of `operation` as the command line says
    /// it, about `file`; `None` where it has none.
    fn catalogue_about(&self, file: &str, operation: Operation, refusal: &Refusal) -> Option<String> {
        // An error with no message carries its name in its place.
        let detail = if self.detail.starts_with(ERROR_PREFIX) { "" } else { self.detail };
        let told = files::Told { file, detail, prefix: self.prefix, folder: &self.folder() };
        files::text(operation, refusal, Client::CommandLine, &told)
    }

    /// [`Told::catalogue_about`] the path of the action — or, of several paths none of
    /// which the refusal names, the catalogue's list form: a sentence written for one file
    /// never has a list inside its quotes.
    fn catalogue(&self, operation: Operation, refusal: &Refusal) -> Option<String> {
        let listed = if self.several { files::text_of_several(operation, refusal, &self.as_any_failure()) } else { None };
        listed.or_else(|| self.catalogue_about(self.path(), operation, refusal))
    }

    /// What a sentence with nothing but the path and the daemon's message is filled with.
    fn as_any_failure(&self) -> files::Told<'_> {
        files::Told { file: self.path(), detail: self.detail, ..files::Told::default() }
    }
}

/// The actions that are told the same for every refusal.
enum Group {
    /// `register`.
    Registration,
    Forget,
    /// `account remove`, which forgets the folder first.
    Remove,
    Refresh,
    Hydrate,
    Dehydrate,
    FreeUpSpace,
    /// `pin`, `unpin`, `free`, `open`: what Dolphin's menu has too.
    FileOperation(Operation),
    /// What reads or steers the upload queue: `upload-queue`, `pause`, `resume`, `ignore`,
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
        Register(_) => Group::Registration,
        Forget => Group::Forget,
        Remove(_) => Group::Remove,
        Refresh => Group::Refresh,
        Hydrate(_) => Group::Hydrate,
        Dehydrate(_) => Group::Dehydrate,
        FreeUpSpace => Group::FreeUpSpace,
        Pin(_) => Group::FileOperation(Operation::Keep),
        Unpin(_) => Group::FileOperation(Operation::Unpin),
        Free(_) => Group::FileOperation(Operation::FreeUp),
        Open(_) => Group::FileOperation(Operation::OpenOnline),
        UploadQueue | Pause | Resume | Ignore | NotUploaded => Group::Uploads,
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
        Group::Refresh => refresh(told, refusal),
        Group::Hydrate => hydrate(told, refusal),
        Group::Dehydrate => dehydrate(told, refusal),
        Group::FreeUpSpace => free_up_space(told, refusal),
        Group::FileOperation(operation) => file_operation(told, operation, refusal),
        Group::Uploads => uploads(told, refusal),
        Group::Settings => settings(told, refusal),
    }
}

/// `Failed`, a name this CLI does not know yet, or an error from the bus itself: the
/// detail is all there is, so it is kept whole.
fn failed(told: &Told<'_>) -> String {
    let filled = told.as_any_failure();
    match told.operation() {
        Some(operation) => {
            let listed = if told.several { files::failed_of_several(operation, &filled) } else { None };
            listed.unwrap_or_else(|| files::failed(operation, &filled))
        }
        None => format!("{} failed: {}", told.action.doing(), told.detail),
    }
}

/// The sentence a refusal has for every action without one of its own.
fn shared(told: &Told<'_>, refusal: &Refusal) -> String {
    let Told { detail, root, prefix, .. } = *told;
    let path = told.path();
    let folder = told.folder();
    // The catalogue's sentence, written for a file operation and said of any command.
    let catalogue = |operation| told.catalogue(operation, refusal).unwrap_or_else(|| failed(told));
    match refusal {
        // `sync open`: OneDrive is asked for the address each time.
        // The daemon's message is the cause (no network, a locked secret
        // storage, an answer that cannot be read), shown when there is one.
        Refusal::Unreachable => catalogue(Operation::OpenOnline),
        Refusal::NotSignedIn => format!(
            "the account is not signed in, and `{prefix} sync register` binds the folder to the \
             account's OneDrive. Sign in first with `{prefix} login`"
        ),
        Refusal::NoAccount => "that account is gone: it was removed meanwhile. `konedrivectl \
             account list` shows the accounts there are"
            .to_owned(),
        // `docs/design/accounts.md` §6.3: the folders of two accounts never nest. The daemon's
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
        Refusal::AlreadyRegistered if !root.is_empty() && path == root => format!(
            "{path} is already the sync folder. To register it again, run \
             `{prefix} sync forget` first; it leaves the files in the folder as they are"
        ),
        Refusal::AlreadyRegistered => format!(
            "this account already has a sync folder{folder}, and an account keeps only one. To \
             use {path} instead, run `{prefix} sync forget` first; it leaves the files in the \
             old folder as they are. For another OneDrive account, add an account of its own: \
             `konedrivectl account add`"
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
            "no sync folder is registered. Register one first with `{prefix} sync register <folder>`"
        ),
        Refusal::NoSource => catalogue(Operation::Keep),
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
        Refusal::NotHydrated | Refusal::ModifiedLocally | Refusal::InUse => catalogue(Operation::FreeUp),
        // With no path of the command's own, the daemon's message names the file.
        Refusal::NotUploaded => {
            let file = if path.is_empty() { detail.split(" is not uploaded").next().unwrap_or(detail) } else { path };
            told.catalogue_about(file, Operation::FreeUp, refusal).unwrap_or_else(|| failed(told))
        }
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
    match refusal {
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
    match refusal {
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

/// `sync refresh`.
fn refresh(told: &Told<'_>, refusal: &Refusal) -> String {
    match refusal {
        Refusal::Unsupported => format!(
            "this folder is not connected to OneDrive, so there is nothing to ask for"
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
    match refusal {
        Refusal::NotManaged => told.catalogue(Operation::FreeUp, refusal).unwrap_or_else(|| shared(told, refusal)),
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

/// `sync free-up-space`.
fn free_up_space(told: &Told<'_>, refusal: &Refusal) -> String {
    match refusal {
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

/// `sync pin`, `unpin`, `free` and `open`: the catalogue's sentence, and where it has none
/// for the command line, [`shared`]'s.
fn file_operation(told: &Told<'_>, operation: Operation, refusal: &Refusal) -> String {
    // The daemon's message names the path and the folder that keeps it: read here, to
    // name the command that unpins the folder.
    if let (Refusal::NotAllowed, Operation::Unpin, Some((_, by))) = (refusal, operation, pinned_parts(told.detail)) {
        let path = told.path();
        return format!(
            "{path} is kept on this device because the folder {by} is, so it cannot stop being \
             kept on its own. `konedrivectl sync unpin {by}` stops keeping the folder"
        );
    }
    told.catalogue(operation, refusal).unwrap_or_else(|| shared(told, refusal))
}

/// `sync upload-queue`, `pause`, `resume`, `ignore`, `not-uploaded` and the large delete.
fn uploads(told: &Told<'_>, refusal: &Refusal) -> String {
    match refusal {
        Refusal::Unsupported => "this folder is not connected to OneDrive, so nothing is uploaded from it".to_owned(),
        // A pattern that can match no name (`sync ignore add`): the daemon's message is the
        // pattern and what is wrong with it.
        Refusal::InvalidArgs if matches!(told.action, SyncAction::Ignore) => format!("the ignore list was not changed: {}", told.detail),
        Refusal::NoRoot
        | Refusal::NotEmpty
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
