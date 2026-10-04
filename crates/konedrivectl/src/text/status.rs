use konedrive_dbus::accounts::{AccountProxy, FolderProxies};

use super::formats::{checked_text, grouped, human_bytes, local_time, seconds_text, unix_now};
use super::transfers::waiting_download_text;
use super::uploads::{space_waiting_text, waiting_text};

/// `status`'s `Client ID:` line: `Accounts.ClientId`, one for every account.
pub fn client_id_line(client_id: &str) -> String {
    let shown = if client_id.is_empty() { "(not set)" } else { client_id };
    format!("{:<12}{shown}\n", "Client ID:")
}

/// `status` for one account. `client_id` is `Some` when this account is all `status` shows:
/// its label and the client ID are printed with it. When `status` shows several, the client
/// ID is printed once above them and each block is headed by its label: `None`.
pub async fn status_text(proxy: &AccountProxy<'_>, client_id: Option<&str>) -> zbus::Result<String> {
    let state = proxy.state().await?;
    let mut out = String::new();
    if let Some(client_id) = client_id {
        out.push_str(&format!("{:<12}{}\n", "Label:", proxy.label().await?));
        out.push_str(&format!("{:<12}{state}\n", "State:"));
        out.push_str(&client_id_line(client_id));
    } else {
        out.push_str(&format!("{:<12}{state}\n", "State:"));
    }
    out.push_str(&format!("{:<12}{}\n", "Mode:", proxy.mode().await?));
    if state == "signed-in" {
        out.push_str(&format!(
            "{:<12}{} <{}>\n",
            "Account:",
            proxy.display_name().await?,
            proxy.email().await?
        ));
        out.push_str(&format!(
            "{:<12}{} of {} used\n",
            "Storage:",
            human_bytes(proxy.quota_used().await?),
            human_bytes(proxy.quota_total().await?)
        ));
    }
    let last_error = proxy.last_error().await?;
    if !last_error.is_empty() {
        out.push_str(&format!("{:<12}{last_error}\n", "Last error:"));
    }
    Ok(out)
}

