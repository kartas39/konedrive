//! `status` and `sync status`: what was read of an account and of its folder, and how it is
//! printed.

use konedrive_dbus::HelperState;

use super::formats::{checked_text, grouped, human_bytes, local_time, seconds_text};
use super::transfers::waiting_download_text;
use super::uploads::{space_waiting_text, waiting_text};

/// `status`'s `Client ID:` line: `Accounts.ClientId`, one for every account.
pub fn client_id_line(client_id: &str) -> String {
    let shown = if client_id.is_empty() { "(not set)" } else { client_id };
    format!("{:<12}{shown}\n", "Client ID:")
}

/// `status`'s `Problem:` line: trouble that belongs to no account (`Accounts.LastError`);
/// nothing when there is none.
pub fn problem_line(trouble: &str) -> String {
    if trouble.is_empty() {
        String::new()
    } else {
        format!("{:<12}{trouble}\n", "Problem:")
    }
}

/// `status`'s `Accounts:` line when there is no account yet.
pub fn no_account_line() -> String {
    format!("{:<12}none yet: `konedrivectl login` adds one called {} and signs it in\n", "Accounts:", crate::FIRST_LABEL)
}

/// One account's block under its label, in `status` and `sync status` when they show several.
pub fn account_block(label: &str, block: &str) -> String {
    format!("\n{label}\n{}", super::formats::indented(block))
}

/// One account as `Account`'s properties say it: what `status` shows. A value is `None`
/// when the daemon has no such property (an older build, not restarted): its line is left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountStatus {
    pub label: Option<String>,
    /// `signed-out`, `signing-in` or `signed-in`.
    pub state: Option<String>,
    pub mode: Option<String>,
    pub display_name: Option<String>,
    pub email: String,
    /// Bytes used and in all.
    pub quota: Option<(u64, u64)>,
    pub last_error: String,
}

/// `status` for one account. `client_id` is `Some` when this account is all `status` shows:
/// its label and the client ID are printed with it. When `status` shows several, the client
/// ID is printed once above them and each block is headed by its label: `None`.
pub fn status_text(status: &AccountStatus, client_id: Option<&str>) -> String {
    let mut out = String::new();
    let mut line = |label: &str, value: &str| out.push_str(&format!("{label:<12}{value}\n"));
    if let (Some(_), Some(label)) = (client_id, &status.label) {
        line("Label:", label);
    }
    if let Some(state) = &status.state {
        line("State:", state);
    }
    if let Some(client_id) = client_id {
        line("Client ID:", if client_id.is_empty() { "(not set)" } else { client_id });
    }
    if let Some(mode) = &status.mode {
        line("Mode:", mode);
    }
    if status.state.as_deref() == Some("signed-in") {
        if let Some(name) = &status.display_name {
            line("Account:", &format!("{name} <{}>", status.email));
        }
        if let Some((used, total)) = status.quota {
            line("Storage:", &format!("{} of {} used", human_bytes(used), human_bytes(total)));
        }
    }
    if !status.last_error.is_empty() {
        line("Last error:", &status.last_error);
    }
    out
}

/// `account mode` with no mode given: the mode, and `Account.LastError` when it says
/// something. `tag` is what a success line starts with.
pub fn mode_shown_text(tag: &str, mode: &str, last_error: &str) -> String {
    let mut out = format!("{tag}{mode}\n");
    if !last_error.is_empty() {
        out.push_str(&format!("{:<12}{last_error}\n", "Last error:"));
    }
    out
}

/// One account's folder as its interfaces' properties say it: what `sync status` shows. A
/// value is `None` when the daemon has no such property (an older build, not restarted): its
/// line is left out. The values whose line is printed only when they say something are read
/// as nothing then.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderStatus {
    /// `Folder.Path`: empty with no folder.
    pub path: String,
    /// `Folder.State`.
    pub state: Option<String>,
    pub last_error: String,
    /// `Folder.Source`: `onedrive`, `local`, or empty.
    pub source: String,
    /// Items in OneDrive, and items in the folder.
    pub items: Option<(u64, u64)>,
    pub skipped: u64,
    /// Unix seconds of the last check with OneDrive; 0 for never.
    pub last_checked: Option<i64>,
    /// `Folder.LiveChanges`.
    pub live_changes: String,
    /// `Account.Mode` of the folder's account.
    pub mode: Option<String>,
    /// `Folder.Writable`: whether what is changed in the folder is uploaded now.
    pub writable: Option<bool>,
    /// Files left to download, and their size.
    pub download_left: Option<(u32, u64)>,
    pub scan: Option<LocalScan>,
    /// Changes waiting to be uploaded, and the size of what they send.
    pub pending: Option<(u32, u64)>,
    /// Changes that need the user.
    pub blocked: u32,
    /// OneDrive is full; then the changes that wait for space, and their size.
    pub quota_full: bool,
    pub quota_waiting: u32,
    pub quota_waiting_bytes: u64,
    /// Files refused as too big for the space left.
    pub too_big: u32,
    /// Removals held for confirmation.
    pub held_deletes: u32,
    pub paused: bool,
    /// Unix seconds when the pause ends by itself; 0 until resumed.
    pub paused_until: i64,
    /// `Folder.HeldBack`: why the account holds back by itself, or empty.
    pub held_back: String,
    /// What the folder's files take on this disk.
    pub local_bytes: Option<u64>,
    /// Files and folders with a pin of their own.
    pub pinned: Option<u32>,
    pub conflicts: u32,
}

