//! A file that was not kept on this device, unpinned, freed up or opened in
//! OneDrive: the sentence of every [`Operation`] for every name a call is
//! refused under (`konedrive-dbus`'s [`Refusal`]), and what the Dolphin
//! plugin says of a call the daemon never answered.
//!
//! The places of a sentence: `{file}` (the file's name in the plugin, the
//! path on the command line), `{detail}` (the daemon's message), and on the
//! command line `{prefix}` (how a command for this account begins) and
//! `{folder}` (` (/the/folder)`, or nothing when it is not known). The
//! plugin's own sentences have `{was not}` ([`WAS_NOT`]) and `{count}` too.

use konedrive_dbus::Refusal;

use crate::Sentence::{Desktop, Each, Same};
use crate::{fill, Client, Sentence};

/// What was asked for a file: an entry of Dolphin's menu, and the `sync`
/// command that does the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// "Always keep on this device", checked; `sync pin`.
    Keep,
    /// "Always keep on this device", unchecked; `sync unpin`.
    Unpin,
    /// "Free up space"; `sync free`.
    FreeUp,
    /// "Open in OneDrive"; `sync open`.
    OpenOnline,
}

impl Operation {
    pub const ALL: [Operation; 4] = [Self::Keep, Self::Unpin, Self::FreeUp, Self::OpenOnline];
}

/// One of something for each [`Operation`].
#[derive(Debug, Clone, Copy)]
pub struct ByOperation<T> {
    pub keep: T,
    pub unpin: T,
    pub free_up: T,
    pub open_online: T,
}

impl<T: Copy> ByOperation<T> {
    pub const fn every(one: T) -> Self {
        Self { keep: one, unpin: one, free_up: one, open_online: one }
    }

    pub const fn of(&self, operation: Operation) -> T {
        match operation {
            Operation::Keep => self.keep,
            Operation::Unpin => self.unpin,
            Operation::FreeUp => self.free_up,
            Operation::OpenOnline => self.open_online,
        }
    }
}

impl<T> ByOperation<T> {
    pub fn map<U>(&self, f: impl Fn(&T) -> U) -> ByOperation<U> {
        ByOperation { keep: f(&self.keep), unpin: f(&self.unpin), free_up: f(&self.free_up), open_online: f(&self.open_online) }
    }
}

impl<T> ByOperation<Option<T>> {
    pub fn of_ref(&self, operation: Operation) -> Option<&T> {
        match operation {
            Operation::Keep => self.keep.as_ref(),
            Operation::Unpin => self.unpin.as_ref(),
            Operation::FreeUp => self.free_up.as_ref(),
            Operation::OpenOnline => self.open_online.as_ref(),
        }
    }
}

/// The words of one refusal.
#[derive(Debug, Clone)]
pub struct RefusalText {
    pub refusal: Refusal,
    /// The sentence for each operation. `None`: it has none of its own, and
    /// is told as any failure ([`FAILED`]) — on the command line, after its
    /// own table of folder commands had nothing either.
    pub sentence: ByOperation<Option<Sentence>>,
    /// Said instead, whatever the operation, when the daemon gave no message.
    pub without_detail: Option<&'static str>,
}

/// A refusal told the same for every operation.
const fn every(refusal: Refusal, sentence: Sentence) -> RefusalText {
    RefusalText { refusal, sentence: ByOperation::every(Some(sentence)), without_detail: None }
}

/// A refusal with no sentence of its own for a file operation.
const fn as_failed(refusal: Refusal) -> RefusalText {
    RefusalText { refusal, sentence: ByOperation::every(None), without_detail: None }
}

/// What the command line says of keeping or unpinning a file changed here is
/// what everybody says of freeing it up.
const MODIFIED_FREE: &str =
    "“{file}” was changed here and has not been uploaded, so freeing its space would lose your edits. It was left exactly as it is.";

const NOT_MANAGED_OPEN: &str = "“{file}” is not a OneDrive file: it is a file of your own in the sync folder, so it has no page in OneDrive.";

