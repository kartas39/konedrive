//! The testable part of `konedrivectl`.
//!
//! The daemon serves one object per account (`konedrive_dbus::account_path`) below the
//! accounts manager (`konedrive_dbus::ACCOUNTS_PATH`). A command acts on one account, chosen
//! as [`choose`] says; `status` and `sync status` show every account when none is chosen; the
//! commands that take a path go through `Files`, which finds the account by the path.

use std::time::Duration;

use konedrive_dbus::accounts::{AccountProxy, FolderProxies};
use konedrive_dbus::{error_name, ERROR_PREFIX};
use zbus::zvariant::OwnedObjectPath;

/// The label `konedrivectl login` gives the account it adds when there is none, as the
/// daemon names the account it migrates from a single-account configuration.
pub const FIRST_LABEL: &str = "Personal";

/// The environment variable that chooses the account when `--account` is not given.
pub const ACCOUNT_VARIABLE: &str = "KONEDRIVE_ACCOUNT";

/// The environment variable that, set to anything but empty, keeps the sign-in page from
/// being opened in a browser (issue #21): a sign-in over SSH, with no desktop, or in tests.
pub const NO_BROWSER_VARIABLE: &str = "KONEDRIVE_NO_BROWSER";

/// Whether a command opens the sign-in page in the browser: only when
/// [`NO_BROWSER_VARIABLE`] is unset or empty (`no_browser`, its value) and stdout is a
/// terminal, so that nobody's desktop gets a browser tab nobody looks at. The address is
/// printed either way.
pub fn opens_browser(no_browser: Option<&std::ffi::OsStr>, terminal: bool) -> bool {
    no_browser.is_none_or(|v| v.is_empty()) && terminal
}

/// One account, as a command chooses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub path: OwnedObjectPath,
    pub id: String,
    pub label: String,
    /// Empty until the account has signed in once.
    pub email: String,
}

/// The account `wanted` names (design §5.1): the one whose id is exactly `wanted`, or whose
/// label or email is `wanted` whatever the case. The daemon refuses a label with an `@` or
/// shaped like an id, but a hand-edited `config.toml` can still give two accounts one label,
/// or one account another's id as its label: so every account any of the three names counts,
/// and `Err` holds them all when there is not exactly one — none, or several, which a command
/// must refuse rather than guess between (`account remove` asks nothing).
pub fn resolve<'a>(accounts: &'a [AccountInfo], wanted: &str) -> Result<&'a AccountInfo, Vec<&'a AccountInfo>> {
    let wanted = wanted.trim();
    let lower = wanted.to_lowercase();
    let named: Vec<&AccountInfo> = accounts
        .iter()
        .filter(|a| {
            a.id == wanted || a.label.to_lowercase() == lower || (!a.email.is_empty() && a.email.to_lowercase() == lower)
        })
        .collect();
    match named.as_slice() {
        [one] => Ok(one),
        _ => Err(named),
    }
}

/// Where the name of the account to use came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--account`.
    Option,
    /// [`ACCOUNT_VARIABLE`].
    Environment,
    /// An argument of the command itself (`account rename`, `account remove`).
    Argument,
}

/// Why a command that acts on one account has none to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoChoice {
    /// There is no account at all.
    NoAccountYet,
    /// The name given is no account's id, label or email.
    Unknown { wanted: String, source: Source, labels: Vec<String> },
    /// The name given is several accounts' id, label or email: each as `label (id)`.
    Ambiguous { wanted: String, candidates: Vec<String> },
    /// Several accounts, and none chosen.
    Several { labels: Vec<String> },
}

impl NoChoice {
    /// 2, as for any other mistake on the command line, when the command has to name an
    /// account; 1 when there is none it could name.
    pub fn exit_status(&self) -> u8 {
        match self {
            NoChoice::NoAccountYet => 1,
            NoChoice::Unknown { labels, .. } if labels.is_empty() => 1,
            _ => 2,
        }
    }
}

impl std::fmt::Display for NoChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoChoice::NoAccountYet => write!(
                f,
                "No account yet: `konedrivectl account add <label>` adds one, or `konedrivectl login` adds \
                 one called {FIRST_LABEL} and signs it in"
            ),
            NoChoice::Unknown { wanted, source: Source::Environment, labels } if labels.is_empty() => write!(
                f,
                "{ACCOUNT_VARIABLE} names the account {wanted:?}, and there are no accounts yet. \
                 `konedrivectl account add {}` adds it; or `unset {ACCOUNT_VARIABLE}`, and `konedrivectl \
                 login` adds one called {FIRST_LABEL} and signs it in",
                shell_word(wanted)
            ),
            NoChoice::Unknown { wanted, labels, .. } if labels.is_empty() => write!(
                f,
                "there is no account {wanted:?}: there are no accounts yet. `konedrivectl account add <label>` \
                 adds one"
            ),
            NoChoice::Unknown { wanted, source: Source::Environment, labels } => write!(
                f,
                "{ACCOUNT_VARIABLE} names no account: {wanted:?}. The accounts are {} (`konedrivectl account \
                 list` shows their ids and emails)",
                labels.join(", ")
            ),
            NoChoice::Unknown { wanted, labels, .. } => write!(
                f,
                "there is no account {wanted:?}. The accounts are {} (`konedrivectl account list` shows their \
                 ids and emails)",
                labels.join(", ")
            ),
            NoChoice::Ambiguous { wanted, candidates } => write!(
                f,
                "{wanted:?} names several accounts: {}. Nothing was done: name the one you mean by \
                 another of its names — its label, id or email, as `konedrivectl account list` shows them",
                candidates.join(", ")
            ),
            NoChoice::Several { labels } => {
                write!(f, "Several accounts: choose one with --account ({})", labels.join(", "))
            }
        }
    }
}

impl std::error::Error for NoChoice {}

/// The account a command acts on (design §5.1): the one `wanted` names, with where the name
/// came from; with no name, the only account there is.
pub fn choose<'a>(accounts: &'a [AccountInfo], wanted: Option<(&str, Source)>) -> Result<&'a AccountInfo, NoChoice> {
    let labels = || accounts.iter().map(|a| a.label.clone()).collect::<Vec<_>>();
    match wanted {
        Some((name, source)) => resolve(accounts, name).map_err(|matches| match matches.as_slice() {
            [] => NoChoice::Unknown { wanted: name.trim().to_owned(), source, labels: labels() },
            several => NoChoice::Ambiguous {
                wanted: name.trim().to_owned(),
                candidates: several.iter().map(|a| format!("{} ({})", a.label, a.id)).collect(),
            },
        }),
        None => match accounts {
            [] => Err(NoChoice::NoAccountYet),
            [one] => Ok(one),
            _ => Err(NoChoice::Several { labels: labels() }),
        },
    }
}

/// How a command this CLI suggests names the account it is about: `konedrivectl --account
/// <label>` whenever the bare command could act on another account — there are several, or
/// [`ACCOUNT_VARIABLE`] is set — and `konedrivectl` otherwise. `label` is `None` when no one
/// account is meant (a path in no folder, say): then `<account>` stands in for it.
pub fn command_prefix(label: Option<&str>, several: bool, variable_set: bool) -> String {
    if !several && !variable_set {
        return "konedrivectl".to_owned();
    }
    match label {
        Some(label) => format!("konedrivectl --account {}", shell_word(label)),
        None => "konedrivectl --account <account>".to_owned(),
    }
}

