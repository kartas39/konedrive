use konedrive_dbus::rows::{Change, KeptBack, KeptBackFiles, KeptBackReason};
use konedrive_reason::{Group, LocalSkip, Reason};

use super::formats::{human_bytes, local_time};

/// `sync status`'s `Waiting to upload:` line: `3 files (1.5 MiB)`.
pub fn waiting_text(count: u32, bytes: u64) -> String {
    match count {
        0 => "nothing".to_owned(),
        1 => format!("1 change ({})", human_bytes(bytes)),
        n => format!("{n} changes ({})", human_bytes(bytes)),
    }
}

/// `sync status`'s `Waiting for space:` line, while OneDrive is full:
/// `2029 files (42.0 GiB) — OneDrive is full`.
pub fn space_waiting_text(count: u32, bytes: u64) -> String {
    let files = if count == 1 { "1 file".to_owned() } else { format!("{count} files") };
    format!("{files} ({}) — OneDrive is full", human_bytes(bytes))
}

/// What `sync refresh` says of the quota it read: the free space and
/// Graph's state, or that OneDrive is still full. Empty when none was read.
pub fn quota_text(state: &str, free: u64, full: bool) -> String {
    if full {
        return format!("OneDrive is full ({} free): free up space in OneDrive, then refresh again.\n", human_bytes(free));
    }
    if state.is_empty() {
        return String::new();
    }
    format!("OneDrive: {} free (quota {state}).\n", human_bytes(free))
}

/// What a row's reason, or a `NotUploaded()` reason, means to a person: a
/// reason as stored, or the key a summary lists it under. One the daemon's
/// tables do not know is shown as it is.
pub fn upload_reason_text(reason: &str) -> String {
    match Reason::parse(reason) {
        // What an examination never uploads has its own table; `not-downloaded` and
        // `state-unreadable` are in both.
        Reason::Other(_) => skip_text(&LocalSkip::parse(reason), reason),
        known => reason_text(&known, reason),
    }
}

/// The sentence of a row's reason; `stored` is how the daemon sent it, shown
/// as it is where the reason has no sentence.
fn reason_text(reason: &Reason, stored: &str) -> String {
    let rename = "rename it to upload it";
    let incomplete = || format!("konedrive's record of this change is incomplete ({stored}): it stays here until the file is changed again");
    // `<key>: <detail>`: the key's sentence, then what the daemon said.
    let detailed = |sentence: &str, detail: &Option<String>| match detail {
        Some(detail) => format!("{sentence} ({detail})"),
        None => sentence.to_owned(),
    };
    match reason {
        Reason::NameCharacters => format!("a name OneDrive refuses (one of \" * : < > ? \\ |): {rename}"),
        Reason::NameSpaces => format!("a name that starts or ends with a space, which OneDrive refuses: {rename}"),
        Reason::NameReserved => format!("a name OneDrive reserves: {rename}"),
        Reason::NameNotUtf8 => format!("a name that is not valid UTF-8: {rename}"),
        Reason::TooLarge => "larger than OneDrive takes (250 GB)".to_owned(),
        Reason::Quota => "OneDrive is full: free some space in OneDrive".to_owned(),
        Reason::WaitingForSpace => "waiting for space: OneDrive is full".to_owned(),
        Reason::Forbidden => "this sign-in does not allow uploads: sign in again".to_owned(),
        Reason::OpenForWriting => "open for writing in another program: it goes up once closed".to_owned(),
        Reason::MassDelete => "part of a large delete: confirm it (`sync deletes confirm`) or undo it (`sync deletes restore`)".to_owned(),
        Reason::NotLocal => "a file from another OneDrive folder that is not downloaded here".to_owned(),
        Reason::Locked => "locked in OneDrive (open for co-authoring): tried again later".to_owned(),
        Reason::Network => "OneDrive could not be reached: tried again later".to_owned(),
        Reason::LocalIo => "the local file could not be read: tried again later".to_owned(),
        Reason::Store => "konedrive's local index failed: tried again later".to_owned(),
        Reason::Failed => "the upload failed: tried again later".to_owned(),
        Reason::Refused(None) => "refused by OneDrive".to_owned(),
        Reason::Refused(Some(message)) => format!("OneDrive refused it: {message}"),
        Reason::NotOpened(detail) => detailed(
            "moved out of the folder before it was downloaded, and it cannot be opened for the download now: tried again later",
            detail,
        ),
        Reason::Paused => "paused with the account: it goes on when the pause ends".to_owned(),
        Reason::SessionOpen => "its name in OneDrive is held by an upload of this folder that has not ended: tried again later".to_owned(),
        Reason::NameHeld => "its name in OneDrive is held by an unfinished upload (another device, or one abandoned): tried again later".to_owned(),
        Reason::ChangedAgain | Reason::ChangingAgain => "it keeps changing in OneDrive: tried again later".to_owned(),
        Reason::SessionEnded => "OneDrive ended the upload twice: tried again later".to_owned(),
        Reason::NotAllowed(detail) => detailed("uploads are not allowed now: it goes on when they are", detail),
        Reason::BadState(detail) => detailed("the file's konedrive state cannot be read: it stays here until the file is replaced", detail),
        Reason::NoName | Reason::NoItem | Reason::NoGuard | Reason::NoHandle | Reason::BadHandle | Reason::AnotherItem | Reason::Blocked => incomplete(),
        Reason::TooBig(None) => "too big for the space left in OneDrive: free up space there, then `sync refresh`".to_owned(),
        Reason::TooBig(Some((needs, free))) => too_big_text(*needs, *free),
        // No sentence yet: shown as stored, with whatever stands behind the key.
        Reason::NotFound
        | Reason::Changed
        | Reason::Parent
        | Reason::Hash
        | Reason::MoveOut
        | Reason::NoHelper
        | Reason::Unreachable(_)
        | Reason::BackInside
        | Reason::PlaceUnknown
        | Reason::Download(_)
        | Reason::GoneOnce
        | Reason::StaleHandle
        | Reason::GoneUnproved
        | Reason::NoLease(_) => stored.to_owned(),
        Reason::Other(_) => match reason.sizes() {
            Some((needs, free)) => too_big_text(needs, free),
            None => stored.to_owned(),
        },
    }
}

