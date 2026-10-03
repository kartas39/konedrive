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

/// What a row's reason, or a `NotUploaded()` reason, means to a person.
pub fn upload_reason_text(reason: &str) -> String {
    let rename = "rename it to upload it";
    match reason {
        "name-characters" => format!("a name OneDrive refuses (one of \" * : < > ? \\ |): {rename}"),
        "name-spaces" => format!("a name that starts or ends with a space, which OneDrive refuses: {rename}"),
        "name-reserved" => format!("a name OneDrive reserves: {rename}"),
        "name-not-utf8" => format!("a name that is not valid UTF-8: {rename}"),
        "too-large" => "larger than OneDrive takes (250 GB)".to_owned(),
        "quota-exceeded" => "OneDrive is full: free some space in OneDrive".to_owned(),
        "waiting-for-space" => "waiting for space: OneDrive is full".to_owned(),
        "forbidden" => "this sign-in does not allow uploads: sign in again".to_owned(),
        "open-for-writing" => "open for writing in another program: it goes up once closed".to_owned(),
        "mass-delete" => "part of a large delete: confirm it (`sync deletes confirm`) or undo it (`sync deletes restore`)".to_owned(),
        "symlink" => "a symbolic link: never uploaded".to_owned(),
        "fifo" | "socket" | "device" => "not a file or a folder: never uploaded".to_owned(),
        "reserved-name" => "a .konedrive- name, which the daemon keeps for itself: never uploaded".to_owned(),
        "not-downloaded" => "a file from another OneDrive folder that is not downloaded here".to_owned(),
        "other-device" => "on another filesystem mounted inside the folder: never uploaded".to_owned(),
        "mounted-inside" => "another filesystem is mounted inside a folder no longer synced here: the folder stays until it is unmounted".to_owned(),
        "leaving-not-found" => "not found in OneDrive, which still lists it, in a folder no longer synced here: kept until OneDrive's listing says it was removed, or it is changed again".to_owned(),
        "unknown-state" => "a file whose konedrive state cannot be read, in a folder no longer synced here: the folder stays until it is fixed or removed".to_owned(),
        "hard-link" => "a file with other hard links: not uploaded".to_owned(),
        "locked" => "locked in OneDrive (open for co-authoring): tried again later".to_owned(),
        "network" => "OneDrive could not be reached: tried again later".to_owned(),
        "local-error" => "the local file could not be read: tried again later".to_owned(),
        "index-error" => "konedrive's local index failed: tried again later".to_owned(),
        "upload-error" => "the upload failed: tried again later".to_owned(),
        "refused" => "refused by OneDrive".to_owned(),
        "paused" => "paused with the account: it goes on when the pause ends".to_owned(),
        "upload-session-open" => "its name in OneDrive is held by an upload of this folder that has not ended: tried again later".to_owned(),
        "name-held-by-an-upload" => "its name in OneDrive is held by an unfinished upload (another device, or one abandoned): tried again later".to_owned(),
        "changed in OneDrive again and again" | "changing in OneDrive again and again" => "it keeps changing in OneDrive: tried again later".to_owned(),
        "the upload session ended twice" => "OneDrive ended the upload twice: tried again later".to_owned(),
        "not allowed now" => "uploads are not allowed now: it goes on when they are".to_owned(),
        "state-unreadable" => "the file's konedrive state cannot be read: it stays here until the file is replaced".to_owned(),
        "no-name" | "no-item" | "no-guard" | "no-handle" | "bad-handle" | "another-item" | "blocked" => {
            format!("konedrive's record of this change is incomplete ({reason}): it stays here until the file is changed again")
        }
        "too-big" => "too big for the space left in OneDrive: free up space there, then `sync refresh`".to_owned(),
        other => match (other.strip_prefix("refused: "), too_big(other)) {
            (Some(message), _) => format!("OneDrive refused it: {message}"),
            (None, Some((needs, free))) => format!("too big: needs {}, {} free", human_bytes(needs), human_bytes(free)),
            (None, None) => match other.split_once(": ") {
                // `<key>: <detail>`: the key's sentence, then what the daemon said.
                Some((key, detail)) if DETAILED.contains(&key) => format!("{} ({detail})", upload_reason_text(key)),
                _ => other.to_owned(),
            },
        },
    }
}

/// The keys that come with a detail behind them, `<key>: <detail>`, beside `refused`.
const DETAILED: [&str; 2] = ["not allowed now", "state-unreadable"];

/// `too-big:<needs>:<free>`: a file too big for the space left in OneDrive.
fn too_big(reason: &str) -> Option<(u64, u64)> {
    let (needs, free) = reason.strip_prefix("too-big:")?.split_once(':')?;
    Some((needs.parse().ok()?, free.parse().ok()?))
}

/// One row of `UploadQueue.Changes()`: (seq, kind, full path, state, bytes sent, bytes
/// in all, reason, next try).
pub type OutboxRow = (u64, String, String, String, u64, u64, String, i64);

/// `sync outbox`: one line per change waiting to go up — its state, kind and
/// path, how far an upload has got, and why it waits.
pub fn outbox_text(rows: &[OutboxRow], more: bool, prefix: &str) -> String {
    if rows.is_empty() {
        return "Nothing is waiting to upload.\n".to_owned();
    }
    let mut out = String::new();
    for (_, kind, path, state, done, total, reason, next_try) in rows {
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
/// the window's per-file cap, the same guess (issue #20).
pub const PER_FILE_SHOWN: u32 = 20;

/// A `NotUploadedSummary()` group's heading.
fn kept_back_group_text(group: &str) -> &str {
    match group {
        "one-action" => "Needs you: one action fixes them all",
        "per-file" => "Needs you: each file",
        "never" => "Never uploaded",
        "waiting" => "Waiting: these go up by themselves",
        other => other,
    }
}

/// One reason's files as `sync not-uploaded` lists them: (reason, (path,
/// reason as stored) of the files asked for, how many there are in all).
pub type ReasonFiles = (String, Vec<(String, String)>, u32);

/// `sync not-uploaded`: what stays on this computer, and why — each group,
/// its reasons with their counts, then `files` for the reasons whose files
/// were asked for.
pub fn not_uploaded_text(summary: &[(String, String, u32, u64)], files: &[ReasonFiles], prefix: &str) -> String {
    if summary.is_empty() {
        return "Everything here is uploaded or waits to be.\n".to_owned();
    }
    let mut out = String::new();
    let mut group_shown: Option<&str> = None;
    for (group, reason, count, bytes) in summary {
        if group_shown != Some(group.as_str()) {
            out.push_str(&format!("{}:\n", kept_back_group_text(group)));
            group_shown = Some(group);
        }
        let size = if *bytes > 0 { format!(", {}", human_bytes(*bytes)) } else { String::new() };
        out.push_str(&format!("  {count}{size}: {}\n", upload_reason_text(reason)));
    }
    for (reason, items, total) in files {
        out.push_str(&format!("\n{}:\n", upload_reason_text(reason)));
        for (path, why) in items {
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
