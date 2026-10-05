use konedrive_dbus::rows::{Change, KeptBack, KeptBackFiles, KeptBackReason};
use konedrive_reason::Group;
use konedrive_text::Client;

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
/// reason as stored, or the key a summary lists it under. The sentence is
/// the catalogue's (`konedrive-text`), as it is written there; a reason with
/// no sentence is shown as it is.
pub fn upload_reason_text(reason: &str) -> String {
    konedrive_text::reasons::text(reason, Client::CommandLine, &human_bytes)
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
        // The sentence is whole, with its full stop: no colon behind it.
        out.push_str(&format!("\n{}\n", upload_reason_text(reason)));
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
