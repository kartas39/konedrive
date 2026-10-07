//! What the commands on an account's folder say when they are done.

use konedrive_dbus::rows::Event;

use konedrive_dbus::rows::NotInFolder;

use super::files::{skip_reason_text, still_here_text, where_here_text};
use super::formats::local_time;
use super::status::held_text;

/// `sync register` and `sync register-without-interception`, done.
pub fn registered_text(path: &str, intercepted: bool) -> String {
    if intercepted {
        format!("Folder registered: {path}")
    } else {
        format!("Folder registered without interception: {path}")
    }
}

/// A registration that went through and left the folder in `error`: `detail` is its
/// `LastError`. The command fails with this, so that a script checking only the exit status
/// sees the same trouble a person reading the output would.
pub fn not_recovered_text(path: &str, detail: &str) -> String {
    format!("the folder at {path} is registered, but was not fully recovered: {detail}")
}

/// A command that did what it was asked, in a folder (`root`; empty when it has none now)
/// whose state is `error`: `detail` is its `LastError`. The command fails with this.
pub fn needs_attention_text(root: &str, detail: &str) -> String {
    if root.is_empty() {
        format!("the sync folder needs attention: {detail}")
    } else {
        format!("the sync folder {root} needs attention: {detail}")
    }
}

/// `sync forget`, done.
pub const FORGOTTEN: &str = "Folder forgotten. Local files were left untouched.";

/// `sync populate-from`, done.
pub fn populated_text(created: u64) -> String {
    format!("Created {created} placeholders.")
}

/// `sync skipped` with no folder.
pub const NO_FOLDER: &str = "No folder is registered.";

/// `sync skipped` of a folder that does not show OneDrive.
pub const NOT_ONEDRIVE: &str = "This folder is not connected to OneDrive.";

/// `sync skipped`: each item OneDrive has and the folder does not, with why; `listing` says
/// that the folder was still being filled when the list was asked for.
pub fn skipped_text(skipped: &[NotInFolder], listing: bool) -> String {
    let mut out = String::new();
    if listing {
        out.push_str("The folder is still being filled from OneDrive; this list may be partial.\n");
    }
    if skipped.is_empty() {
        out.push_str("Nothing is skipped.\n");
    }
    for NotInFolder { path, reason, waits, here } in skipped {
        out.push_str(&format!("{path}\n    {}\n", skip_reason_text(reason)));
        if let Some(still_here) = still_here_text(waits) {
            out.push_str(&format!("    {still_here}\n"));
        }
        if !here.is_empty() {
            out.push_str(&format!("    {}\n", where_here_text(here)));
        }
    }
    out
}

/// `sync refresh`, done.
pub const REFRESHED: &str = "Asked OneDrive for changes.";

/// What is wrong with `text` given to `sync pause --for`.
pub fn not_a_duration_text(text: &str) -> String {
    format!("`{text}` is not a duration: write it as 30m, 2h, 1d or 1h30m")
}

/// `sync pause`, done: until `until` (unix seconds) for a pause with a duration, or until
/// resumed for one without.
pub fn paused_now_text(until: Option<i64>, prefix: &str) -> String {
    match until {
        None => format!("Paused until `{prefix} sync resume`."),
        Some(until) => format!("Paused until {}.", local_time(until)),
    }
}

/// `sync resume`, done.
pub const RESUMED: &str = "Resumed.";

/// `sync anyway`, done: `held` is why the account held back by itself (`Folder.HeldBack`),
/// empty when it did not.
pub fn anyway_text(held: &str) -> String {
    match held {
        "" => "Not paused by itself: nothing to lift.".to_owned(),
        reason => {
            format!("Syncing anyway ({}) until the connection, the battery or the power profile changes.", held_text(reason))
        }
    }
}

/// `sync anyway --all` when no account held back by itself.
pub const NOTHING_TO_LIFT: &str = "No account is paused by itself: nothing to lift.";

/// `sync thumbnails`' answer.
pub fn thumbnails_text(on: bool) -> &'static str {
    if on {
        "Thumbnails: on — OneDrive's previews of images and videos are downloaded."
    } else {
        "Thumbnails: off — Dolphin downloads a cloud-only file in full to show its preview while its previews are on."
    }
}

/// `sync ignore add` of a pattern that is on the list.
pub fn ignored_already_text(pattern: &str) -> String {
    format!("`{pattern}` is on the list already.")
}

/// `sync ignore add`, done.
pub fn ignore_added_text(pattern: &str) -> String {
    format!("Added `{pattern}`: local files named so are not uploaded.")
}

/// `sync ignore remove`, done.
pub fn ignore_removed_text(pattern: &str) -> String {
    format!("Removed `{pattern}`: local files named so are uploaded from now on.")
}

/// `sync ignore remove` of a pattern that is not on the list.
pub fn not_ignored_text(pattern: &str, prefix: &str) -> String {
    format!("`{pattern}` is not on the list (`{prefix} sync ignore list`)")
}

/// `sync deletes confirm`, done: how many held removals went ahead.
pub fn deletes_confirmed_text(count: u32) -> String {
    match count {
        0 => NO_DELETE_WAITS.to_owned(),
        n => format!("Confirmed: {n} change(s) go to OneDrive's recycle bin."),
    }
}

/// `sync deletes restore`, done: how many held removals were dropped.
pub fn deletes_restored_text(count: u32) -> String {
    match count {
        0 => NO_DELETE_WAITS.to_owned(),
        n => format!("Restored: {n} change(s) dropped; the items come back from OneDrive."),
    }
}

const NO_DELETE_WAITS: &str = "No delete is waiting for confirmation.";

/// `sync dismiss`, done.
pub const DISMISSED: &str = "Dismissed. The file was left where it is.";

/// After `sync free-up-space` in a folder with pins.
pub const PINS_LEFT: &str = "Files kept on this device (`konedrivectl sync pin`) were left as they are.";

/// `sync activity`: one line per event, newest first — time, kind, full
/// path, and the detail in parentheses when there is one.
pub fn activity_text(events: &[Event]) -> String {
    if events.is_empty() {
        return "Nothing has happened yet.\n".to_owned();
    }
    let mut out = String::new();
    for Event { at, kind, path, detail } in events {
        let detail = if detail.is_empty() { String::new() } else { format!("  ({detail})") };
        out.push_str(&format!("{}  {kind:<10}  {path}{detail}\n", local_time(*at)));
    }
    out
}

#[cfg(test)]
mod tests;