const OUTSIDE: Sentence = Each {
    desktop: "“{file}” is not inside any of KOneDrive's folders, so nothing was done with it. Only files and folders inside them can be kept on this device or freed up.",
    command_line: "{file}: only files and folders inside the sync folder{folder} can be kept on this device or freed up — not symbolic links, or anything outside it.",
};

// The daemon refuses the whole call if any path it was given is pinned by a
// folder above, and its message names that path and the folder. The plugin
// shows the message as it is, with no file name in front: the failure may be
// any path of the call. The command line reads the message for the two paths
// and names the command to run, in its own table.
const NOT_ALLOWED_KEPT: Sentence = Desktop("Could not change what is kept on this device: {detail}");

/// Every name of [`Refusal`], in its order.
pub static REFUSALS: [RefusalText; 29] = [
    as_failed(Refusal::NotEmpty),
    as_failed(Refusal::Unsupported),
    every(Refusal::InUse, Same("“{file}” is open in another program, so its space cannot be freed right now. Close it there and try again.")),
    // The command line says how to register a folder, as for any of its commands.
    every(Refusal::NoRoot, Desktop("The folder holding “{file}” is no longer registered with KOneDrive, so nothing was done with it.")),
    RefusalText {
        refusal: Refusal::NoHelper,
        sentence: ByOperation {
            // The command line has no sentence for `sync pin` and `sync unpin`
            // without the helper: it says what it says of a registration.
            keep: Some(Desktop(
                "The konedrive helper is not connected, so nothing was changed. Keeping “{file}” on this device must first have the helper stop letting its opens through unchecked — a download that failed partway would otherwise leave it reading as zeros. Try again once the helper is back (“konedrivectl sync status” shows when it is).",
            )),
            unpin: Some(Desktop(
                "The konedrive helper is not connected, so nothing was changed. Unpinning “{file}” needs the helper too, the same as any other change under KOneDrive's watch. Try again once KOneDrive is connected to the helper again — it reconnects on its own.",
            )),
            // `sync free` takes several paths, and may have freed some.
            free_up: Some(Each {
                desktop: "The konedrive helper is not connected, so nothing was changed. Freeing up “{file}” must first have the helper take off any mark that lets the file's opens through unchecked, or the emptied file could read as zeros from then on. Try again once KOneDrive is connected to the helper again — it reconnects on its own.",
                command_line: "The konedrive helper is not connected, so nothing more was freed up. Freeing up a file must first have the helper take off any mark that lets the file's opens through unchecked, or the emptied file could read as zeros from then on; try again once the daemon is connected to the helper again — it reconnects on its own.",
            }),
            // WebUrl needs no helper.
            open_online: None,
        },
        without_detail: None,
    },
    RefusalText {
        refusal: Refusal::NotManaged,
        sentence: ByOperation {
            keep: Some(Same("“{file}” is not a OneDrive file: it is a file of your own in the sync folder, so there is nothing for KOneDrive to keep downloaded.")),
            unpin: Some(Same("“{file}” is not a OneDrive file: it is a file of your own in the sync folder, so it was never pinned.")),
            free_up: Some(Same(
                "“{file}” is not a OneDrive file: it is a file of your own in the sync folder, and KOneDrive never frees the space of a file it could not download again.",
            )),
            open_online: Some(Same(NOT_MANAGED_OPEN)),
        },
        without_detail: None,
    },
    every(Refusal::NotHydrated, Same("“{file}” is not downloaded, so there is no space to free — it already takes none.")),
    RefusalText {
        refusal: Refusal::ModifiedLocally,
        sentence: ByOperation {
            keep: Some(Each {
                desktop: "“{file}” was changed here and has not been uploaded, so downloading it again would overwrite your edits. It was left exactly as it is.",
                command_line: MODIFIED_FREE,
            }),
            unpin: Some(Each {
                desktop: "“{file}” was changed here and has not been uploaded, so it was left exactly as it is.",
                command_line: MODIFIED_FREE,
            }),
            free_up: Some(Same(MODIFIED_FREE)),
            open_online: None,
        },
        without_detail: None,
    },
    RefusalText {
        refusal: Refusal::OutsideRoot,
        sentence: ByOperation {
            keep: Some(OUTSIDE),
            unpin: Some(OUTSIDE),
            free_up: Some(OUTSIDE),
            open_online: Some(Each {
                desktop: "“{file}” is not inside any of KOneDrive's folders, so it has no page in OneDrive.",
                command_line: "{file}: only files and folders inside the sync folder{folder}, and the folder itself, have a page in OneDrive — not symbolic links, or anything outside it.",
            }),
        },
        without_detail: None,
    },
    as_failed(Refusal::AlreadyRegistered),
    RefusalText {
        refusal: Refusal::NotSignedIn,
        sentence: ByOperation {
            keep: None,
            unpin: None,
            free_up: None,
            open_online: Some(Each {
                desktop: "The account is not signed in, so OneDrive cannot be asked for the page of “{file}”. Sign in and try again.",
                command_line: "The account is not signed in, so OneDrive cannot be asked for the page of {file}. Sign in with `{prefix} login` and try again.",
            }),
        },
        without_detail: None,
    },
    every(
        Refusal::NoSource,
        Each {
            desktop: "KOneDrive does not know where to download “{file}” from yet. Run “konedrivectl sync populate-from” first; KOneDrive does not remember that directory across a restart, so run it again after one (files already there are left alone).",
            command_line: "KOneDrive does not know where to download {file} from yet. Run `{prefix} sync populate-from <dir>` first; the daemon does not remember that directory across a restart, so run it again after one (files already there are left alone).",
        },
    ),
    as_failed(Refusal::NoConflict),
    RefusalText {
        refusal: Refusal::NotAllowed,
        sentence: ByOperation {
            keep: Some(NOT_ALLOWED_KEPT),
            unpin: Some(NOT_ALLOWED_KEPT),
            free_up: Some(Desktop("Could not free up space: {detail}")),
            open_online: Some(NOT_ALLOWED_KEPT),
        },
        without_detail: None,
    },
    as_failed(Refusal::Overlaps),
    as_failed(Refusal::NoAccount),
    RefusalText {
        refusal: Refusal::NotUploaded,
        sentence: ByOperation {
            keep: None,
            unpin: None,
            free_up: Some(Each {
                desktop: "“{file}” is not uploaded yet, so freeing it up would lose the changes made here. It was left exactly as it is.",
                command_line: "{file} is not uploaded yet, so freeing up its space would lose the changes made here. It was left as it is; its space can be freed once it is uploaded (`{prefix} sync outbox`).",
            }),
            open_online: Some(Each {
                desktop: "“{file}” is not uploaded yet, so it has no page in OneDrive.",
                command_line: "{file} is not uploaded yet, so it has no page in OneDrive. It can be opened there once it is uploaded (`{prefix} sync outbox`).",
            }),
        },
        without_detail: None,
    },
    as_failed(Refusal::PendingUploads),
    // "Open in OneDrive" asks OneDrive each time. The daemon's message is the
    // cause (no network, a locked secret storage, an answer that cannot be read).
    RefusalText {
        refusal: Refusal::Unreachable,
        sentence: ByOperation::every(Some(Same("OneDrive could not be reached: {detail}"))),
        without_detail: Some("OneDrive could not be reached."),
    },
    as_failed(Refusal::NotUp),
    as_failed(Refusal::Failed),
    as_failed(Refusal::WritesNotAllowed),
    as_failed(Refusal::ModeNotGranted),
    as_failed(Refusal::InvalidArgs),
    as_failed(Refusal::BusFailed),
    as_failed(Refusal::UnknownObject),
    as_failed(Refusal::UnknownMethod),
    as_failed(Refusal::UnknownInterface),
    as_failed(Refusal::Internal),
];