/// The account status's layout, with a wider label column: `Always on this
/// device:` is the longest label.
///
/// `Folder.State` is `none` on an ordinary machine that has never registered a
/// folder — this prints as an unremarkable "(none)", not an error. `error`
/// means a root is registered but something needs attention (startup
/// recovery could not finish, or could not even run, including a recovery
/// that finished with files it could not fix); `LastError` then carries the
/// detail and is always printed alongside it, so `error` can never be
/// mistaken for `ready` by someone scanning quickly.
///
/// A registered folder also gets an `Opens:` line, in this CLI's own words,
/// saying whether anything fills a file when it is opened. That matters
/// most for `no-interception`, the developer's mode: without the helper, a
/// file that is not downloaded reads as zeros, and that has to be on screen
/// every time, not left to the user's memory or to whatever `LastError`
/// happens to say.
///
/// `Helper:` says how the privileged helper stands (`Accounts.HelperState`,
/// HS4, passed in as `helper`) and, when it is not connected, how to install,
/// start or look at it — whether or not a folder is registered. One helper
/// serves every account, so when `sync status` shows several, it prints that
/// line once above them and each block is printed with `helper = None`.
///
/// `Last checked:` is added for a folder that shows OneDrive — "20 s
/// ago", or "never" — and `On this computer:` for any registered folder
/// that is up (not one the daemon has only just started with, whose state is
/// still `none`): what its files take on this disk — and `Always on this
/// device:`, how many files and folders are pinned (`konedrivectl sync pin`).
/// `Conflicts:` says how many local versions were moved out of the way,
/// when there are any: they are not a problem, so `LastError` does not carry
/// them.
pub async fn sync_status_text(proxy: &FolderProxies<'_>, helper: Option<&str>, prefix: &str) -> zbus::Result<String> {
    const W: usize = SYNC_STATUS_WIDTH;
    let path = proxy.folder.path().await?;
    let state = proxy.folder.state().await?;
    let shown = if path.is_empty() { "(none)" } else { path.as_str() };
    let mut out = format!("{:<W$}{shown}\n", "Folder:");
    out.push_str(&format!("{:<W$}{state}\n", "State:"));
    if let Some(opens) = opens_line(&state) {
        out.push_str(&format!("{:<W$}{opens}\n", "Opens:"));
    }
    if let Some(helper) = helper {
        out.push_str(&helper_line(helper));
    }
    let last_error = proxy.folder.last_error().await?;
    if !last_error.is_empty() {
        out.push_str(&format!("{:<W$}{last_error}\n", "Last error:"));
    }
    if proxy.folder.source().await? == "onedrive" {
        let (listed, placed, skipped) =
            (proxy.folder.items_listed().await?, proxy.folder.items_placed().await?, proxy.folder.skipped_count().await?);
        out.push_str(&format!("{:<W$}{listed} in OneDrive, {placed} in the folder\n", "Items:"));
        if skipped > 0 {
            out.push_str(&format!("{:<W$}{skipped} (see `{prefix} sync skipped`)\n", "Skipped:"));
        }
        let checked = checked_text(proxy.folder.last_checked().await?, unix_now());
        out.push_str(&format!("{:<W$}{checked}\n", "Last checked:"));
        if let Some(live) = live_text(&proxy.folder.live_changes().await?) {
            out.push_str(&format!("{:<W$}{live}\n", "Changes from OneDrive:"));
        }
        let mode = account_mode(proxy).await;
        out.push_str(&format!("{:<W$}{}\n", "Mode:", mode_text(&mode)));
        let (down, down_bytes) = (proxy.transfers.download_left_count().await?, proxy.transfers.download_left_bytes().await?);
        out.push_str(&format!("{:<W$}{}\n", "Waiting to download:", waiting_download_text(down, down_bytes)));
        let scan = LocalScan {
            state: proxy.scan.state().await?,
            reason: proxy.scan.reason().await?,
            started: proxy.scan.started().await?,
            directories: proxy.scan.directories().await?,
            files: proxy.scan.files().await?,
            expected: proxy.scan.expected().await?,
            finished: proxy.scan.finished().await?,
            took: proxy.scan.took().await?,
        };
        out.push_str(&format!("{:<W$}{}\n", "Local scan:", local_scan_text(&scan, unix_now())));
        let (pending, bytes, blocked) = (proxy.queue.pending_count().await?, proxy.queue.pending_bytes().await?, proxy.queue.blocked_count().await?);
        if mode == "read-write" || pending > 0 || blocked > 0 {
            out.push_str(&format!("{:<W$}{}\n", "Waiting to upload:", waiting_text(pending, bytes)));
        }
        if blocked > 0 {
            out.push_str(&format!("{:<W$}{blocked} (see `{prefix} sync not-uploaded`)\n", "Blocked:"));
        }
        if proxy.queue.quota_full().await? {
            let (count, bytes) = (proxy.queue.quota_waiting_count().await?, proxy.queue.quota_waiting_bytes().await?);
            out.push_str(&format!("{:<W$}{}\n", "Waiting for space:", space_waiting_text(count, bytes)));
        }
        let too_big = proxy.queue.too_big_count().await?;
        if too_big > 0 {
            out.push_str(&format!("{:<W$}{too_big} (see `{prefix} sync outbox`)\n", "Too big for the space:"));
        }
        let held = proxy.queue.held_count().await?;
        if held > 0 {
            out.push_str(&format!(
                "{:<W$}{held} deletions (`{prefix} sync deletes confirm` or `{prefix} sync deletes restore`)\n",
                "Held for confirmation:"
            ));
        }
        if proxy.folder.paused().await? {
            let until = proxy.folder.paused_until().await?;
            out.push_str(&format!("{:<W$}{}\n", "Paused until:", paused_text(until, prefix)));
        }
        let held = proxy.folder.held_back().await?;
        if !held.is_empty() {
            out.push_str(&format!("{:<W$}{} (`{prefix} sync anyway` syncs now)\n", "Paused by itself:", held_text(&held)));
        }
    }
    // A folder not brought up yet has its path and the state `waiting`: nothing has measured it.
    if !path.is_empty() && state != "none" && state != "waiting" {
        out.push_str(&format!("{:<W$}{}\n", "On this computer:", human_bytes(proxy.folder.local_bytes().await?)));
        out.push_str(&format!("{:<W$}{}\n", "Always on this device:", proxy.folder.pinned_count().await?));
    }
    let conflicts = proxy.conflicts.count().await?;
    if conflicts > 0 {
        out.push_str(&format!("{:<W$}{conflicts} (see `{prefix} sync conflicts`)\n", "Conflicts:"));
    }
    Ok(out)
}