/// Whether `error` says that the object called is not there: an account removed while the
/// command ran.
pub fn is_gone(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => is_gone_name(name.as_str()),
        zbus::Error::FDO(error) => {
            use zbus::DBusError;
            is_gone_name(error.name().as_str())
        }
        _ => false,
    }
}

/// `label` as one word of a shell command: as it is when it needs no quoting, in single
/// quotes otherwise.
pub fn shell_word(label: &str) -> String {
    if !label.is_empty() && label.chars().all(|c| c.is_alphanumeric() || "._-+,:".contains(c)) {
        label.to_owned()
    } else {
        format!("'{}'", label.replace('\'', r"'\''"))
    }
}

/// One line of `account list`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountRow {
    pub id: String,
    pub label: String,
    pub email: String,
    pub state: String,
    pub mode: String,
    /// `Folder.Path`: empty with no folder.
    pub folder: String,
    pub root_state: String,
}

/// `account list` (design §5.2): a table of every account, in the order they were added —
/// id, label, email, sign-in state, mode, and the folder with its `Folder.State`.
pub fn account_list_text(rows: &[AccountRow]) -> String {
    if rows.is_empty() {
        return format!(
            "No accounts yet. `konedrivectl login` adds one called {FIRST_LABEL} and signs it in; \
             `konedrivectl account add <label>` adds one by another name.\n"
        );
    }
    let none = || "\u{2014}".to_owned();
    let mut table = vec![["ID", "LABEL", "EMAIL", "STATE", "MODE", "FOLDER"].map(str::to_owned)];
    for row in rows {
        let email = if row.email.is_empty() { none() } else { row.email.clone() };
        let folder = if row.folder.is_empty() { none() } else { format!("{} ({})", row.folder, row.root_state) };
        table.push([row.id.clone(), row.label.clone(), email, row.state.clone(), row.mode.clone(), folder]);
    }
    let mut widths = [0; 6];
    for line in &table {
        for (width, cell) in widths.iter_mut().zip(line) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for line in &table {
        let cells: Vec<String> = line.iter().zip(widths).map(|(cell, w)| format!("{cell:<w$}")).collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// `text` with every line that is not empty indented by two spaces: one account's block in
/// `status` and `sync status` when they show several.
pub fn indented(text: &str) -> String {
    text.lines().map(|line| if line.is_empty() { "\n".to_owned() } else { format!("  {line}\n") }).collect()
}

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
/// ago", or "never" — and `On this computer:` for any registered folder:
/// what its files take on this disk — and `Always on this device:`, how
/// many files and folders are pinned (`konedrivectl sync pin`).
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
    if !path.is_empty() {
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

/// `40 s`, `2 min`, `3 h 5 min`.
fn seconds_text(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds} s"),
        60..=3_599 => format!("{} min", seconds / 60),
        _ if seconds % 3_600 < 60 => format!("{} h", seconds / 3_600),
        _ => format!("{} h {} min", seconds / 3_600, seconds % 3_600 / 60),
    }
}

/// `45 678`: the thousands set apart.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

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

/// A duration as `sync pause --for` takes it: `90s`, `30m`, `2h`, `1d`, or
/// several at once (`1h30m`); a bare number is seconds. `None` for anything
/// else, for zero, and for more than a `u32` of seconds.
pub fn parse_duration(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(seconds) = text.parse::<u64>() {
        return u32::try_from(seconds).ok().filter(|&s| s > 0);
    }
    let (mut total, mut number) = (0u64, String::new());
    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return None,
        };
        let value: u64 = std::mem::take(&mut number).parse().ok()?;
        total = total.checked_add(value.checked_mul(unit)?)?;
    }
    if !number.is_empty() {
        return None;
    }
    u32::try_from(total).ok().filter(|&s| s > 0)
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
        "too-big" => "too big for the space left in OneDrive: free up space there, then `sync refresh`".to_owned(),
        other => match (other.strip_prefix("refused: "), too_big(other)) {
            (Some(message), _) => format!("OneDrive refused it: {message}"),
            (None, Some((needs, free))) => format!("too big: needs {}, {} free", human_bytes(needs), human_bytes(free)),
            (None, None) => other.to_owned(),
        },
    }
}

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
/// `no-interception` (nothing does). `error` can mean either — `LastError`
/// says which — and `none` has no folder to talk about.
fn opens_line(state: &str) -> Option<&'static str> {
    match state {
        "ready" => Some("intercepted: a file is downloaded when something opens it"),
        "no-interception" => Some(
            "NOT intercepted: a file that is not downloaded reads as zeros until you run \
             `konedrivectl sync hydrate <file>`",
        ),
        "listing" => Some("the folder is being filled with your OneDrive's items"),
        _ => None,
    }
}

