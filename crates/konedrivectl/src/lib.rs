//! The testable part of `konedrivectl`.
//!
//! The daemon serves one object per account (`konedrive_dbus::account_path`) below the
//! accounts manager (`konedrive_dbus::ACCOUNTS_PATH`). A command acts on one account, chosen
//! as [`choose`] says; `status` and `sync status` show every account when none is chosen; the
//! commands that take a path go through `Files1`, which finds the account by the path.

use std::time::Duration;

use konedrive_dbus::accounts::{Account1Proxy, Sync1Proxy};
use konedrive_dbus::{error_name, ERROR_PREFIX};
use zbus::zvariant::OwnedObjectPath;

/// The label `konedrivectl login` gives the account it adds when there is none, as the
/// daemon names the account it migrates from a single-account configuration.
pub const FIRST_LABEL: &str = "Personal";

/// The environment variable that chooses the account when `--account` is not given.
pub const ACCOUNT_VARIABLE: &str = "KONEDRIVE_ACCOUNT";

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
    /// `RootPath`: empty with no folder.
    pub folder: String,
    pub root_state: String,
}

/// `account list` (design §5.2): a table of every account, in the order they were added —
/// id, label, email, sign-in state, mode, and the folder with its `RootState`.
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

/// `status`'s `Client ID:` line: `Accounts1.ClientId`, one for every account.
pub fn client_id_line(client_id: &str) -> String {
    let shown = if client_id.is_empty() { "(not set)" } else { client_id };
    format!("{:<12}{shown}\n", "Client ID:")
}