/// The sentence of what an examination never uploads.
fn skip_text(skip: &LocalSkip, stored: &str) -> String {
    match skip {
        LocalSkip::Symlink => "a symbolic link: never uploaded".to_owned(),
        LocalSkip::Fifo | LocalSkip::Socket | LocalSkip::Device => "not a file or a folder: never uploaded".to_owned(),
        LocalSkip::ReservedName => "a .konedrive- name, which the daemon keeps for itself: never uploaded".to_owned(),
        // Spelled as a row's `not-downloaded`, which is read first.
        LocalSkip::NotDownloaded => reason_text(&Reason::NotLocal, stored),
        LocalSkip::OtherDevice => "on another filesystem mounted inside the folder: never uploaded".to_owned(),
        LocalSkip::HardLink => "a file with other hard links: not uploaded".to_owned(),
        LocalSkip::Unreadable => "cannot be read: not uploaded, nor anything inside it, until konedrive may read it".to_owned(),
        // Spelled as a row's `state-unreadable`, which is read first.
        LocalSkip::BadState => reason_text(&Reason::BadState(None), stored),
        // No sentence: shown as stored.
        LocalSkip::Ignored => stored.to_owned(),
        LocalSkip::Other(_) => reason_text(&Reason::Other(stored.to_owned()), stored),
    }
}

/// A file too big for the space left in OneDrive.
fn too_big_text(needs: u64, free: u64) -> String {
    format!("too big: needs {}, {} free", human_bytes(needs), human_bytes(free))
}

/// `sync outbox`: one line per change waiting to go up — its state, kind and
/// path, how far an upload has got, and why it waits.
pub fn outbox_text(rows: &[Change], more: bool, prefix: &str) -> String {
    if rows.is_empty() {
        return "Nothing is waiting to upload.\n".to_owned();
    }
    let mut out = String::new();
    for Change { kind, path, state, sent: done, total, reason, next_try, .. } in rows {
        out.push_str(&format!("{state:<8} {kind:<8} {path}"));
        if state == "running" && *total > 0 {
            let percent = done.saturating_mul(100) / total;
            out.push_str(&format!("  {percent}% of {}", human_bytes(*total)));
        }
        if !reason.is_empty() {
            out.push_str(&format!("  ({})", upload_reason_text(reason)));
        }
        if state == "retry" && *next_try > 0 {
            out.push_str(&format!("  next try {}", local_time(*next_try)));
        }
        out.push('\n');
    }
    if more {
        out.push_str(&format!("… and more: `{prefix} sync outbox --all` shows them all\n"));
    }
    out
}

/// How many files of a reason `sync not-uploaded` lists without `--all`:
/// the window's per-file cap, the same guess.
pub const PER_FILE_SHOWN: u32 = 20;

/// A `NotUploadedSummary()` group's heading.
fn kept_back_group_text(group: &str) -> &str {
    match Group::parse(group) {
        Some(Group::OneAction) => "Needs you: one action fixes them all",
        Some(Group::PerFile) => "Needs you: each file",
        Some(Group::Never) => "Never uploaded",
        Some(Group::Waiting) => "Waiting: these go up by themselves",
        None => group,
    }
}

/// One reason's files as `sync not-uploaded` lists them: the reason as the summary names it,
/// and its files as they were asked for.
pub type ReasonFiles = (String, KeptBackFiles);

/// `sync not-uploaded`: what stays on this computer, and why — each group,
/// its reasons with their counts, then `files` for the reasons whose files
/// were asked for.
pub fn not_uploaded_text(summary: &[KeptBackReason], files: &[ReasonFiles], prefix: &str) -> String {
    if summary.is_empty() {
        return "Everything here is uploaded or waits to be.\n".to_owned();
    }
    let mut out = String::new();
    let mut group_shown: Option<&str> = None;
    for KeptBackReason { group, reason, count, bytes } in summary {
        if group_shown != Some(group.as_str()) {
            out.push_str(&format!("{}:\n", kept_back_group_text(group)));
            group_shown = Some(group);
        }
        let size = if *bytes > 0 { format!(", {}", human_bytes(*bytes)) } else { String::new() };
        out.push_str(&format!("  {count}{size}: {}\n", upload_reason_text(reason)));
    }
    for (reason, KeptBackFiles { items, total }) in files {
        out.push_str(&format!("\n{}:\n", upload_reason_text(reason)));
        for KeptBack { path, reason: why } in items {
            out.push_str(&format!("  {path}"));
            if why != reason {
                out.push_str(&format!("  ({})", upload_reason_text(why)));
            }
            out.push('\n');
        }
        let more = (*total as usize).saturating_sub(items.len());
        if more > 0 {
            out.push_str(&format!("  … and {more} more: `{prefix} sync not-uploaded --all` lists them all\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests;