/// The `sync` subcommand a refusal answered, with the path it named — the
/// context a refusal needs to be explained in terms of *this* user's file.
#[derive(Debug, Clone, Copy)]
pub enum SyncAction<'a> {
    Register(&'a str),
    RegisterWithoutInterception(&'a str),
    Forget,
    PopulateFrom(&'a str),
    Hydrate(&'a str),
    Dehydrate(&'a str),
    Refresh,
    Skipped,
    Activity,
    Conflicts,
    Dismiss(&'a str),
    FreeUpSpace,
    /// The paths given, joined with ", ".
    Pin(&'a str),
    /// The path refused ([`refused_path`]), or the paths given, joined with ", ".
    Unpin(&'a str),
    /// The path refused ([`refused_path`]), or the paths given, joined with ", ".
    Free(&'a str),
    /// `sync open`: `Files.WebUrl`, with the path.
    Open(&'a str),
    /// `account remove`, with the account's label: `Accounts.Remove` forgets the
    /// folder as `Forget` does, and is refused under the same names.
    Remove(&'a str),
    Outbox,
    Pause,
    Resume,
    Ignore,
    /// `sync thumbnails`: an account's own sync setting.
    Settings,
    /// `sync anyway`.
    Anyway,
    NotUploaded,
    Deletes,
}

impl SyncAction<'_> {
    /// "downloading /path", for the one sentence every refusal without a
    /// name of its own is built from.
    fn doing(&self) -> String {
        match self {
            Self::Register(path) => format!("registering {path}"),
            Self::RegisterWithoutInterception(path) => {
                format!("registering {path} without interception")
            }
            Self::Forget => "forgetting the sync folder".to_owned(),
            Self::PopulateFrom(dir) => format!("filling the sync folder from {dir}"),
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
            Self::Outbox => "listing the changes waiting to upload".to_owned(),
            Self::Pause => "pausing the sync".to_owned(),
            Self::Resume => "resuming the sync".to_owned(),
            Self::Ignore => "changing the ignore list".to_owned(),
            Self::Settings => "changing the sync settings".to_owned(),
            Self::Anyway => "syncing anyway".to_owned(),
            Self::NotUploaded => "listing what is not uploaded".to_owned(),
            Self::Deletes => "deciding on the large delete".to_owned(),
        }
    }

    fn path(&self) -> &str {
        match self {
            Self::Register(path)
            | Self::RegisterWithoutInterception(path)
            | Self::PopulateFrom(path)
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
            | Self::Outbox
            | Self::Pause
            | Self::Resume
            | Self::Ignore
            | Self::Settings
            | Self::Anyway
            | Self::NotUploaded
            | Self::Deletes => "",
        }
    }
}

/// What to tell a person when a folder call made for `action` failed.
///
/// Matches the D-Bus error **name** (`konedrive_dbus::error_name`), never the
/// message: every refusal the folder makes arrives under
/// `konedrive_dbus::ERROR_PREFIX`, and each gets a sentence saying what
/// happened to the user's file and what they can do about it. `root` is the
/// registered folder (`Folder.Path`, possibly empty), which two refusals name.
///
/// The daemon's own message is kept only where it is the specific part:
/// `Unsupported` (which filesystem feature is missing) and anything with no
/// name of its own (`Failed`, or an error from the bus itself), where it is
/// all there is.
pub fn explain_sync_error(action: SyncAction<'_>, error: &zbus::Error, root: &str) -> String {
    let detail = match error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    };
    refusal_text(action, error_name(error), &detail, root)
}

/// What the CLI knows of the daemon besides a refusal, read after it: the
/// folder of the account the refusal is about (`Folder.Path`: the chosen
/// account's, or for a path, the folder that holds it), what it shows
/// (`RootSource`), the helper (`Accounts.HelperState`), and for a path, every
/// account's folder. Any of them may be empty.
#[derive(Debug, Default, Clone, Copy)]
pub struct Context<'a> {
    pub root: &'a str,
    pub source: &'a str,
    pub helper: &'a str,
    /// Every account's registered folder, for a path command: a path in none
    /// of them is refused `OutsideRoot` before any account sees it.
    pub folders: &'a [String],
    /// The folder a registration named carries another account's drive
    /// (`user.konedrive.drive`, design §8.3), which the daemon refuses under
    /// `NotEmpty` too.
    pub foreign: bool,
    /// How a suggested command names the account ([`command_prefix`]); empty
    /// for plain `konedrivectl`.
    pub prefix: &'a str,
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

/// [`explain_sync_error`], with what [`Context`] adds: a refusal for want
/// of the helper ends with how to start it (HS4), a folder that shows
/// OneDrive is not told to populate itself from a directory (B-M6), a path
/// in no account's folder is told which folders there are, and a folder
/// that was another account's is told so.
pub fn explain_sync_error_in(action: SyncAction<'_>, error: &zbus::Error, context: Context<'_>) -> String {
    let detail = match error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    };
    refusal_text_in(action, error_name(error), &detail, context)
}

/// [`explain_sync_error_in`]'s decision, on the name and message alone.
pub fn refusal_text_in(action: SyncAction<'_>, name: Option<&str>, detail: &str, context: Context<'_>) -> String {
    use SyncAction::*;
    let refusal = name.and_then(|name| name.strip_prefix(ERROR_PREFIX)).and_then(|rest| rest.strip_prefix('.'));
    let path = action.path();
    let prefix = context.prefix();
    let path_command = matches!(action, Hydrate(_) | Dehydrate(_) | Pin(_) | Unpin(_) | Free(_) | Open(_));
    // An account removed while this command ran: its object is gone.
    if name.is_some_and(is_gone_name) && !path_command {
        let text = refusal_text_as(action, Some(&format!("{ERROR_PREFIX}.NoAccount")), detail, context.root, prefix);
        return text;
    }
    match (refusal, action) {
        // `Files` refuses a path in no account's folder before any account
        // sees it; with no folder at all, that is `NoRoot`'s situation.
        (Some("OutsideRoot"), _) if path_command && context.root.is_empty() => {
            return match context.folders {
                [] => refusal_text_as(action, Some(&format!("{ERROR_PREFIX}.NoRoot")), detail, "", prefix),
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
        (Some("NotEmpty"), Register(_) | RegisterWithoutInterception(_)) if context.foreign => {
            return format!(
                "{path} holds the files of another OneDrive account's folder, so it cannot be this \
                 account's. Choose an empty folder, or create a new one. If it was this account's own \
                 folder before, sign the account in first (`{prefix} login`), then register it again"
            );
        }
        _ => {}
    }
    let text = refusal_text_as(action, name, detail, context.root, prefix);
    match (refusal, konedrive_dbus::helper_advice(context.helper)) {
        // A folder that shows OneDrive downloads from OneDrive; it has no
        // source yet only while it waits to be brought up.
        (Some("NoSource"), _) if context.source == "onedrive" => {
            "the folder is not connected yet; try again in a moment".to_owned()
        }
        (Some("NoHelper"), Some(advice)) => format!("{text}. {}", sentence(advice)),
        _ => text,
    }
}

/// `text` with its first letter in capitals, to stand as a sentence of its own.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
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

/// Whether `name` is the bus's answer for an object, interface or method that
/// is not there ([`is_gone`]).
fn is_gone_name(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.DBus.Error.UnknownObject"
            | "org.freedesktop.DBus.Error.UnknownMethod"
            | "org.freedesktop.DBus.Error.UnknownInterface"
    )
}

/// [`explain_sync_error`]'s decision, on the name and message alone — so it
/// can be tested with a name and a message that disagree.
pub fn refusal_text(action: SyncAction<'_>, name: Option<&str>, detail: &str, root: &str) -> String {
    refusal_text_as(action, name, detail, root, "konedrivectl")
}

/// [`refusal_text`], with every command it suggests for this account begun
/// with `prefix` ([`command_prefix`]).
fn refusal_text_as(action: SyncAction<'_>, name: Option<&str>, detail: &str, root: &str, prefix: &str) -> String {
    use SyncAction::*;
    let refusal = name
        .and_then(|name| name.strip_prefix(ERROR_PREFIX))
        .and_then(|rest| rest.strip_prefix('.'));
    let path = action.path();
    let folder = if root.is_empty() { String::new() } else { format!(" ({root})") };
    match (refusal, action) {
        // `sync open` (issue #53): OneDrive is asked for the address each time.
        // The daemon's message is the cause (no network, a locked secret
        // storage, an answer that cannot be read), shown when there is one.
        (Some("Unreachable"), _) if detail.is_empty() || detail.starts_with(ERROR_PREFIX) => {
            "OneDrive could not be reached".to_owned()
        }
        (Some("Unreachable"), _) => format!("OneDrive could not be reached: {detail}"),
        (Some("NotSignedIn"), Open(_)) => format!(
            "the account is not signed in, so OneDrive cannot be asked for the page of {path}. Sign in \
             with `{prefix} login` and try again"
        ),
        (Some("NotUploaded"), Open(_)) => format!(
            "{path} is not uploaded yet, so it has no page in OneDrive. It can be opened there once it \
             is uploaded (`{prefix} sync outbox`)"
        ),
        (Some("NotManaged"), Open(_)) => format!(
            "{path} is not a OneDrive file: it is a file of your own in the sync folder, so it has no \
             page in OneDrive"
        ),
        (Some("OutsideRoot"), Open(_)) => format!(
            "{path}: only files and folders inside the sync folder{folder}, and the folder itself, have \
             a page in OneDrive — not symbolic links, or anything outside it"
        ),
        (Some("NotSignedIn"), _) => format!(
            "the account is not signed in, and `{prefix} sync register` binds the folder to the \
             account's OneDrive. Sign in first with `{prefix} login` — or, in the developer's mode \
             with local files, use `{prefix} sync register-without-interception {path}`: without \
             the helper, files that are not downloaded read as zeros until you hydrate them"
        ),
        // `Accounts.Remove` forgets the folder first, under Forget's rule.
        (Some("NoHelper"), Remove(label)) => format!(
            "the konedrive helper is not connected, so the account {label} was not removed and \
             nothing was changed. Removing it forgets its folder{folder}, and forgetting a folder \
             registered with the helper has to tell the helper to stop watching it; without that, \
             a file freed up there later could read as zeros from then on. Try again once the \
             helper is back (`konedrivectl sync status` shows when it is)"
        ),
        // The changes waiting to upload would go with the account's folder.
        (Some("PendingUploads"), Remove(label)) => {
            let prefix = format!("konedrivectl --account {}", shell_word(label));
            format!(
                "the account {label} was not removed, and nothing was changed: {detail}. `{prefix} sync \
                 outbox` lists what waits; `{prefix} account mode read-only --force` drops it — the \
                 files stay here as they are, and OneDrive does not get the changes — and the account \
                 can be removed then"
            )
        }
        (Some("PendingUploads"), _) => format!(
            "the sync folder{folder} is still registered, and nothing was changed: {detail}. `{prefix} \
             sync outbox` lists what waits; `{prefix} account mode read-only --force` drops it — the \
             files stay here as they are, and OneDrive does not get the changes — and the folder can \
             be forgotten then"
        ),
        (Some("NoAccount"), Remove(label)) => format!(
            "there is no account {label} any more, so nothing was removed. `konedrivectl account \
             list` shows the accounts there are"
        ),
        (Some("NoAccount"), _) => "that account is gone: it was removed meanwhile. `konedrivectl \
             account list` shows the accounts there are"
            .to_owned(),
        // Design §8.3: the folders of two accounts never nest. The daemon's
        // message names the other account.
        (Some("Overlaps"), _) => format!(
            "{path} cannot be this account's folder: {detail}. The folders of two accounts cannot \
             be one inside the other: choose a folder outside every other account's folder \
             (`konedrivectl account list` shows them)"
        ),
        // In a folder registered without interception too: a
        // helper that is running while this daemon has no connection to it
        // may hold a mark on the file that nothing can clear.
        (Some("NoHelper"), Dehydrate(_)) => format!(
            "the konedrive helper is not connected, so nothing was changed. Freeing up {path} \
             must first have the helper take off any mark that lets the file's opens through \
             unchecked, or the emptied file could read as zeros from then on; try again once \
             the daemon is connected to the helper again — it reconnects on its own"
        ),
        // follow-up: a file that may still carry the helper's
        // ignore mark is only downloaded again once the helper has cleared
        // it, since a download that fails empties the file.
        (Some("NoHelper"), FreeUpSpace | Free(_)) => "the konedrive helper is not connected, so nothing more \
             was freed up. Freeing up a file must first have the helper take off any mark that lets \
             the file's opens through unchecked, or the emptied file could read as zeros from then \
             on; try again once the daemon is connected to the helper again — it reconnects on its \
             own"
            .to_owned(),
        (Some("NoConflict"), _) => format!(
            "{path} is not in the list of conflicts, so there was nothing to dismiss. `{prefix} \
             sync conflicts` lists them, each under the path where your version is kept"
        ),
        (Some("NoHelper"), Hydrate(_)) => format!(
            "the konedrive helper is not connected, so nothing was changed. {path} was left \
             half freed up, or is marked downloaded with nothing to show it was, and downloading \
             it again must first have the helper stop letting it through unchecked — a download \
             that failed partway would otherwise leave it reading as zeros. Try again once the \
             helper is back (`konedrivectl sync status` shows when it is)"
        ),
        // A folder registered with the helper is forgotten
        // through the helper or not at all.
        (Some("NoHelper"), Forget) => format!(
            "the konedrive helper is not connected, so the sync folder{folder} is still \
             registered and nothing was changed. Forgetting it has to tell the helper to stop \
             watching it; without that, a file freed up there later could read as zeros from \
             then on. Try again once the helper is back (`konedrivectl sync status` shows \
             when it is)"
        ),
        // said of `refresh`, the registration text
        // read "and  was not registered", with no path.
        (Some("NoHelper"), Refresh) => "the konedrive helper is not connected, so the folder is not \
             kept in step with OneDrive until it is, and nothing was asked for"
            .to_owned(),
        // HS2: a OneDrive folder is registered with the helper or not at
        // all; without interception is the developer's mode, never OneDrive.
        (Some("NoHelper"), _) => format!(
            "the konedrive helper is not connected, so {path} was not registered: only through \
             the helper is a OneDrive folder kept in step and a file downloaded when something \
             opens it. Start the helper and try again (`{prefix} sync register-without-interception` \
             is the developer's mode, for a local folder whose files read as zeros until hydrated)"
        ),
        // What a restored folder that is still waiting for its helper
        // answers, among others: asking for the same folder again, in the
        // other mode.
        (Some("AlreadyRegistered"), _) if !root.is_empty() && path == root => format!(
            "{path} is already the sync folder. To register it again another way, run \
             `{prefix} sync forget` first; it leaves the files in the folder as they are"
        ),
        (Some("AlreadyRegistered"), _) => format!(
            "this account already has a sync folder{folder}, and an account keeps only one. To \
             use {path} instead, run `{prefix} sync forget` first; it leaves the files in the \
             old folder as they are. For another OneDrive account, add an account of its own: \
             `konedrivectl account add <label>`"
        ),
        (Some("NotEmpty"), _) => format!(
            "{path} is not empty. A new sync folder has to start empty, so that nothing already \
             in it is mistaken for a OneDrive file: choose an empty folder, or create a new one"
        ),
        // the source overlaps the sync folder.
        (Some("Unsupported"), PopulateFrom(_)) => {
            format!("the sync folder cannot be filled from {path}: {detail}")
        }
        (Some("Unsupported"), Refresh) => format!(
            "this folder is not connected to OneDrive, so there is nothing to ask for: it was \
             registered while signed out and is filled with `{prefix} sync populate-from`"
        ),
        (Some("Unsupported"), Outbox | Pause | Resume | Ignore | NotUploaded | Deletes) => {
            "this folder is not connected to OneDrive, so nothing is uploaded from it".to_owned()
        }
        (Some("Unsupported"), Settings | Anyway) => {
            "this folder is not connected to OneDrive, so it has no sync settings".to_owned()
        }
        (Some("NoRoot"), Outbox | Pause | Resume | Ignore | NotUploaded | Deletes) => {
            "the folder's sync has not started yet; try again in a moment".to_owned()
        }
        (Some("Unsupported"), _) => {
            let why = detail.strip_prefix(&format!("{path}: ")).unwrap_or(detail);
            format!("{path} cannot be used as the sync folder: {why}")
        }
        (Some("NoRoot"), Forget) => {
            "no sync folder is registered, so there is nothing to forget".to_owned()
        }
        (Some("NoRoot"), _) => format!(
            "no sync folder is registered. Register one first with `{prefix} sync register \
             <folder>` — or, in the developer's mode with local files, `{prefix} sync \
             register-without-interception <folder>`"
        ),
        (Some("NoSource"), _) => format!(
            "KOneDrive does not know where to download {path} from yet. Run `{prefix} sync \
             populate-from <dir>` first; the daemon does not remember that directory across a \
             restart, so run it again after one (files already there are left alone)"
        ),
        (Some("OutsideRoot"), Pin(_) | Unpin(_) | Free(_)) => format!(
            "{path}: only files and folders inside the sync folder{folder} can be kept on this \
             device or freed up — not symbolic links, or anything outside it"
        ),
        (Some("OutsideRoot"), _) => format!(
            "{path} is not a regular file inside the sync folder{folder}. Only files inside it \
             can be downloaded or freed up — not folders, symbolic links, or anything outside it"
        ),
        // A free-up of something "Always keep on this device" keeps here.
        // The daemon names what pins it: "pinned by <path>: unpin it first".
        (Some("NotAllowed"), _) => match (pinned_parts(detail).map(|(_, by)| by), action) {
            (Some(by), Unpin(_)) => format!(
                "{path} is kept on this device because the folder {by} is, so it cannot stop being \
                 kept on its own. `konedrivectl sync unpin {by}` stops keeping the folder"
            ),
            (Some(by), _) if by == path => format!(
                "{path} is kept on this device, so its space is not freed up. `konedrivectl sync \
                 free {by}` stops keeping it and frees it up"
            ),
            (Some(by), _) => format!(
                "{path} is kept on this device because the folder {by} is, so its space is not \
                 freed up. To free it, free up the folder first: `konedrivectl sync free {by}` \
                 stops keeping it and frees up what is in it"
            ),
            (None, _) => format!("{} was refused: {detail}", action.doing()),
        },
        (Some("NotManaged"), Dehydrate(_)) => format!(
            "{path} is not a OneDrive file: it is a file of your own in the sync folder, and \
             KOneDrive never frees the space of a file it could not download again"
        ),
        (Some("NotManaged"), Pin(_) | Unpin(_) | Free(_)) => format!(
            "{path}: a file of your own in the sync folder is not a OneDrive file, so there is \
             nothing to keep on this device or to free up"
        ),
        (Some("NotManaged"), _) => format!(
            "{path} is not a OneDrive file: it is a file of your own in the sync folder, so \
             there is nothing to download"
        ),
        (Some("NotHydrated"), _) => format!(
            "{path} is not downloaded, so there is no space to free — it already takes none"
        ),
        (Some("NotUploaded"), _) => format!(
            "{} is not uploaded yet, so freeing up its space would lose the changes made here. It \
             was left as it is; its space can be freed once it is uploaded (`{prefix} sync outbox`)",
            if path.is_empty() { detail.split(" is not uploaded").next().unwrap_or(detail) } else { path }
        ),
        (Some("ModifiedLocally"), Hydrate(_)) => format!(
            "{path} was changed here and has not been uploaded, so downloading it again would \
             overwrite your edits. It was left exactly as it is"
        ),
        (Some("ModifiedLocally"), _) => format!(
            "{path} was changed here and has not been uploaded, so freeing its space would lose \
             your edits. It was left exactly as it is"
        ),
        (Some("InUse"), _) => format!(
            "{path} is open in another program, so its space cannot be freed right now. Close \
             it there and try again"
        ),
        // `Failed`, a name this CLI does not know yet, or an error from the
        // bus itself: the detail is all there is, so it is kept whole.
        _ => format!("{} failed: {detail}", action.doing()),
    }
}

/// What each `Skipped()` reason means to the person whose file it is. The
/// same sentences, word for word, as `whyText` in `app/synccontroller.cpp`
/// (each wrapped there in `i18n(...)`); `skip_reason_text_matches_the_windows_wording`
/// below pins each one, and `the_window_uses_the_same_sentences` (in
/// `tests/sync_cli.rs`) checks the C++ source directly, so the two cannot
/// drift apart unnoticed.
pub fn skip_reason_text(reason: &str) -> &'static str {
    match reason {
        "name-too-long" => "The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two).",
        "personal-vault" => "The Personal Vault is locked separately and is not synced.",
        "shared" => "A shared folder added to your OneDrive; shared folders are not synced yet.",
        "onenote" => "A OneNote notebook, which is not a file.",
        "reserved-name" => "The name begins with .konedrive-, which konedrive keeps for itself.",
        _ => "It is neither a file nor a folder konedrive can show.",
    }
}

/// What to tell a person when `TokenExport.ReadOnly()` failed — matched by the
/// D-Bus error name, never the message, the same discipline
/// [`explain_sync_error`] uses for the folder. Being signed out is worth its
/// own sentence, since the fix (`konedrivectl login`) is not what the
/// daemon's own message says; a locked wallet or a network error already
/// says what is wrong on its own, so its text is kept as is.
pub fn explain_dev_error(error: &zbus::Error, prefix: &str) -> String {
    let detail = match error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    };
    dev_refusal_text(error_name(error), &detail, prefix)
}

/// [`explain_dev_error`]'s decision, on the name and message alone — so it
/// can be tested with a name and a message that disagree, the same shape
/// [`refusal_text`] is tested with.
pub fn dev_refusal_text(name: Option<&str>, detail: &str, prefix: &str) -> String {
    let refusal = name.and_then(|name| name.strip_prefix(ERROR_PREFIX)).and_then(|rest| rest.strip_prefix('.'));
    match refusal {
        Some("NotSignedIn") => format!(
            "cannot get an access token: the account is not signed in. Are you signed in? \
             (`{prefix} status` says; `{prefix} login` signs in.)"
        ),
        Some("WritesNotAllowed") => format!(
            "cannot get a read-write access token: while uploads are being developed, only the test \
             accounts listed in write_test_drive_ids in ~/.config/konedrive/config.toml can be \
             read-write, and this account is not one of them. {WITHOUT_READ_WRITE}"
        ),
        Some("ModeNotGranted") => format!(
            "cannot get a read-write access token: the account is read-only. Switch it first: \
             `{prefix} account mode read-write`"
        ),
        _ => format!("cannot get an access token: {detail}"),
    }
}

/// What a refusal of read-write adds: the export without `--read-write` still works.
const WITHOUT_READ_WRITE: &str = "Without --read-write, the export gives a read-only token.";

/// A call on the accounts themselves, for [`explain_account_error`]. (`Remove`
/// forgets a folder, and is a [`SyncAction`].)
#[derive(Debug, Clone, Copy)]
pub enum AccountAction<'a> {
    /// `Accounts.Add`, with the label asked for.
    Add(&'a str),
    /// `Account.SetLabel`: the account's label, and the one asked for.
    Rename(&'a str, &'a str),
    /// `Accounts.SetClientId`: the id given, and the labels of the accounts
    /// signed in or signing in when it was refused.
    SetClientId(&'a str, &'a [String]),
    /// `Account.BeginSignIn`, with the account's label.
    SignIn(&'a str),
    /// `Account.SignOut`, with the account's label.
    SignOut(&'a str),
    /// `Account.SetMode`: the account's label, the mode asked for, and how a command
    /// suggested about the account starts ([`command_prefix`]).
    SetMode(&'a str, &'a str, &'a str),
    /// `Accounts.SetPauseOnMetered` or `SetOnBattery`: a setting every account shares.
    Settings,
}

/// What to tell a person when a call on the accounts failed: `Accounts.Add`,
/// `Accounts.SetClientId`, `Account.SetLabel`, `BeginSignIn` or `SignOut`.
/// These refuse under the bus's own names — `InvalidArgs` for a label or a
/// client id the rules refuse, `Failed` for the rest — so the name decides,
/// and the daemon's message is kept as the reason.
pub fn explain_account_error(action: AccountAction<'_>, error: &zbus::Error) -> String {
    let detail = match error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::MethodError(name, None, _) => name.to_string(),
        other => other.to_string(),
    };
    account_refusal_text(action, error_name(error), &detail)
}

/// [`explain_account_error`]'s decision, on the name and message alone.
pub fn account_refusal_text(action: AccountAction<'_>, name: Option<&str>, detail: &str) -> String {
    use AccountAction::*;
    let invalid = name == Some("org.freedesktop.DBus.Error.InvalidArgs");
    let failed = name == Some("org.freedesktop.DBus.Error.Failed");
    let ours = name.and_then(|name| name.strip_prefix(ERROR_PREFIX)).and_then(|rest| rest.strip_prefix('.'));
    match action {
        SetMode(label, _, _) if ours == Some("WritesNotAllowed") => format!(
            "{label} was not switched to read-write: while uploads are being developed, only the test \
             accounts listed in write_test_drive_ids in ~/.config/konedrive/config.toml can be, and \
             this account is not one of them. Nothing was changed"
        ),
        SetMode(label, _, prefix) if ours == Some("NotSignedIn") => format!(
            "{label} was not switched to read-write: it is not signed in. Sign in first: `{prefix} login`"
        ),
        SetMode(label, _, prefix) if ours == Some("PendingUploads") => format!(
            "{label} was not switched to read-only: {detail}. `{prefix} account mode read-only --force` \
             switches anyway"
        ),
        SetMode(label, mode, _) => format!("{label} was not switched to {mode}: {detail}"),
        Add(label) | Rename(_, label) if invalid => format!(
            "{label:?} cannot be an account's label: {detail}. A label has 1 to 40 characters, no \"/\" \
             and no \"@\", and is not another account's label, whatever the case"
        ),
        SetClientId(id, _) if invalid => format!(
            "{id:?} is not an Application (client) ID. It is a GUID like \
             00000000-0000-0000-0000-000000000000: copy it from the Overview page of your app \
             registration in the Microsoft Entra admin center"
        ),
        // The daemon refuses a change while any account uses the old id.
        SetClientId(_, busy) if failed && !busy.is_empty() => {
            let (who, verb) = match busy {
                [one] => (one.clone(), "is"),
                several => (several.join(", "), "are"),
            };
            let sign_out: Vec<String> =
                busy.iter().map(|label| format!("`konedrivectl --account {} logout`", shell_word(label))).collect();
            format!(
                "the client ID was not changed: every account signs in with it, so it cannot change \
                 while {who} {verb} signed in or signing in. Sign out first: {}",
                sign_out.join(", ")
            )
        }
        SetClientId(..) => format!("the client ID was not saved: {detail}"),
        Add(label) => format!("the account {label:?} was not added: {detail}"),
        Rename(old, new) => format!("the account {old} was not renamed to {new:?}: {detail}"),
        SignIn(label) => format!("cannot start signing in to {label}: {detail}"),
        SignOut(label) => format!("cannot sign {label} out: {detail}"),
        Settings => format!("the setting was not changed: {detail}"),
    }
}

/// Writes `data` to `path` as a brand-new file, atomically and privately.
///
/// A temporary file is created in the same directory as `path`
/// (`O_CREAT | O_EXCL | O_NOFOLLOW`, mode 0600 from the instant it exists —
/// so the name can only be *this* call's own new file, never an existing
/// one and never a symlink), written, `fsync`ed, then renamed over `path`.
/// `rename(2)` replaces whatever `path` names — a symlink there included —
/// by swapping the directory entry to the new inode, rather than writing
/// through whatever `path` used to point to; so a symlink at `path` is
/// *replaced*, never followed and never written through, and anyone who
/// already had the old `path` open keeps reading the old inode's content,
/// completely untouched, for as long as they hold it open. The temporary
/// file is removed on any failure along the way, so a half-written one is
/// never left where an unrelated later read could find it.
///
/// This is what `dev export-access-token` uses to write the token: the
/// symlink case matters because `--out` names a path the person running the
/// command chose, which could already be a symlink (by accident, or by
/// something else's doing) to a file they did not mean to touch.
pub fn write_secret_atomically(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name in the given path")
    })?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp_path = dir.join(&tmp_name);

    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // O_CREAT | O_EXCL: this name is ours alone.
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&tmp_path)
        .and_then(|mut file| {
            file.write_all(data)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp_path, path));

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

/// When the folder was last checked with OneDrive, as `sync status` says it
///: "20 s ago", "5 min ago", "3 h ago", "2 d ago", or "never"
/// for 0. `last` and `now` are unix seconds.
pub fn checked_text(last: i64, now: i64) -> String {
    if last == 0 {
        return "never".to_owned();
    }
    match now - last {
        ago if ago < 0 => "just now".to_owned(),
        ago @ 0..=59 => format!("{ago} s ago"),
        ago @ 60..=3_599 => format!("{} min ago", ago / 60),
        ago @ 3_600..=86_399 => format!("{} h ago", ago / 3_600),
        ago => format!("{} d ago", ago / 86_400),
    }
}

/// Unix seconds now.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `2026-09-24 10:00:05`, in this machine's time zone (`TZ` as the C library
/// reads it).
pub fn local_time(at: i64) -> String {
    let seconds = at as libc::time_t;
    // SAFETY: `tm` is plain data that `localtime_r` fills in; both pointers
    // are to live locals for the length of the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return at.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
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

/// One direction's queue totals (issue #16): `Transfers`' `DownloadLeftCount`,
/// `DownloadLeftBytes`, `DownloadDoneBytes`, `DownloadTimeLeft`, or the same four for uploads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueTotals {
    /// Files (downloads) or changes (uploads) left.
    pub left_count: u32,
    pub left_bytes: u64,
    /// Bytes moved in this run.
    pub done_bytes: u64,
    /// Seconds; 0 when unknown.
    pub time_left: u32,
}

/// What `sync transfers` says first: the files moving each way and the account's transfer
/// pool (`Transfers`' `ActiveDownloads`, `DownloadSpeed`, `ActiveUploads`, `UploadSpeed`,
/// `PoolInUse`, `PoolSize`, `LargeFiles`, `LargeStreams`, `LargeStreamLimit`, `RetryAfter`)
/// and the queue totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferSummary {
    /// Files downloading and uploading now, each once.
    pub active_downloads: u32,
    pub download_speed: u64,
    pub downloads: QueueTotals,
    pub active_uploads: u32,
    pub upload_speed: u64,
    pub uploads: QueueTotals,
    /// Slots held now, and the pool's size: in use may be above the size.
    pub pool_in_use: u32,
    pub pool_size: u32,
    /// Large files the sync moves now; the streams of large sync transfers, and their limit.
    pub large_files: u32,
    pub large_streams: u32,
    pub large_stream_limit: u32,
    /// Seconds left of OneDrive's `Retry-After`; 0 when there is none.
    pub retry_after: u32,
}

/// Reads a [`TransferSummary`] from `Transfers`.
pub async fn transfer_summary(proxy: &FolderProxies<'_>) -> zbus::Result<TransferSummary> {
    Ok(TransferSummary {
        active_downloads: proxy.transfers.active_downloads().await?,
        download_speed: proxy.transfers.download_speed().await?,
        downloads: QueueTotals {
            left_count: proxy.transfers.download_left_count().await?,
            left_bytes: proxy.transfers.download_left_bytes().await?,
            done_bytes: proxy.transfers.download_done_bytes().await?,
            time_left: proxy.transfers.download_time_left().await?,
        },
        active_uploads: proxy.transfers.active_uploads().await?,
        upload_speed: proxy.transfers.upload_speed().await?,
        uploads: QueueTotals {
            left_count: proxy.transfers.upload_left_count().await?,
            left_bytes: proxy.transfers.upload_left_bytes().await?,
            done_bytes: proxy.transfers.upload_done_bytes().await?,
            time_left: proxy.transfers.upload_time_left().await?,
        },
        pool_in_use: proxy.transfers.pool_in_use().await?,
        pool_size: proxy.transfers.pool_size().await?,
        large_files: proxy.transfers.large_files().await?,
        large_streams: proxy.transfers.large_streams().await?,
        large_stream_limit: proxy.transfers.large_stream_limit().await?,
        retry_after: proxy.transfers.retry_after().await?,
    })
}

/// The pool's line, as the window shows it too (issue #50): the slots in use of the pool's
/// size, then the large files and their streams — "Pool: 7 of 32 · large files: 1 (4 of 4
/// streams)" — with "— OneDrive asked to wait 30 s" during a `Retry-After`. In use may be
/// above the size (an open's reserve; slots still held after a throttle halved the pool), and
/// is shown as it is.
pub fn pool_text(summary: &TransferSummary) -> String {
    let mut line = format!(
        "Pool: {} of {} · large files: {} ({} of {} streams)",
        summary.pool_in_use, summary.pool_size, summary.large_files, summary.large_streams, summary.large_stream_limit
    );
    if summary.retry_after > 0 {
        line.push_str(&format!(" — OneDrive asked to wait {} s", summary.retry_after));
    }
    line
}

/// A queue's time left: `about 45 s`, `about 12 min`, `about 2 h 5 min`, `about 3 d 4 h`.
pub fn time_left_text(seconds: u32) -> String {
    let seconds = u64::from(seconds);
    let minutes = seconds.div_ceil(60);
    if seconds < 60 {
        format!("about {seconds} s")
    } else if minutes < 60 {
        format!("about {minutes} min")
    } else if seconds < 86_400 {
        let (h, m) = (minutes / 60, minutes % 60);
        if m == 0 { format!("about {h} h") } else { format!("about {h} h {m} min") }
    } else {
        let hours = seconds.div_ceil(3600);
        let (d, h) = (hours / 24, hours % 24);
        if h == 0 { format!("about {d} d") } else { format!("about {d} d {h} h") }
    }
}

/// One direction's summary line of `sync transfers` (and of `sync outbox`, for uploads):
/// `Downloading: 12 now, 1 234 files left (48.2 GiB, about 12 min), 3.1 GiB done, 8.4 MiB/s`.
/// What is left, and what is done, only while anything is left; the time only when known.
fn direction_line(label: &str, active: u32, totals: &QueueTotals, noun: (&str, &str), speed: u64) -> String {
    let mut line = format!("{label:<12} {active:>2} now, ");
    if totals.left_count > 0 {
        let noun = if totals.left_count == 1 { noun.0 } else { noun.1 };
        line.push_str(&format!("{} {noun} left", grouped(totals.left_count.into())));
        let mut about = Vec::new();
        if totals.left_bytes > 0 {
            about.push(human_bytes(totals.left_bytes));
        }
        if totals.time_left > 0 {
            about.push(time_left_text(totals.time_left));
        }
        if !about.is_empty() {
            line.push_str(&format!(" ({})", about.join(", ")));
        }
        line.push_str(&format!(", {} done, ", human_bytes(totals.done_bytes)));
    }
    line.push_str(&format!("{}/s", human_bytes(speed)));
    line
}

/// The `Downloading:` summary line.
pub fn downloading_line(summary: &TransferSummary) -> String {
    direction_line("Downloading:", summary.active_downloads, &summary.downloads, ("file", "files"), summary.download_speed)
}

/// The `Uploading:` summary line: what is left to upload is counted in changes.
pub fn uploading_line(summary: &TransferSummary) -> String {
    direction_line("Uploading:", summary.active_uploads, &summary.uploads, ("change", "changes"), summary.upload_speed)
}

/// `sync status`'s `Waiting to download:` line: `1 234 files (48.2 GiB)`.
pub fn waiting_download_text(count: u32, bytes: u64) -> String {
    match count {
        0 => "nothing".to_owned(),
        1 => format!("1 file ({})", human_bytes(bytes)),
        n => format!("{} files ({})", grouped(n.into()), human_bytes(bytes)),
    }
}

/// `sync transfers`: how many files go each way now, what is left and done, how fast, and
/// the pool; then one line per download and upload under way — its direction, path, how
/// far, and the whole size.
pub fn transfers_text(summary: &TransferSummary, downloads: &[(String, u64, u64)], uploads: &[(String, u64, u64)]) -> String {
    let mut out = format!("{}\n{}\n{}\n", downloading_line(summary), uploading_line(summary), pool_text(summary));
    if downloads.is_empty() && uploads.is_empty() {
        out.push_str("Nothing is downloading or uploading.\n");
        return out;
    }
    let lines = downloads.iter().map(|t| ("down", t)).chain(uploads.iter().map(|t| ("up", t)));
    for (direction, (path, done, total)) in lines {
        let percent = if *total == 0 { 0 } else { done.saturating_mul(100) / total };
        out.push_str(&format!("{direction:<4} {path}  {percent}%  {}\n", human_bytes(*total)));
    }
    out
}

/// One row of `Conflicts.List()`: (unix time, original full path, full path
/// of the kept version, how it was kept: `rescued` or `copy`).
pub type ConflictRow = (i64, String, String, String);

/// `sync conflicts`: each local version kept, where it was and where it is
/// now, and when: moved out of the way, or — in a read-write folder — kept as
/// a copy beside OneDrive's.
pub fn conflicts_text(conflicts: &[ConflictRow]) -> String {
    if conflicts.is_empty() {
        return "No conflicts.\n".to_owned();
    }
    let mut out = String::new();
    for (at, original, rescued, kind) in conflicts {
        if kind == "copy" {
            out.push_str(&format!(
                "{original}\n    changed here and in OneDrive: yours is kept beside it as {rescued}, {}\n",
                local_time(*at)
            ));
        } else {
            out.push_str(&format!("{original}\n    moved to {rescued} on {}\n", local_time(*at)));
        }
    }
    out
}

/// The directories the files `conflicts` lists were rescued to: for each, the rescued path
/// with the original's place in `folder` taken off its end (`rescued/<id>/<time>`, or a
/// directory beside a folder on another filesystem), or the file's own directory when that
/// cannot be told. Each once, in the order first met.
pub fn rescue_dirs(folder: &str, conflicts: &[ConflictRow]) -> Vec<String> {
    use std::path::Path;
    let mut dirs: Vec<String> = Vec::new();
    // A copy is in the folder, beside its original, and stays with it.
    for (_, original, rescued, _) in conflicts.iter().filter(|c| c.3 != "copy") {
        let rescued = Path::new(rescued);
        let within = if folder.is_empty() { None } else { Path::new(original).strip_prefix(folder).ok() };
        let dir = match within {
            Some(within) if within.components().next().is_some() && rescued.ends_with(within) => {
                let mut dir = rescued.to_path_buf();
                for _ in within.components() {
                    dir.pop();
                }
                dir
            }
            _ => rescued.parent().map(Path::to_path_buf).unwrap_or_default(),
        };
        let dir = dir.display().to_string();
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// What `account remove` says it did: what was deleted, and what was kept — the folder, as
/// it is, and the files the conflicts list named (read before the removal), by where they
/// were rescued to. Rescued files are never deleted; where the rescues of conflicts
/// dismissed earlier went cannot be told from here (the data directory, beside a folder on
/// another filesystem, or a migrated account's older ones), so only that they stay is said.
pub fn removed_text(label: &str, folder: &str, conflicts: &[ConflictRow]) -> String {
    let mut out = format!(
        "Removed the account {label}: it is signed out, and its token, cached name and quota, list of \
         OneDrive items, activity and conflicts list are deleted.\n"
    );
    if folder.is_empty() {
        out.push_str("It had no folder.\n");
    } else {
        out.push_str(&format!(
            "Kept: the folder {folder}, as it is. A file in it that was never downloaded stays as an empty \
             placeholder, which reads as zeros.\n"
        ));
    }
    let rescued = conflicts.iter().filter(|c| c.3 != "copy").count();
    let files = if rescued == 1 { "1 file".to_owned() } else { format!("{rescued} files") };
    match rescue_dirs(folder, conflicts).as_slice() {
        [] => {}
        [one] => out.push_str(&format!("Kept: the {files} the conflicts list named, rescued in {one}\n")),
        several => {
            out.push_str(&format!("Kept: the {files} the conflicts list named, rescued in:\n"));
            for dir in several {
                out.push_str(&format!("  {dir}\n"));
            }
        }
    }
    if !folder.is_empty() || !conflicts.is_empty() {
        out.push_str("Rescued files are never deleted: any from conflicts dismissed earlier stay where they were moved.\n");
    }
    out
}

/// `sync free-up-space`: "Freed N files (X). M files were in use and kept."
pub fn free_up_text(files: u32, bytes: u64, busy: u32) -> String {
    if files == 0 && busy == 0 {
        return "Nothing was downloaded, so there was nothing to free up.".to_owned();
    }
    let mut out = format!("Freed {files} {} ({}).", if files == 1 { "file" } else { "files" }, human_bytes(bytes));
    if busy > 0 {
        let were = if busy == 1 { "file was" } else { "files were" };
        out.push_str(&format!(" {busy} {were} in use and kept."));
    }
    out
}

/// `sync pin`: that the paths are kept on this device, and how many of their
/// files are downloading now.
pub fn pin_text(queued: u32, prefix: &str) -> String {
    match queued {
        0 => "Kept on this device. Everything in it is here already.".to_owned(),
        1 => format!("Kept on this device. 1 file is downloading (`{prefix} sync transfers`)."),
        n => format!("Kept on this device. {n} files are downloading (`{prefix} sync transfers`)."),
    }
}

/// The path a `NotAllowed` refusal is about, and what pins it: the daemon
/// says exactly "<path> is pinned by <folder>: unpin it first", and the
/// folder is the path or one above it — which tells the two apart even when
/// a name holds " is pinned by " itself.
fn pinned_parts(detail: &str) -> Option<(&str, &str)> {
    const BY: &str = " is pinned by ";
    let both = detail.strip_suffix(": unpin it first")?;
    both.match_indices(BY)
        .map(|(at, _)| (&both[..at], &both[at + BY.len()..]))
        .find(|(path, by)| std::path::Path::new(path).starts_with(by))
}

/// The one path a `NotAllowed` or `NotUploaded` refusal of a call on several
/// is about. `None` for any other error.
pub fn refused_path(error: &zbus::Error) -> Option<&str> {
    let zbus::Error::MethodError(name, Some(detail), _) = error else { return None };
    refused_path_of(name.as_str(), detail)
}

/// [`refused_path`], on the error's name and message.
fn refused_path_of<'a>(name: &str, detail: &'a str) -> Option<&'a str> {
    match name.strip_prefix(ERROR_PREFIX) {
        Some(".NotAllowed") => pinned_parts(detail).map(|(path, _)| path),
        // "<path> is not uploaded yet, so freeing it up would lose …"
        Some(".NotUploaded") => detail.split_once(" is not uploaded yet").map(|(path, _)| path),
        _ => None,
    }
}

/// `sync unpin`: how many pins came off; the files stay.
pub fn unpin_text(unpinned: u32) -> String {
    match unpinned {
        0 => "Nothing here had a pin of its own, so nothing changed.".to_owned(),
        1 => "No longer kept on this device. What is downloaded stays; `konedrivectl sync free` frees it.".to_owned(),
        n => format!(
            "{n} items are no longer kept on this device. What is downloaded stays; `konedrivectl sync free` frees it."
        ),
    }
}

/// `sync free`: what was freed, what was kept because it was in use or
/// changed here (FreeUp's `busy` counts both), and the downloaded files a pin
/// of their own — or of a folder below the one freed — kept.
pub fn free_text(files: u32, bytes: u64, busy: u32, pinned: u32) -> String {
    if files == 0 && busy == 0 && pinned == 0 {
        return "Nothing here was downloaded, so there was nothing to free up.".to_owned();
    }
    let mut out = if files == 0 {
        "Nothing was freed up.".to_owned()
    } else {
        format!("Freed {files} {} ({}).", if files == 1 { "file" } else { "files" }, human_bytes(bytes))
    };
    if busy > 0 {
        let were = if busy == 1 { "file was" } else { "files were" };
        out.push_str(&format!(" {busy} {were} in use or changed here, and kept."));
    }
    if pinned > 0 {
        let were = if pinned == 1 { "file was" } else { "files were" };
        out.push_str(&format!(" {pinned} {were} kept: a file or folder below is kept on this device."));
    }
    out
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Polls `proxy` until `SetMode("read-write")`'s sign-in has ended: `Ok` once `Mode` is
/// `read-write`, `Err` with `LastError` when the switch did not go through. The account stays
/// `signed-in` throughout, so `State` cannot tell; it is polled for the same reason
/// [`wait_for_sign_in`] polls.
pub async fn wait_for_read_write(proxy: &AccountProxy<'_>) -> anyhow::Result<()> {
    loop {
        if proxy.mode().await? == "read-write" {
            return Ok(());
        }
        // `SetMode` clears `LastError` before it answers, so anything in it now is why the
        // switch did not go through. Cancelled, it says nothing: the caller stops waiting.
        let last_error = proxy.last_error().await?;
        if !last_error.is_empty() {
            anyhow::bail!("the account stays read-only: {last_error}");
        }
        if proxy.state().await? != "signed-in" {
            anyhow::bail!("the account was signed out; it stays read-only");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Polls `proxy` until the account leaves the `signing-in` state, then reports the outcome.
///
/// Polling (rather than watching the `StateChanged` signal) sidesteps a coalescing hazard:
/// the daemon publishes state through a `tokio::watch` channel, so a fast transition (e.g.
/// `signing-in` -> `signed-in` completing almost immediately, as with an SSO session) can be
/// collapsed and observed only as its final value. A waiter that only starts counting once it
/// has *seen* `signing-in` on the signal stream could then wait forever for a change that
/// already happened. `BeginSignIn` sets the state to `signing-in` before it replies, so the
/// very first poll here is guaranteed to observe either `signing-in` or the terminal state -
/// there is no window in which the relevant transition can be missed.
pub async fn wait_for_sign_in(proxy: &AccountProxy<'_>) -> anyhow::Result<()> {
    loop {
        let state = proxy.state().await?;
        if state != "signing-in" {
            return if state == "signed-in" {
                Ok(())
            } else {
                let last_error = proxy.last_error().await?;
                if last_error.is_empty() {
                    anyhow::bail!("sign-in was cancelled");
                } else {
                    anyhow::bail!("sign-in did not complete: {last_error}");
                }
            };
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests;
