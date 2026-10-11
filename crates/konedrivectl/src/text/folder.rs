//! What the commands on an account's folder say when they are done.

use konedrive_dbus::rows::Event;

use konedrive_dbus::rows::NotInFolder;

use super::files::{skip_reason_text, still_here_text, where_here_text};
use super::formats::local_time;
use super::status::held_text;

/// `sync register`, done.
pub fn registered_text(path: &str) -> String {
    format!("Folder registered: {path}")
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

/// What `sync forget` and `account remove` say before they go on of the changes made here
/// that were not sent to OneDrive: `count` of them, or why the count cannot be read.
/// `None` for none: nothing is said, and nothing is asked.
// TODO: wording
pub fn unsent_warning_text(count: Result<u64, &str>) -> Option<String> {
    match count {
        Ok(0) => None,
        Ok(1) => Some("1 change was not sent to OneDrive and will stay only on this computer.".to_owned()),
        Ok(count) => Some(format!("{count} changes were not sent to OneDrive and will stay only on this computer.")),
        Err(why) => Some(format!("Could not read how many changes were not sent to OneDrive ({why}); any that were not will stay only on this computer.")),
    }
}

/// What follows the warning of unsent changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsentQuestion {
    /// The command was given `--yes`: it goes on unasked.
    Skipped,
    /// The question is asked on the terminal.
    Asked,
    /// There is no terminal to ask on (a script, a pipe, a loop that reads a list):
    /// nothing is read from the input, and the command fails with [`NO_TERMINAL_TO_ASK`].
    NoTerminal,
}

/// What follows the warning: `--yes` skips the question, whatever the input is; without
/// it the question is asked of a terminal alone. Pure.
pub fn decide_unsent_question(is_asking_skipped: bool, is_input_terminal: bool) -> UnsentQuestion {
    match (is_asking_skipped, is_input_terminal) {
        (true, _) => UnsentQuestion::Skipped,
        (false, true) => UnsentQuestion::Asked,
        (false, false) => UnsentQuestion::NoTerminal,
    }
}

/// What was answered to the question whether to go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsentAnswer {
    /// "y" or "yes", in any case: the command goes on.
    GoOn,
    /// Anything else, the empty line among it: nothing is changed.
    Declined,
    /// The input ended with no line: the command fails with [`QUESTION_NOT_ANSWERED`].
    NotAnswered,
}

/// What `line` answers, the line read after the question; `None` when the input ended
/// before one. Pure.
pub fn decide_unsent_answer(line: Option<&str>) -> UnsentAnswer {
    match line.map(|line| line.trim().to_ascii_lowercase()).as_deref() {
        None => UnsentAnswer::NotAnswered,
        Some("y" | "yes") => UnsentAnswer::GoOn,
        Some(_) => UnsentAnswer::Declined,
    }
}

/// The question after the warning of unsent changes.
pub const GO_ON_QUESTION: &str = "Go on? [y/N] ";

/// The question was declined.
pub const NOTHING_CHANGED: &str = "Nothing was changed.";

/// The question cannot be asked: the command fails with this.
pub const NO_TERMINAL_TO_ASK: &str = "Nothing was changed: there is no terminal to ask on. Pass --yes to go on without being asked.";

/// The input ended before the question was answered: the command fails with this.
pub const QUESTION_NOT_ANSWERED: &str = "Nothing was changed: the question was not answered. Pass --yes to go on without being asked.";

/// `sync forget`, done.
pub const FORGOTTEN: &str = "Folder forgotten. Local files were left untouched.";

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