/// `sync status` for one account's folder, as it was read at `now` (unix seconds).
///
/// `Folder.State` is `none` on an ordinary machine that has never registered a folder: that
/// prints as an unremarkable "(none)", not an error. `error` means a folder is registered
/// and something needs attention; `LastError` then carries the detail and is always printed
/// with it.
///
/// A registered folder gets an `Opens:` line, saying whether anything fills a file when it is
/// opened. That matters most for `no-interception`: without the helper, a file that is not
/// downloaded reads as zeros, and that is on screen every time.
///
/// `helper` is `Accounts.HelperState` when this folder is all `sync status` shows: the
/// `Helper:` line says how the helper stands and, when it is not connected, what to do. One
/// helper serves every account, so when `sync status` shows several, it prints that line
/// once above them and each block is printed with `None`.
///
/// The lines about OneDrive are printed for a folder that shows OneDrive; `On this
/// computer:` and `Always on this device:` for a folder that is up; `Conflicts:` when there
/// are any. `prefix` is how a suggested command starts.
pub fn sync_status_text(status: &FolderStatus, helper: Option<&str>, prefix: &str, now: i64) -> String {
    const W: usize = SYNC_STATUS_WIDTH;
    let mut out = String::new();
    let mut line = |label: &str, value: &str| out.push_str(&format!("{label:<W$}{value}\n"));
    let state = status.state.as_deref().unwrap_or_default();
    line("Folder:", if status.path.is_empty() { "(none)" } else { &status.path });
    if status.state.is_some() {
        line("State:", state);
    }
    if let Some(opens) = opens_line(state) {
        line("Opens:", opens);
    }
    if let Some(helper) = helper {
        line("Helper:", &helper_text(helper));
    }
    if !status.last_error.is_empty() {
        line("Last error:", &status.last_error);
    }
    if status.source == "onedrive" {
        if let Some((listed, placed)) = status.items {
            line("Items:", &format!("{listed} in OneDrive, {placed} in the folder"));
        }
        if status.skipped > 0 {
            line("Skipped:", &format!("{} (see `{prefix} sync skipped`)", status.skipped));
        }
        if let Some(last_checked) = status.last_checked {
            line("Last checked:", &checked_text(last_checked, now));
        }
        if let Some(live) = live_text(&status.live_changes) {
            line("Changes from OneDrive:", live);
        }
        if let Some(mode) = &status.mode {
            line("Mode:", &mode_text(mode, status.writable));
        }
        if let Some((count, bytes)) = status.download_left {
            line("Waiting to download:", &waiting_download_text(count, bytes));
        }
        if let Some(scan) = &status.scan {
            line("Local scan:", &local_scan_text(scan, now));
        }
        if let Some((pending, bytes)) = status.pending {
            if status.mode.as_deref() == Some("read-write") || pending > 0 || status.blocked > 0 {
                line("Waiting to upload:", &waiting_text(pending, bytes));
            }
        }
        if status.blocked > 0 {
            line("Blocked:", &format!("{} (see `{prefix} sync not-uploaded`)", status.blocked));
        }
        if status.quota_full {
            line("Waiting for space:", &space_waiting_text(status.quota_waiting, status.quota_waiting_bytes));
        }
        if status.too_big > 0 {
            line("Too big for the space:", &format!("{} (see `{prefix} sync outbox`)", status.too_big));
        }
        if status.held_deletes > 0 {
            line(
                "Held for confirmation:",
                &format!(
                    "{} deletions (`{prefix} sync deletes confirm` or `{prefix} sync deletes restore`)",
                    status.held_deletes
                ),
            );
        }
        if status.paused {
            line("Paused until:", &paused_text(status.paused_until, prefix));
        }
        if !status.held_back.is_empty() {
            line("Paused by itself:", &format!("{} (`{prefix} sync anyway` syncs now)", held_text(&status.held_back)));
        }
    }
    // A folder not brought up yet has its path and the state `waiting`: nothing has measured it.
    if !status.path.is_empty() && state != "none" && state != "waiting" {
        if let Some(bytes) = status.local_bytes {
            line("On this computer:", &human_bytes(bytes));
        }
        if let Some(pinned) = status.pinned {
            line("Always on this device:", &pinned.to_string());
        }
    }
    if status.conflicts > 0 {
        line("Conflicts:", &format!("{} (see `{prefix} sync conflicts`)", status.conflicts));
    }
    out
}

