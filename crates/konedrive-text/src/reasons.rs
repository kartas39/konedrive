//! Why a change is not uploaded: the sentence of every key of `Reason` and
//! `LocalSkip`, which share one table (`not-downloaded` and
//! `state-unreadable` are in both, with one meaning).
//!
//! The places of a sentence: `{key}` (the key itself), `{detail}` (what
//! stands behind `<key>: `), and `{needs}` and `{free}` (the sizes of
//! `too-big:<needs>:<free>`, as the client writes sizes).

use konedrive_reason::key_of;

use crate::Sentence::{Each, Same};
use crate::{fill, Client, Sentence};

/// The words of one key.
#[derive(Debug, Clone, Copy)]
pub struct ReasonText {
    /// The key, as `konedrive-reason` spells it.
    pub key: &'static str,
    /// The sentence of the key alone. `None`: it has no sentence, and the
    /// clients show the reason as the daemon wrote it.
    pub bare: Option<Sentence>,
    /// The sentence of the key with something behind it. `None`: such a
    /// reason is shown as the daemon wrote it.
    pub detailed: Option<Sentence>,
}

const fn bare(key: &'static str, sentence: &'static str) -> ReasonText {
    ReasonText { key, bare: Some(Same(sentence)), detailed: None }
}

const fn detailed(key: &'static str, sentence: &'static str, with_detail: &'static str) -> ReasonText {
    ReasonText { key, bare: Some(Same(sentence)), detailed: Some(Same(with_detail)) }
}

/// A key with no sentence yet: shown as stored, with whatever stands behind it.
const fn as_stored(key: &'static str) -> ReasonText {
    ReasonText { key, bare: None, detailed: None }
}

const KEEPS_CHANGING: &str = "It keeps changing in OneDrive: tried again later.";
const INCOMPLETE: &str = "KOneDrive's record of this change is incomplete ({key}): it stays here until the file is changed again.";
const NOT_A_FILE: &str = "Not a file or a folder: never uploaded.";

/// Every key of `Reason`, in its order, then those of `LocalSkip` that are
/// not a row's reasons too.
pub const REASONS: &[ReasonText] = &[
    bare("open-for-writing", "Open for writing in another program: it goes up once closed."),
    ReasonText {
        key: "mass-delete",
        bare: Some(Each {
            desktop: "Part of a large delete: delete it in OneDrive too, or restore it, on the Status page.",
            command_line: "Part of a large delete: confirm it (`sync deletes confirm`) or undo it (`sync deletes restore`).",
        }),
        detailed: None,
    },
    bare("name-characters", "A name OneDrive refuses (it holds one of \" * : < > ? \\ |): rename it to upload it."),
    bare("name-spaces", "A name that starts or ends with a space, which OneDrive refuses: rename it to upload it."),
    bare("name-reserved", "A name OneDrive reserves: rename it to upload it."),
    bare("name-not-utf8", "A name that is not valid UTF-8: rename it to upload it."),
    bare("too-large", "Larger than OneDrive takes (250 GB)."),
    bare("quota-exceeded", "OneDrive is full: free some space in OneDrive."),
    bare("forbidden", "This sign-in does not allow uploads: sign in again."),
    detailed("refused", "Refused by OneDrive.", "OneDrive refused it: {detail}"),
    bare("locked", "Locked in OneDrive (open for co-authoring): tried again later."),
    as_stored("not-found"),
    // A row's reason and a skip's: the same sentence.
    bare("not-downloaded", "A file from another OneDrive folder that is not downloaded here."),
    as_stored("changed-while-sending"),
    as_stored("parent-not-in-onedrive"),
    as_stored("hash-mismatch"),
    as_stored("move-out-not-yet"),
    as_stored("waiting-for-the-helper"),
    as_stored("moved-out-unreachable"),
    as_stored("back-in-the-folder"),
    as_stored("moved-out-place-unknown"),
    detailed(
        "moved-out-not-opened",
        "Moved out of the folder before it was downloaded, and it cannot be opened for the download now: tried again later.",
        "Moved out of the folder before it was downloaded, and it cannot be opened for the download now: tried again later ({detail}).",
    ),
    as_stored("download-failed"),
    as_stored("gone-once"),
    as_stored("handle-from-another-filesystem"),
    as_stored("gone-unproved"),
    as_stored("lease-probe-failed"),
    bare("paused", "Paused with the account: it goes on when the pause ends."),
    bare("upload-session-open", "Its name in OneDrive is held by an upload of this folder that has not ended: tried again later."),
    bare("name-held-by-an-upload", "Its name in OneDrive is held by an unfinished upload (another device, or one abandoned): tried again later."),
    bare("changed in OneDrive again and again", KEEPS_CHANGING),
    bare("changing in OneDrive again and again", KEEPS_CHANGING),
    bare("the upload session ended twice", "OneDrive ended the upload twice: tried again later."),
    detailed("not allowed now", "Uploads are not allowed now: it goes on when they are.", "Uploads are not allowed now: it goes on when they are ({detail})."),
    // A row's reason and a skip's: the same sentence.
    detailed(
        "state-unreadable",
        "The file's KOneDrive state cannot be read: it stays here until the file is replaced.",
        "The file's KOneDrive state cannot be read: it stays here until the file is replaced ({detail}).",
    ),
    bare("no-name", INCOMPLETE),
    bare("no-item", INCOMPLETE),
    bare("no-guard", INCOMPLETE),
    bare("no-handle", INCOMPLETE),
    bare("bad-handle", INCOMPLETE),
    bare("another-item", INCOMPLETE),
    bare("blocked", INCOMPLETE),
    bare("network", "OneDrive could not be reached: tried again later."),
    bare("local-error", "The local file could not be read: tried again later."),
    bare("index-error", "KOneDrive's local index failed: tried again later."),
    bare("upload-error", "The upload failed: tried again later."),
    ReasonText {
        key: "waiting-for-space",
        // The window says what to do, with its button; the command line only what is waited for.
        bare: Some(Each { desktop: "OneDrive is full: free up space in OneDrive, then Refresh.", command_line: "Waiting for space: OneDrive is full." }),
        detailed: None,
    },
    ReasonText {
        key: "too-big",
        bare: Some(Each {
            desktop: "Too big for the space left in OneDrive: free up space there, then Refresh.",
            command_line: "Too big for the space left in OneDrive: free up space there, then `sync refresh`.",
        }),
        detailed: Some(Same("Too big: needs {needs}, {free} free.")),
    },
    bare("symlink", "A symbolic link: never uploaded."),
    bare("fifo", NOT_A_FILE),
    bare("socket", NOT_A_FILE),
    bare("device", NOT_A_FILE),
    bare("reserved-name", "A .konedrive- name, which KOneDrive keeps for itself: never uploaded."),
    bare("hard-link", "A file with other hard links: not uploaded."),
    bare("other-device", "On another filesystem mounted inside the folder: never uploaded."),
    bare("unreadable", "Cannot be read: not uploaded, nor anything inside it, until KOneDrive may read it."),
    as_stored("ignored"),
];