/// `status` for one account. `client_id` is `Some` when this account is all `status` shows:
/// its label and the client ID are printed with it. When `status` shows several, the client
/// ID is printed once above them and each block is headed by its label: `None`.
pub async fn status_text(proxy: &Account1Proxy<'_>, client_id: Option<&str>) -> zbus::Result<String> {
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
/// `RootState` is `none` on an ordinary machine that has never registered a
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
/// `Helper:` says how the privileged helper stands (`Accounts1.HelperState`,
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
pub async fn sync_status_text(proxy: &Sync1Proxy<'_>, helper: Option<&str>, prefix: &str) -> zbus::Result<String> {
    const W: usize = SYNC_STATUS_WIDTH;
    let path = proxy.root_path().await?;
    let state = proxy.root_state().await?;
    let shown = if path.is_empty() { "(none)" } else { path.as_str() };
    let mut out = format!("{:<W$}{shown}\n", "Folder:");
    out.push_str(&format!("{:<W$}{state}\n", "State:"));
    if let Some(opens) = opens_line(&state) {
        out.push_str(&format!("{:<W$}{opens}\n", "Opens:"));
    }
    if let Some(helper) = helper {
        out.push_str(&helper_line(helper));
    }
    let last_error = proxy.last_error().await?;
    if !last_error.is_empty() {
        out.push_str(&format!("{:<W$}{last_error}\n", "Last error:"));
    }
    if proxy.root_source().await? == "onedrive" {
        let (listed, placed, skipped) =
            (proxy.items_listed().await?, proxy.items_placed().await?, proxy.skipped_count().await?);
        out.push_str(&format!("{:<W$}{listed} in OneDrive, {placed} in the folder\n", "Items:"));
        if skipped > 0 {
            out.push_str(&format!("{:<W$}{skipped} (see `{prefix} sync skipped`)\n", "Skipped:"));
        }
        let checked = checked_text(proxy.last_checked().await?, unix_now());
        out.push_str(&format!("{:<W$}{checked}\n", "Last checked:"));
        let mode = account_mode(proxy).await;
        out.push_str(&format!("{:<W$}{}\n", "Mode:", mode_text(&mode)));
        let (pending, bytes, blocked) = (proxy.pending_count().await?, proxy.pending_bytes().await?, proxy.blocked_count().await?);
        if mode == "read-write" || pending > 0 || blocked > 0 {
            out.push_str(&format!("{:<W$}{}\n", "Waiting to upload:", waiting_text(pending, bytes)));
        }
        if blocked > 0 {
            out.push_str(&format!("{:<W$}{blocked} (see `{prefix} sync not-uploaded`)\n", "Blocked:"));
        }
        let held = proxy.held_count().await?;
        if held > 0 {
            out.push_str(&format!(
                "{:<W$}{held} deletions (`{prefix} sync deletes confirm` or `{prefix} sync deletes restore`)\n",
                "Held for confirmation:"
            ));
        }
        if proxy.paused().await? {
            let until = proxy.paused_until().await?;
            out.push_str(&format!("{:<W$}{}\n", "Paused until:", paused_text(until, prefix)));
        }
    }
    if !path.is_empty() {
        out.push_str(&format!("{:<W$}{}\n", "On this computer:", human_bytes(proxy.local_bytes().await?)));
        out.push_str(&format!("{:<W$}{}\n", "Always on this device:", proxy.pinned_count().await?));
    }
    let conflicts = proxy.conflict_count().await?;
    if conflicts > 0 {
        out.push_str(&format!("{:<W$}{conflicts} (see `{prefix} sync conflicts`)\n", "Conflicts:"));
    }
    Ok(out)
}

/// The width of `sync status`'s label column: `Always on this device:` is the
/// longest label.
const SYNC_STATUS_WIDTH: usize = 24;

/// `Account1.Mode` of the account whose `Sync1` is `proxy` (the same object);
/// empty when it cannot be read.
async fn account_mode(proxy: &Sync1Proxy<'_>) -> String {
    let inner = proxy.inner();
    let account = async {
        konedrive_dbus::accounts::Account1Proxy::builder(inner.connection())
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

/// `sync status`'s `Waiting to upload:` line: `3 files (1.5 MiB)`.
pub fn waiting_text(count: u32, bytes: u64) -> String {
    match count {
        0 => "nothing".to_owned(),
        1 => format!("1 change ({})", human_bytes(bytes)),
        n => format!("{n} changes ({})", human_bytes(bytes)),
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
        "forbidden" => "this sign-in does not allow uploads: sign in again".to_owned(),
        "open-for-writing" => "open for writing in another program: it goes up once closed".to_owned(),
        "mass-delete" => "part of a large delete: confirm it (`sync deletes confirm`) or undo it (`sync deletes restore`)".to_owned(),
        "symlink" => "a symbolic link: never uploaded".to_owned(),
        "fifo" | "socket" | "device" => "not a file or a folder: never uploaded".to_owned(),
        "reserved-name" => "a .konedrive- name, which the daemon keeps for itself: never uploaded".to_owned(),
        "not-downloaded" => "a file from another OneDrive folder that is not downloaded here".to_owned(),
        "other-device" => "on another filesystem mounted inside the folder: never uploaded".to_owned(),
        "hard-link" => "a file with other hard links: not uploaded".to_owned(),
        "locked" => "locked in OneDrive (open for co-authoring): tried again later".to_owned(),
        other => match other.strip_prefix("refused: ") {
            Some(message) => format!("OneDrive refused it: {message}"),
            None => other.to_owned(),
        },
    }
}

/// One row of `Sync1.Outbox()`: (seq, kind, full path, state, bytes sent, bytes
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

/// `sync not-uploaded`: what stays on this computer, and why.
pub fn not_uploaded_text(items: &[(String, String)]) -> String {
    if items.is_empty() {
        return "Everything here is uploaded or waits to be.\n".to_owned();
    }
    let mut out = String::new();
    for (path, reason) in items {
        out.push_str(&format!("{path}\n    {}\n", upload_reason_text(reason)));
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
    /// `account remove`, with the account's label: `Accounts1.Remove` forgets the
    /// folder as `Forget` does, and is refused under the same names.
    Remove(&'a str),
    Outbox,
    Pause,
    Resume,
    Ignore,
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
            Self::Remove(label) => format!("removing the account {label}"),
            Self::Outbox => "listing the changes waiting to upload".to_owned(),
            Self::Pause => "pausing the sync".to_owned(),
            Self::Resume => "resuming the sync".to_owned(),
            Self::Ignore => "changing the ignore list".to_owned(),
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
            | Self::Free(path) => path,
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
            | Self::NotUploaded
            | Self::Deletes => "",
        }
    }
}

/// What to tell a person when a `Sync1` call made for `action` failed.
///
/// Matches the D-Bus error **name** (`konedrive_dbus::error_name`), never the
/// message: every refusal `Sync1` makes arrives under
/// `konedrive_dbus::ERROR_PREFIX`, and each gets a sentence saying what
/// happened to the user's file and what they can do about it. `root` is the
/// registered folder (`RootPath`, possibly empty), which two refusals name.
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
/// folder of the account the refusal is about (`RootPath`: the chosen
/// account's, or for a path, the folder that holds it), what it shows
/// (`RootSource`), the helper (`Accounts1.HelperState`), and for a path, every
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
    let path_command = matches!(action, Hydrate(_) | Dehydrate(_) | Pin(_) | Unpin(_) | Free(_));
    // An account removed while this command ran: its object is gone.
    if name.is_some_and(is_gone_name) && !path_command {
        let text = refusal_text_as(action, Some(&format!("{ERROR_PREFIX}.NoAccount")), detail, context.root, prefix);
        return text;
    }
    match (refusal, action) {
        // `Files1` refuses a path in no account's folder before any account
        // sees it; with no folder at all, that is `NoRoot`'s situation.
        (Some("OutsideRoot"), _) if path_command && context.root.is_empty() => {
            return match context.folders {
                [] => refusal_text_as(action, Some(&format!("{ERROR_PREFIX}.NoRoot")), detail, "", prefix),
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
        (Some("NotSignedIn"), _) => format!(
            "the account is not signed in, and `{prefix} sync register` binds the folder to the \
             account's OneDrive. Sign in first with `{prefix} login` — or, in the developer's mode \
             with local files, use `{prefix} sync register-without-interception {path}`: without \
             the helper, files that are not downloaded read as zeros until you hydrate them"
        ),
        // `Accounts1.Remove` forgets the folder first, under Forget's rule.
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

/// What to tell a person when `Dev1.AccessToken()` failed — matched by the
/// D-Bus error name, never the message, the same discipline
/// [`explain_sync_error`] uses for `Sync1`. Being signed out is worth its
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
    /// `Accounts1.Add`, with the label asked for.
    Add(&'a str),
    /// `Account1.SetLabel`: the account's label, and the one asked for.
    Rename(&'a str, &'a str),
    /// `Accounts1.SetClientId`: the id given, and the labels of the accounts
    /// signed in or signing in when it was refused.
    SetClientId(&'a str, &'a [String]),
    /// `Account1.BeginSignIn`, with the account's label.
    SignIn(&'a str),
    /// `Account1.SignOut`, with the account's label.
    SignOut(&'a str),
    /// `Account1.SetMode`: the account's label, the mode asked for, and how a command
    /// suggested about the account starts ([`command_prefix`]).
    SetMode(&'a str, &'a str, &'a str),
}

/// What to tell a person when a call on the accounts failed: `Accounts1.Add`,
/// `Accounts1.SetClientId`, `Account1.SetLabel`, `BeginSignIn` or `SignOut`.
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

/// `sync transfers`: one line per download and upload under way — its
/// direction, path, how far, and the whole size.
pub fn transfers_text(downloads: &[(String, u64, u64)], uploads: &[(String, u64, u64)]) -> String {
    if downloads.is_empty() && uploads.is_empty() {
        return "Nothing is downloading or uploading.\n".to_owned();
    }
    let mut out = String::new();
    let lines = downloads.iter().map(|t| ("down", t)).chain(uploads.iter().map(|t| ("up", t)));
    for (direction, (path, done, total)) in lines {
        let percent = if *total == 0 { 0 } else { done.saturating_mul(100) / total };
        out.push_str(&format!("{direction:<4} {path}  {percent}%  {}\n", human_bytes(*total)));
    }
    out
}

/// One row of `Sync1.Conflicts()`: (unix time, original full path, full path
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
pub async fn wait_for_read_write(proxy: &Account1Proxy<'_>) -> anyhow::Result<()> {
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
pub async fn wait_for_sign_in(proxy: &Account1Proxy<'_>) -> anyhow::Result<()> {
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
mod tests {
    use super::{
        account_refusal_text, choose, command_prefix, dev_refusal_text, human_bytes, outbox_text, parse_duration,
        refusal_text, refusal_text_in, removed_text, rescue_dirs, shell_word, skip_reason_text, upload_reason_text,
        write_secret_atomically, AccountAction, AccountInfo,
        Context, NoChoice, Source, SyncAction,
    };

    /// `account remove` and `sync forget` refused while changes wait say how
    /// to see them and how to drop them.
    #[test]
    fn a_remove_or_forget_refused_while_changes_wait_says_what_to_do() {
        let detail = "2 change(s) made here have not been uploaded yet";
        let removed = refusal_text(SyncAction::Remove("Test"), Some("org.konedrive.Error.PendingUploads"), detail, "/home/u/OneDrive");
        assert!(removed.contains("was not removed") && removed.contains(detail), "{removed}");
        assert!(removed.contains("`konedrivectl --account Test account mode read-only --force`"), "{removed}");
        let forgot = refusal_text(SyncAction::Forget, Some("org.konedrive.Error.PendingUploads"), detail, "/home/u/OneDrive");
        assert!(forgot.contains("still registered") && forgot.contains("`konedrivectl account mode read-only --force`"), "{forgot}");
    }

    /// `account mode` and `export-access-token --read-write` explain each refusal by its
    /// name (`docs/design/writes.md` §11): the gate, uploads waiting, a read-only account.
    #[test]
    fn a_refused_mode_is_explained_by_its_name() {
        let prefix = "konedrivectl --account Test";
        let pending = account_refusal_text(
            AccountAction::SetMode("Test", "read-only", prefix),
            Some("org.konedrive.Error.PendingUploads"),
            "3 changes made here have not been uploaded yet",
        );
        assert!(pending.contains("3 changes") && pending.contains("konedrivectl --account Test account mode read-only --force"), "{pending}");
        let signed_out = account_refusal_text(AccountAction::SetMode("Test", "read-write", prefix), Some("org.konedrive.Error.NotSignedIn"), "x");
        assert!(signed_out.contains("`konedrivectl --account Test login`"), "{signed_out}");
        let other = account_refusal_text(AccountAction::SetMode("Test", "read-write", prefix), Some("org.konedrive.Error.Failed"), "no client id");
        assert_eq!(other, "Test was not switched to read-write: no client id");
        let read_only = dev_refusal_text(Some("org.konedrive.Error.ModeNotGranted"), "read-only", prefix);
        assert!(read_only.contains("`konedrivectl --account Test account mode read-write`"), "{read_only}");
        let gate = dev_refusal_text(Some("org.konedrive.Error.WritesNotAllowed"), "x", prefix);
        assert!(gate.contains("write_test_drive_ids") && gate.contains("Without --read-write"), "{gate}");
    }

    fn account(id: &str, label: &str, email: &str) -> AccountInfo {
        AccountInfo {
            path: konedrive_dbus::account_path(id).unwrap(),
            id: id.into(),
            label: label.into(),
            email: email.into(),
        }
    }

    /// Design §5.1: an exact id, else a label, else an email, the last two
    /// whatever the case; with no name, the only account; several and no
    /// name is a mistake on the command line (exit status 2).
    #[test]
    fn an_account_is_chosen_by_id_label_or_email() {
        let accounts =
            [account("3f9a1c0e5b7d", "Personal", "ann@outlook.com"), account("8c21d07a44e1", "Family", "")];
        let chosen = |name: &str| choose(&accounts, Some((name, Source::Option))).map(|a| a.label.as_str());
        assert_eq!(chosen("8c21d07a44e1"), Ok("Family"));
        assert_eq!(chosen("family"), Ok("Family"));
        assert_eq!(chosen("ANN@outlook.com"), Ok("Personal"));
        let unknown = chosen("nobody").unwrap_err();
        assert_eq!(unknown.exit_status(), 2);
        assert!(unknown.to_string().contains("Personal, Family"), "{unknown}");

        let several = choose(&accounts, None).unwrap_err();
        assert_eq!(several, NoChoice::Several { labels: vec!["Personal".into(), "Family".into()] });
        assert_eq!(several.to_string(), "Several accounts: choose one with --account (Personal, Family)");
        assert_eq!(several.exit_status(), 2);
        assert_eq!(choose(&accounts[..1], None).unwrap().label, "Personal");
        assert_eq!(choose(&[], None).unwrap_err().exit_status(), 1);
        assert!(choose(&[], None).unwrap_err().to_string().contains("konedrivectl account add <label>"));
    }

    /// A name that fits two accounts — one's id and the other's label, or one label twice in a
    /// hand-edited `config.toml` — is refused with exit status 2, listing both, never taken
    /// as the first: `account remove` asks nothing.
    #[test]
    fn a_name_that_fits_two_accounts_is_refused() {
        let accounts = [account("3f9a1c0e5b7d", "Personal", ""), account("8c21d07a44e1", "3f9a1c0e5b7d", "")];
        let refused = choose(&accounts, Some(("3f9a1c0e5b7d", Source::Argument))).unwrap_err();
        assert_eq!(refused.exit_status(), 2);
        let said = refused.to_string();
        assert!(said.contains("Personal (3f9a1c0e5b7d), 3f9a1c0e5b7d (8c21d07a44e1)"), "{said}");
        assert_eq!(choose(&accounts, Some(("8c21d07a44e1", Source::Option))).unwrap().label, "3f9a1c0e5b7d");

        let twice = [account("3f9a1c0e5b7d", "Personal", ""), account("8c21d07a44e1", "personal", "")];
        assert!(matches!(choose(&twice, Some(("PERSONAL", Source::Option))), Err(NoChoice::Ambiguous { .. })));
    }

    /// Named from the environment with no account at all: exit status 1, naming the variable.
    #[test]
    fn a_name_with_no_account_at_all_says_where_it_came_from() {
        let refused = choose(&[], Some(("Test", Source::Environment))).unwrap_err();
        assert_eq!(refused.exit_status(), 1);
        assert!(refused.to_string().starts_with("KONEDRIVE_ACCOUNT names the account \"Test\""), "{refused}");
    }

    /// A suggested command names the account whenever the bare one could act on another.
    #[test]
    fn a_suggested_command_names_the_account_when_it_has_to() {
        assert_eq!(command_prefix(Some("Family"), false, false), "konedrivectl");
        assert_eq!(command_prefix(Some("Family"), true, false), "konedrivectl --account Family");
        assert_eq!(command_prefix(Some("My Home"), false, true), "konedrivectl --account 'My Home'");
        assert_eq!(command_prefix(None, true, false), "konedrivectl --account <account>");
        let context = Context { prefix: "konedrivectl --account Family", ..Context::default() };
        let text = refusal_text_in(
            SyncAction::Register("/home/u/Other"),
            Some("org.konedrive.Error.AlreadyRegistered"),
            "",
            Context { root: "/home/u/Family", ..context },
        );
        assert!(text.contains("run `konedrivectl --account Family sync forget` first"), "{text}");
    }

    /// `account remove` names where the listed conflicts' files were rescued, and nothing it
    /// cannot know.
    #[test]
    fn removal_says_where_the_listed_rescues_are() {
        let rescued = |original: &str, kept: &str| (0, original.to_owned(), kept.to_owned(), "rescued".to_owned());
        let conflicts = [
            rescued("/home/u/OneDrive/docs/a.txt", "/data/rescued/id/t1/docs/a.txt"),
            rescued("/home/u/OneDrive/b.txt", "/home/u/.konedrive-rescued-OneDrive/t2/b.txt"),
            rescued("/home/u/OneDrive/c.txt", "/data/rescued/id/t1/c.txt"),
            (0, "/home/u/OneDrive/d.txt".to_owned(), "/home/u/OneDrive/d-fedora.txt".to_owned(), "copy".to_owned()),
        ];
        let dirs = rescue_dirs("/home/u/OneDrive", &conflicts);
        assert_eq!(dirs, ["/data/rescued/id/t1", "/home/u/.konedrive-rescued-OneDrive/t2"], "a copy stays in the folder");
        let listed = super::conflicts_text(&conflicts);
        assert!(listed.contains("moved to /data/rescued/id/t1/c.txt"), "{listed}");
        assert!(listed.contains("changed here and in OneDrive: yours is kept beside it as /home/u/OneDrive/d-fedora.txt"), "{listed}");
        let said = removed_text("Home", "", &[]);
        assert!(said.contains("It had no folder.") && !said.contains("Rescued"), "{said}");
    }

    #[test]
    fn a_label_is_quoted_for_the_shell_only_when_it_has_to_be() {
        assert_eq!(shell_word("Family"), "Family");
        assert_eq!(shell_word("Семья"), "Семья");
        assert_eq!(shell_word("Ann's work"), r"'Ann'\''s work'");
    }

    /// `Files1` refuses a path in no account's folder `OutsideRoot`: with no
    /// folder at all, that is said as `NoRoot` is; with folders, they are named.
    #[test]
    fn a_path_in_no_folder_is_told_which_folders_there_are() {
        let outside = |folders: &[String]| {
            let context = Context { folders, ..Context::default() };
            refusal_text_in(SyncAction::Hydrate("/tmp/x"), Some("org.konedrive.Error.OutsideRoot"), "", context)
        };
        assert!(outside(&[]).contains("no sync folder is registered"), "{}", outside(&[]));
        let two = ["/home/u/OneDrive".to_owned(), "/home/u/Family".to_owned()];
        let text = outside(&two);
        assert!(text.contains("/home/u/OneDrive, /home/u/Family"), "{text}");
    }

    /// Pins every `Skipped()` reason's sentence — the same wording as the
    /// window's `whyText` (`app/synccontroller.cpp`), which is what keeps a
    /// user reading the same explanation from `konedrivectl sync skipped`
    /// and from the window regardless of which one they happen to use.
    #[test]
    fn skip_reason_text_matches_the_windows_wording() {
        assert_eq!(
            skip_reason_text("name-too-long"),
            "The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two)."
        );
        assert_eq!(
            skip_reason_text("personal-vault"),
            "The Personal Vault is locked separately and is not synced."
        );
        assert_eq!(
            skip_reason_text("shared"),
            "A shared folder added to your OneDrive; shared folders are not synced yet."
        );
        assert_eq!(skip_reason_text("onenote"), "A OneNote notebook, which is not a file.");
        assert_eq!(
            skip_reason_text("reserved-name"),
            "The name begins with .konedrive-, which konedrive keeps for itself."
        );
        assert_eq!(
            skip_reason_text("unsupported"),
            "It is neither a file nor a folder konedrive can show."
        );
        assert_eq!(
            skip_reason_text("something-nobody-invented-yet"),
            "It is neither a file nor a folder konedrive can show."
        );
    }

    /// Being signed out gets its own sentence, distinct from a locked wallet
    /// or a network error, which keep the daemon's own message: the fix for
    /// each is different, so folding them into one generic sentence would
    /// hide which one applies.
    #[test]
    fn dev_refusal_text_distinguishes_signed_out_from_other_failures() {
        let signed_out = dev_refusal_text(Some("org.konedrive.Error.NotSignedIn"), "nobody is signed in", "konedrivectl");
        assert!(signed_out.to_lowercase().contains("signed in"), "{signed_out}");

        let locked = dev_refusal_text(Some("org.konedrive.Error.Failed"), "secret storage is locked", "konedrivectl");
        assert!(locked.contains("secret storage is locked"), "{locked}");
        assert!(
            !locked.to_lowercase().contains("are you signed in"),
            "a locked wallet is not the same thing as being signed out: {locked}"
        );

        let network = dev_refusal_text(
            Some("org.konedrive.Error.Failed"),
            "Microsoft rejected the token refresh: invalid_client: bad request",
            "konedrivectl",
        );
        assert!(network.contains("Microsoft rejected the token refresh"), "{network}");

        // A name from outside `org.konedrive.Error` is not mistaken for
        // `NotSignedIn` just because the detail happens to mention signing in.
        let bus_error = dev_refusal_text(Some("org.freedesktop.DBus.Error.NoReply"), "no reply", "konedrivectl");
        assert!(!bus_error.to_lowercase().contains("are you signed in"), "{bus_error}");
    }

    #[test]
    fn write_secret_atomically_creates_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("token");
        write_secret_atomically(&out, b"AT-1").unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "AT-1");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
        // No temporary file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "token")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// The heart of I1: `rename(2)` replaces the symlink itself, so its
    /// target is never opened, truncated, or written through.
    #[test]
    fn write_secret_atomically_replaces_a_symlink_without_touching_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real-file");
        std::fs::write(&target, b"do not touch").unwrap();
        let link = dir.path().join("out-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_secret_atomically(&link, b"AT-2").unwrap();

        assert!(
            !std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
            "the link must be replaced by a regular file, not written through"
        );
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "AT-2");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "do not touch", "the old target is untouched");
    }

    /// The other half of I1: an fd opened before the export keeps reading
    /// the old inode's content — `rename(2)` never truncates it in place.
    #[test]
    fn write_secret_atomically_does_not_disturb_a_reader_of_the_old_file() {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("token");
        std::fs::write(&out, b"old-content").unwrap();
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut held_open = std::fs::File::open(&out).unwrap();

        write_secret_atomically(&out, b"AT-3").unwrap();

        assert_eq!(std::fs::read_to_string(&out).unwrap(), "AT-3");
        assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
        let mut still_reads = String::new();
        held_open.read_to_string(&mut still_reads).unwrap();
        assert_eq!(still_reads, "old-content", "an fd opened before the export keeps its own inode");
    }

    /// The name decides, never the message. A refusal named
    /// `ModifiedLocally` whose message happens to read like `NotHydrated`'s
    /// must still be explained as edits that would be lost — matching on
    /// the prose is exactly what a named error exists to replace.
    #[test]
    fn a_refusal_is_explained_by_its_name_not_its_message() {
        let text = refusal_text(
            SyncAction::Dehydrate("/r/doc.bin"),
            Some("org.konedrive.Error.ModifiedLocally"),
            "the file is not downloaded",
            "/r",
        );
        assert!(text.contains("lose your edits"), "{text}");
        assert!(!text.contains("no space to free"), "{text}");

        let text = refusal_text(
            SyncAction::Dehydrate("/r/doc.bin"),
            Some("org.konedrive.Error.NotHydrated"),
            "the file was modified locally",
            "/r",
        );
        assert!(text.contains("no space to free"), "{text}");
        assert!(!text.contains("lose your edits"), "{text}");
    }

    /// A name from outside `org.konedrive.Error` — the bus's own, say — is
    /// not mistaken for one of ours even when its last component matches.
    #[test]
    fn only_names_under_the_konedrive_prefix_are_ours() {
        let text = refusal_text(
            SyncAction::Hydrate("/r/doc.bin"),
            Some("org.freedesktop.DBus.Error.InUse"),
            "something else entirely",
            "/r",
        );
        assert_eq!(text, "downloading /r/doc.bin failed: something else entirely");
    }

    /// A Forget of a folder registered with the helper now
    /// needs the helper, and is refused `NoHelper` without one. The generic
    /// `NoHelper` text was written for registering — "… and  was not
    /// registered … `register-without-interception `" with an empty path —
    /// which reads as nonsense after `sync forget`, and says nothing about
    /// the folder still being registered.
    #[test]
    fn a_forget_refused_for_want_of_the_helper_says_the_folder_is_still_registered() {
        let text = refusal_text(
            SyncAction::Forget,
            Some("org.konedrive.Error.NoHelper"),
            "the konedrive helper is not connected",
            "/home/u/OneDrive",
        );
        assert!(text.contains("/home/u/OneDrive"), "{text}");
        assert!(text.contains("still registered"), "{text}");
        assert!(text.contains("once the helper is back"), "{text}");
        assert!(!text.contains("was not registered"), "{text}");
        assert!(!text.contains("register-without-interception"), "{text}");
    }

    /// follow-up: `Hydrate` of a file that may carry an ignore
    /// mark — one a cancelled "free up space" left half done, or one labelled
    /// downloaded with nothing to prove it — needs the helper to clear that
    /// mark first, and is refused `NoHelper` without one. The generic text
    /// talks about registering a folder.
    #[test]
    fn a_download_refused_for_want_of_the_helper_says_nothing_changed() {
        let text = refusal_text(
            SyncAction::Hydrate("/home/u/OneDrive/doc.bin"),
            Some("org.konedrive.Error.NoHelper"),
            "the konedrive helper is not connected",
            "/home/u/OneDrive",
        );
        assert!(text.contains("/home/u/OneDrive/doc.bin"), "{text}");
        assert!(text.contains("nothing was changed"), "{text}");
        assert!(text.contains("once the helper is back"), "{text}");
        assert!(!text.contains("was not registered"), "{text}");
    }

    /// a populate source that overlaps the sync
    /// folder is refused `Unsupported`, whose text was written for a folder
    /// that cannot be registered.
    #[test]
    fn a_populate_source_refused_as_unsupported_is_not_called_a_sync_folder() {
        let text = refusal_text(
            SyncAction::PopulateFrom("/home/u/OneDrive/src"),
            Some("org.konedrive.Error.Unsupported"),
            "/home/u/OneDrive/src is inside the sync folder /home/u/OneDrive, and a folder \
             cannot be filled from itself",
            "/home/u/OneDrive",
        );
        assert!(!text.contains("cannot be used as the sync folder"), "{text}");
        assert!(text.contains("cannot be filled from itself"), "{text}");
    }

    /// The same folder asked for again — which is what a restored folder
    /// waiting for its helper now answers `AlreadyRegistered` to — is not
    /// "use this one instead".
    #[test]
    fn registering_the_folder_that_is_already_registered_says_so() {
        let text = refusal_text(
            SyncAction::RegisterWithoutInterception("/home/u/OneDrive"),
            Some("org.konedrive.Error.AlreadyRegistered"),
            "a sync root is already registered; forget it first",
            "/home/u/OneDrive",
        );
        assert!(text.contains("already the sync folder"), "{text}");
        assert!(!text.contains("instead"), "{text}");

        let text = refusal_text(
            SyncAction::RegisterWithoutInterception("/home/u/Other"),
            Some("org.konedrive.Error.AlreadyRegistered"),
            "a sync root is already registered; forget it first",
            "/home/u/OneDrive",
        );
        assert!(text.contains("To use /home/u/Other instead"), "{text}");
    }

    #[test]
    fn durations_read_as_sync_pause_takes_them() {
        assert_eq!(parse_duration("90"), Some(90));
        assert_eq!(parse_duration("30m"), Some(1800));
        assert_eq!(parse_duration("2h"), Some(7200));
        assert_eq!(parse_duration("1d"), Some(86_400));
        assert_eq!(parse_duration("1h30m"), Some(5400));
        for bad in ["", "0", "soon", "2x", "h", "30m5", "999999999999"] {
            assert_eq!(parse_duration(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn outbox_lines_say_what_waits_and_why() {
        let rows = vec![
            (1, "create".to_owned(), "/f/a.txt".to_owned(), "running".to_owned(), 512, 2048, String::new(), 0),
            (2, "create".to_owned(), "/f/a:b".to_owned(), "blocked".to_owned(), 0, 1, "name-characters".to_owned(), 0),
        ];
        let text = outbox_text(&rows, true, "konedrivectl");
        assert!(text.contains("running  create   /f/a.txt  25% of 2.0 KiB"), "{text}");
        assert!(text.contains("blocked  create   /f/a:b  (a name OneDrive refuses"), "{text}");
        assert!(text.ends_with("`konedrivectl sync outbox --all` shows them all\n"), "{text}");
        assert_eq!(outbox_text(&[], false, "k"), "Nothing is waiting to upload.\n");
        assert_eq!(upload_reason_text("refused: bad name"), "OneDrive refused it: bad name");
    }

    /// the outbox on the bus: a `FreeUp` of several paths refused `NotUploaded` names the
    /// one path that has a change waiting, not all of them.
    #[test]
    fn a_free_up_refused_not_uploaded_names_the_one_path() {
        let detail = "/f/B/y is not uploaded yet, so freeing it up would lose the changes made here";
        assert_eq!(super::refused_path_of("org.konedrive.Error.NotUploaded", detail), Some("/f/B/y"));
        assert_eq!(super::refused_path_of("org.konedrive.Error.Failed", detail), None);
    }

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
