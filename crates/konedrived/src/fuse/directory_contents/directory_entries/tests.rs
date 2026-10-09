use std::path::PathBuf;

use konedrive_tree::{Kind, Placement, Row, SkipReason};

use super::stays;
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::unsent_changes::UnsentChanges;

fn file() -> Row {
    row("A", Some("R"), "a.txt", Kind::File)
}

#[test]
fn a_placed_row_no_unsent_change_names_stays_in_its_directory() {
    assert!(stays(&file(), &UnsentChanges::default()));
    // What is unsent of another row says nothing of this one.
    let mut of_another = UnsentChanges::default();
    of_another.removed_ids.insert("B".into());
    of_another.modified_ids.insert("A".into());
    assert!(stays(&file(), &of_another));
}

/// The store has it out of the folder: it is shown nowhere.
#[test]
fn a_row_that_is_not_placed_does_not_stay() {
    for placement in [Placement::Skipped(SkipReason::OneNote), Placement::Skipped(SkipReason::NameTooLong)] {
        assert!(!stays(&Row { placement, ..file() }, &UnsentChanges::default()), "{placement:?}");
    }
}

#[test]
fn a_row_removed_here_does_not_stay() {
    let mut unsent_changes = UnsentChanges::default();
    unsent_changes.removed_ids.insert("A".into());
    assert!(!stays(&file(), &unsent_changes));
}

/// Moved here, it is shown where it was moved to, and not where the store has it — also
/// when that is the same directory, under another name.
#[test]
fn a_row_moved_here_does_not_stay() {
    let mut unsent_changes = UnsentChanges::default();
    unsent_changes.moved_ids.insert("A".into(), (DirectoryReference { onedrive_id: Some("R".into()), path: PathBuf::new() }, "renamed.txt".into()));
    assert!(!stays(&file(), &unsent_changes));
}