/// The entry of a key.
pub fn entry(key: &str) -> Option<&'static ReasonText> {
    REASONS.iter().find(|entry| entry.key == key)
}

/// The key a stored reason is listed under (`konedrive-reason`'s [`key_of`])
/// and what stands behind it: after `<key>: `, or the sizes after `too-big:`.
pub fn split(stored: &str) -> (&str, Option<&str>) {
    let key = key_of(stored);
    let detail = stored.strip_prefix(key).filter(|rest| !rest.is_empty()).and_then(|rest| rest.strip_prefix(": ").or_else(|| rest.strip_prefix(':')));
    (key, detail)
}

/// `(needs, free)` of the detail of a `too-big` reason, in bytes.
pub fn sizes(detail: &str) -> Option<(u64, u64)> {
    let (needs, free) = detail.split_once(':')?;
    Some((needs.parse().ok()?, free.parse().ok()?))
}

/// Whether a sentence has a place for sizes, which only `too-big:<needs>:<free>` fills.
pub fn takes_sizes(sentence: &str) -> bool {
    sentence.contains("{needs}") || sentence.contains("{free}")
}

/// What a reason as the daemon sent it — a row's, an `upload-failed`
/// event's, a `NotUploaded()` one, or the key a summary lists it under —
/// means to a person, as `client` says it; `size` writes a number of bytes
/// as the client does. A reason with no sentence is given back as it came.
pub fn text(stored: &str, client: Client, size: &dyn Fn(u64) -> String) -> String {
    let (key, detail) = split(stored);
    let Some(entry) = entry(key) else { return stored.to_owned() };
    let sentence = match detail {
        None => entry.bare,
        Some(_) => entry.detailed,
    };
    let Some(sentence) = sentence.and_then(|sentence| sentence.of(client)) else { return stored.to_owned() };
    let detail = detail.unwrap_or_default();
    if takes_sizes(sentence) {
        return match sizes(detail) {
            Some((needs, free)) => fill(sentence, &[("needs", &size(needs)), ("free", &size(free))]),
            None => stored.to_owned(),
        };
    }
    fill(sentence, &[("key", key), ("detail", detail)])
}
