use super::{activity_text, decide_unsent_answer, decide_unsent_question, needs_attention_text, paused_now_text, unsent_warning_text, UnsentAnswer, UnsentQuestion, NO_TERMINAL_TO_ASK, QUESTION_NOT_ANSWERED};
use crate::text::files::{folders_unread_text, pin_text, PLAIN_PREFIX};
use crate::text::formats::{local_time, warning_text};

/// A command that did what it was asked in a folder whose state is `error` names the folder,
/// when the account has one, and says why it needs attention.
#[test]
fn a_folder_in_error_is_named_with_what_is_wrong() {
    assert_eq!(
        needs_attention_text("/home/u/OneDrive", "recovery left 1 file"),
        "the sync folder /home/u/OneDrive needs attention: recovery left 1 file"
    );
    assert_eq!(needs_attention_text("", "recovery left 1 file"), "the sync folder needs attention: recovery left 1 file");
}

/// A path command whose folders cannot be read afterwards still says what it did, with the
/// plain command where it suggests one, and the failed read is a warning.
#[test]
fn what_a_path_command_did_is_said_though_the_folders_cannot_be_read() {
    assert_eq!(
        pin_text(2, PLAIN_PREFIX),
        "Kept on this device. 2 files are downloading (`konedrivectl sync transfers`)."
    );
    assert_eq!(
        warning_text(&folders_unread_text("the connection is closed")),
        "warning: done, but the sync folders could not be read afterwards, so whether one needs attention was not \
         checked: the connection is closed"
    );
}

#[test]
fn a_pause_says_when_it_ends() {
    assert_eq!(paused_now_text(None, "konedrivectl"), "Paused until `konedrivectl sync resume`.");
    assert_eq!(paused_now_text(Some(0), "konedrivectl"), format!("Paused until {}.", local_time(0)));
}

/// Before a Forget and a removal of an account: nothing is said of no unsent change, the
/// count is said in the singular and the plural, and a count that cannot be read is said
/// with why.
#[test]
fn the_unsent_changes_are_said_with_their_count_and_none_is_not_said() {
    assert_eq!(unsent_warning_text(Ok(0)), None);
    assert_eq!(unsent_warning_text(Ok(1)).as_deref(), Some("1 change was not sent to OneDrive and will stay only on this computer."));
    assert_eq!(unsent_warning_text(Ok(12)).as_deref(), Some("12 changes were not sent to OneDrive and will stay only on this computer."));
    assert_eq!(
        unsent_warning_text(Err("the daemon does not answer")).as_deref(),
        Some("Could not read how many changes were not sent to OneDrive (the daemon does not answer); any that were not will stay only on this computer.")
    );
}

/// `--yes` skips the question whatever the input is; without it the question is asked of a
/// terminal alone, and with no terminal the command fails with a text that names `--yes`.
#[test]
fn the_question_is_skipped_by_yes_and_asked_of_a_terminal_alone() {
    assert_eq!(decide_unsent_question(true, true), UnsentQuestion::Skipped);
    assert_eq!(decide_unsent_question(true, false), UnsentQuestion::Skipped, "a script that passed --yes");
    assert_eq!(decide_unsent_question(false, true), UnsentQuestion::Asked);
    assert_eq!(decide_unsent_question(false, false), UnsentQuestion::NoTerminal, "nothing is read from an input that is no terminal");
    assert!(NO_TERMINAL_TO_ASK.contains("--yes") && QUESTION_NOT_ANSWERED.contains("--yes"));
}

/// Only "y" and "yes" go on; anything else changes nothing; an input that ended is no
/// answer at all.
#[test]
fn only_y_and_yes_go_on_and_an_input_that_ended_is_no_answer() {
    for line in ["y\n", "Y\n", "yes\n", " YES \n", "y"] {
        assert_eq!(decide_unsent_answer(Some(line)), UnsentAnswer::GoOn, "{line:?}");
    }
    for line in ["n\n", "N\n", "no\n", "\n", "", "yes please\n", "да\n"] {
        assert_eq!(decide_unsent_answer(Some(line)), UnsentAnswer::Declined, "{line:?}");
    }
    assert_eq!(decide_unsent_answer(None), UnsentAnswer::NotAnswered, "Ctrl-D, or a pipe that ended");
}

/// `sync activity` says of a directory that could not be removed here, and of a name
/// OneDrive holds with an item that cannot be shown here, why, and what to do; every other
/// kind keeps its detail, or none.
#[test]
fn the_activity_says_why_a_directory_was_not_removed_or_a_name_cannot_be_used_here() {
    let event = |kind: &str, path: &str, detail: &str| konedrive_dbus::rows::Event { at: 0, kind: kind.to_owned(), path: path.to_owned(), detail: detail.to_owned() };
    let out = activity_text(&[event("not-removed-here", "/r/Notebooks", ""), event("uploaded", "/r/a.txt", "3 KB"), event("removed", "/r/b.txt", "")]);
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines[0].ends_with("not-removed-here  /r/Notebooks  (cannot be removed here: it holds items that cannot be shown on this computer. Remove it in OneDrive.)"),
        "{out}"
    );
    assert!(lines[1].ends_with("/r/a.txt  (3 KB)"), "{out}");
    // A name OneDrive holds with an item the folder cannot show: why, with the item's reason, and what to do.
    let taken = activity_text(&[event("name-taken-in-onedrive", "/r/Documents/IT", "onenote")]);
    assert!(
        taken.trim_end().ends_with(
            "name-taken-in-onedrive  /r/Documents/IT  (cannot be used here: OneDrive has an item of this name that cannot be shown on this computer. A OneNote notebook, which is not a file. Rename it in OneDrive to use the name.)"
        ),
        "{taken}"
    );
    // A store made anew that lost something: the kind, the folder, and the daemon's sentence.
    let rebuilt = activity_text(&[event("store-rebuilt", "/r", "The database of this folder was of an older version and was rebuilt; lost with it: 3 pin(s)")]);
    assert!(rebuilt.trim_end().ends_with("store-rebuilt  /r  (The database of this folder was of an older version and was rebuilt; lost with it: 3 pin(s))"), "{rebuilt}");
    assert!(lines[2].ends_with("/r/b.txt"), "{out}");
}