/// The width of `sync status`'s label column: `Always on this device:` is the
/// longest label.
const SYNC_STATUS_WIDTH: usize = 24;

/// `sync status`'s `Mode:` line: the account's mode, and for a read-write account whether
/// the folder is `writable` (`Folder.Writable`) — one that is not runs read-only for now. A
/// daemon that does not say (`None`) leaves the account's mode alone.
pub fn mode_text(mode: &str, writable: Option<bool>) -> String {
    match mode {
        "read-write" if writable == Some(false) => {
            "read-write, but this folder is read-only for now: nothing made or changed here is uploaded".to_owned()
        }
        "read-write" => "read-write: changes made here are uploaded".to_owned(),
        "read-only" => "read-only: nothing made or changed here is uploaded".to_owned(),
        other => other.to_owned(),
    }
}

/// The Full local scan as `LocalScan`'s properties say it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalScan {
    pub state: String,
    pub reason: String,
    pub started: i64,
    pub directories: u64,
    pub files: u64,
    pub expected: u64,
    pub finished: i64,
    pub took: u32,
}

/// `sync status`'s `Local scan:` line: `running — 1 234 folders and 45 678 files, of about
/// 50 000 (2 min, after the switch to read-write)`, `last finished 5 min ago (took 40 s)`,
/// `not yet since the daemon started`, or `none — read-only`.
pub fn local_scan_text(scan: &LocalScan, now: i64) -> String {
    match scan.state.as_str() {
        "none" => "none — read-only".to_owned(),
        "running" => {
            let mut text = format!("running — {} folders and {} files", grouped(scan.directories), grouped(scan.files));
            if scan.expected > 0 {
                text.push_str(&format!(", of about {}", grouped(scan.expected)));
            }
            let running = seconds_text(u64::try_from(now - scan.started).unwrap_or(0));
            format!("{text} ({running}, {})", scan_reason_text(&scan.reason))
        }
        _ if scan.finished == 0 => "not yet since the daemon started".to_owned(),
        _ => format!("last finished {} (took {})", checked_text(scan.finished, now), seconds_text(u64::from(scan.took))),
    }
}

/// Why a local scan runs, after its count.
pub fn scan_reason_text(reason: &str) -> String {
    match reason {
        "start" => "as syncing started".to_owned(),
        "read-write" => "after the switch to read-write".to_owned(),
        "helper-back" => "after the helper came back".to_owned(),
        "overflow" => "after too many changes at once for the notifications".to_owned(),
        "ignore-list" => "after the ignore list changed".to_owned(),
        "periodic" => "the regular scan while part of the folder cannot be watched".to_owned(),
        other => other.to_owned(),
    }
}

/// `sync status`'s `Paused until:` line.
pub fn paused_text(until: i64, prefix: &str) -> String {
    if until == 0 {
        format!("resumed (`{prefix} sync resume`)")
    } else {
        format!("{} (`{prefix} sync resume` ends it now)", local_time(until))
    }
}

/// How changes made in OneDrive arrive (`Folder.LiveChanges`), as `sync status` says it;
/// nothing while `off`: the pause or the hold says why already.
pub fn live_text(live: &str) -> Option<&'static str> {
    match live {
        "connected" => Some("live"),
        "connecting" => Some("every minute (connecting)"),
        _ => None,
    }
}

/// Why an account holds back by itself (`Folder.HeldBack`), as `sync status` says it.
pub fn held_text(reason: &str) -> &str {
    match reason {
        "metered" => "metered connection",
        "on-battery" => "on battery",
        "power-saver" => "power-saver mode",
        other => other,
    }
}

/// `sync status`'s `Helper:` line ([`helper_text`]).
pub fn helper_line(helper: &str) -> String {
    format!("{:<W$}{}\n", "Helper:", helper_text(helper), W = SYNC_STATUS_WIDTH)
}

/// What happens when something opens a file in a folder in `state`, for the
/// states where that is known: `ready` (the helper intercepts and fills) and
/// `no-interception` (nothing does); `waiting` is a folder that is not up yet. `error` can
/// mean either — `LastError` says which — and `none` has no folder to talk about.
fn opens_line(state: &str) -> Option<&'static str> {
    match state {
        "ready" => Some("intercepted: a file is downloaded when something opens it"),
        "no-interception" => Some(
            "NOT intercepted: a file that is not downloaded reads as zeros until you run \
             `konedrivectl sync hydrate <file>`",
        ),
        "listing" => Some("the folder is being filled with your OneDrive's items"),
        "waiting" => Some("not yet: the folder is being brought up, or waits for the helper to connect"),
        _ => None,
    }
}

/// The `Helper:` line's text (HS4): `HelperState`, and what to do about it
/// when the helper is not connected — the same words `LastError` uses.
pub fn helper_text(state: &str) -> String {
    match HelperState::parse(state).and_then(HelperState::advice) {
        Some(advice) => format!("{state} — {advice}"),
        None => state.to_owned(),
    }
}

#[cfg(test)]
mod tests;