/// The width of `sync status`'s label column: `Always on this device:` is the
/// longest label.
const SYNC_STATUS_WIDTH: usize = 24;

/// `Account.Mode` of the account whose folder is `proxy` (the same object);
/// empty when it cannot be read.
async fn account_mode(proxy: &FolderProxies<'_>) -> String {
    let inner = proxy.folder.inner();
    let account = async {
        konedrive_dbus::accounts::AccountProxy::builder(inner.connection())
            .path(inner.path().to_owned())?
            .build()
            .await?
            .mode()
            .await
    };
    account.await.unwrap_or_default()
}

/// `sync status`'s `Mode:` line.
pub fn mode_text(mode: &str) -> String {
    match mode {
        "read-write" => "read-write: changes made here are uploaded".to_owned(),
        "read-only" => "read-only: nothing made or changed here is uploaded".to_owned(),
        other => other.to_owned(),
    }
}

/// The Full local scan as `LocalScan`'s properties say it (issue #8).
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

/// `sync thumbnails`' answer.
pub fn thumbnails_text(on: bool) -> &'static str {
    if on {
        "Thumbnails: on — OneDrive's previews of images and videos are downloaded."
    } else {
        "Thumbnails: off — Dolphin downloads a cloud-only file in full to show its preview while its previews are on."
    }
}

/// `settings on-metered`'s answer.
pub fn on_metered_text(pause: bool) -> &'static str {
    if pause {
        "On a metered connection: pause."
    } else {
        "On a metered connection: sync as usual."
    }
}

/// `settings on-battery`'s answer, for `sync`, `power-saver` or `pause`.
pub fn on_battery_text(choice: &str) -> String {
    match choice {
        "sync" => "On battery: sync as usual.".to_owned(),
        "power-saver" => "On battery: pause in power-saver mode.".to_owned(),
        "pause" => "On battery: pause.".to_owned(),
        other => format!("On battery: {other}."),
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

/// The running daemon's build, as `konedrivectl --version` found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonBuild {
    /// No daemon on the session bus (or no session bus): nothing was started to ask.
    NotRunning(String),
    /// A daemon that did not say: a build older than the `Version` property, most likely.
    Unknown(String),
    Running { version: String, commit: String },
}

/// How to restart the daemon, so that it runs the build installed.
pub const RESTART_HINT: &str = "systemctl --user restart konedrived";

/// What `konedrivectl --version` prints: its own line, the daemon's, and, when the daemon runs
/// another build than this one (another version or another commit), a line that says to restart
/// it.
pub fn version_text(version: &str, commit: &str, daemon: &DaemonBuild) -> String {
    use konedrive_dbus::version::line;
    let mut out = line("konedrivectl", version, commit) + "\n";
    let other = match daemon {
        DaemonBuild::NotRunning(why) => {
            out.push_str(&format!("konedrived: not running ({why})\n"));
            false
        }
        DaemonBuild::Unknown(why) => {
            out.push_str(&format!("konedrived: running, version unknown ({why})\n"));
            true
        }
        DaemonBuild::Running { version: daemon_version, commit: daemon_commit } => {
            out.push_str(&line("konedrived", daemon_version, daemon_commit));
            out.push('\n');
            daemon_version != version || daemon_commit != commit
        }
    };
    if other {
        out.push_str(&format!(
            "The daemon runs another build than this one: restart it ({RESTART_HINT}) to use this one.\n"
        ));
    }
    out
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
    match konedrive_dbus::helper_advice(state) {
        Some(advice) => format!("{state} — {advice}"),
        None => state.to_owned(),
    }
}

/// `sync activity`: one line per event, newest first — time, kind, full
/// path, and the detail in parentheses when there is one.
pub fn activity_text(events: &[(i64, String, String, String)]) -> String {
    if events.is_empty() {
        return "Nothing has happened yet.\n".to_owned();
    }
    let mut out = String::new();
    for (at, kind, path, detail) in events {
        let detail = if detail.is_empty() { String::new() } else { format!("  ({detail})") };
        out.push_str(&format!("{}  {kind:<10}  {path}{detail}\n", local_time(*at)));
    }
    out
}

#[cfg(test)]
mod tests;
