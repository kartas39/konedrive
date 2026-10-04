use super::{needs_attention_text, paused_now_text};
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