/// A refusal with no sentence of its own, a name nobody knows, or an error
/// from the bus itself: the daemon's message is all there is, so it is kept
/// whole.
pub const FAILED: ByOperation<&str> = ByOperation {
    keep: "Keeping “{file}” on this device failed: {detail}",
    unpin: "Unpinning “{file}” failed: {detail}",
    free_up: "Freeing up “{file}” failed: {detail}",
    open_online: "Opening “{file}” in OneDrive failed: {detail}",
};

/// "…, so “file” was not {was not}.": how the plugin says a file was not
/// changed, in a sentence that is the same for every operation.
pub const WAS_NOT: ByOperation<&str> = ByOperation { keep: "kept on this device", unpin: "unpinned", free_up: "freed up", open_online: "opened in OneDrive" };

/// The plugin's alone: the call never reached a running daemon.
pub const NOT_RUNNING: &str = "KOneDrive is not running, so “{file}” was not {was not}. Start it with “systemctl --user start konedrived” and try again.";

/// The plugin's alone: the daemon left the bus while the call waited for it.
pub const STOPPED: ByOperation<&str> = ByOperation {
    keep: "KOneDrive stopped before it finished downloading everything of “{file}”. Its emblem shows whether it is fully downloaded yet; if it is not, start KOneDrive again (“systemctl --user start konedrived”) and try again.",
    unpin: "KOneDrive stopped before it finished unpinning “{file}”. Its emblem shows whether it is still pinned; if it is, start KOneDrive again (“systemctl --user start konedrived”) and try again.",
    free_up: "KOneDrive stopped before it finished freeing up “{file}”. Its emblem shows whether it still takes space; if it does, start KOneDrive again (“systemctl --user start konedrived”) and try again.",
    open_online: "KOneDrive stopped before it found the page of “{file}” in OneDrive. Start KOneDrive again (“systemctl --user start konedrived”) and try again.",
};

/// The plugin's alone: the file was not sent, since an earlier call for it
/// has no answer yet.
pub const ALREADY_WAITING: &str = "KOneDrive has not yet answered an earlier request for “{file}”, so it was not asked again.";

/// The plugin's alone: the file was not sent, since too many calls wait.
pub const TOO_MANY_WAITING: &str =
    "{count} requests to KOneDrive are already waiting for an answer, so “{file}” was not {was not}. Try again once some of them have finished.";

/// The entry of a refusal; `None` for a name this build does not know.
pub fn entry(refusal: &Refusal) -> Option<&'static RefusalText> {
    REFUSALS.iter().find(|entry| entry.refusal == *refusal)
}

/// What a sentence of the command line is filled with.
#[derive(Debug, Clone, Copy, Default)]
pub struct Told<'a> {
    /// The path the refusal is about.
    pub file: &'a str,
    /// The daemon's message; empty when it gave none.
    pub detail: &'a str,
    /// How a command suggested for this account begins.
    pub prefix: &'a str,
    /// ` (/the/folder)`, or nothing.
    pub folder: &'a str,
}

impl Told<'_> {
    fn fill(&self, sentence: &str) -> String {
        fill(sentence, &[("file", self.file), ("detail", self.detail), ("prefix", self.prefix), ("folder", self.folder)])
    }
}

/// The sentence of `refusal` for `operation`, as `client` says it; `None`
/// where the catalogue has none for this client.
pub fn text(operation: Operation, refusal: &Refusal, client: Client, told: &Told<'_>) -> Option<String> {
    let entry = entry(refusal)?;
    let sentence = entry.sentence.of(operation)?.of(client)?;
    match entry.without_detail {
        Some(bare) if told.detail.is_empty() => Some(bare.to_owned()),
        _ => Some(told.fill(sentence)),
    }
}

/// [`FAILED`], filled in.
pub fn failed(operation: Operation, told: &Told<'_>) -> String {
    told.fill(FAILED.of(operation))
}

#[cfg(test)]
mod tests;
