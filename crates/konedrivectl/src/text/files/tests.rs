use crate::text::accounts::removed_text;
use konedrive_dbus::rows::Conflict;

/// `sync conflicts` names the file and the copy kept beside it; `account remove` says what
/// it deleted and that the folder stays.
#[test]
fn a_conflict_is_listed_with_the_copy_kept_beside_the_file() {
    let conflicts = [Conflict { at: 0, original: "/home/u/OneDrive/d.txt".to_owned(), kept: "/home/u/OneDrive/d-fedora.txt".to_owned(), how: "copy".to_owned() }];
    let listed = super::conflicts_text(&conflicts);
    assert!(listed.starts_with("/home/u/OneDrive/d.txt\n    changed here and in OneDrive: yours is kept beside it as /home/u/OneDrive/d-fedora.txt, "), "{listed}");
    assert_eq!(super::conflicts_text(&[]), "No conflicts.\n");
    assert!(removed_text("Home", "").contains("It had no folder."));
    assert!(removed_text("Home", "/home/u/OneDrive").contains("Kept: the folder /home/u/OneDrive"));
}

/// The path refused and what pins it travel inside the daemon's sentences (issue #232):
/// what the daemon writes today is read back here, so a sentence reworded on
/// one side fails this test.
#[test]
fn the_daemons_sentences_are_read_back() {
    use std::path::Path;

    use konedrived::account_folder::SyncError;

    let pinned = konedrived::account_folder::pins::refusal(Path::new("/f/Docs/a b.txt"), Path::new("/f/Docs"));
    assert_eq!(pinned, "/f/Docs/a b.txt is pinned by /f/Docs: unpin it first");
    assert_eq!(super::pinned_parts(&pinned), Some(("/f/Docs/a b.txt", "/f/Docs")));
    assert_eq!(super::refused_path_of("org.konedrive.Error.NotAllowed", &pinned), Some("/f/Docs/a b.txt"));

    let waiting = SyncError::NotUploaded("/f/B/y".into()).to_string();
    assert_eq!(super::refused_path_of("org.konedrive.Error.NotUploaded", &waiting), Some("/f/B/y"));
    let no_page = SyncError::NotInOneDrive("/f/B/y".into()).to_string();
    assert_eq!(super::refused_path_of("org.konedrive.Error.NotUploaded", &no_page), Some("/f/B/y"));
    assert_eq!(super::refused_path_of("org.konedrive.Error.Failed", &waiting), None, "only the two refusals name a path");
}

/// `sync skipped` says of an item that is still on this computer that it
/// is, and what keeps it; of one that is not here it says only why the
/// folder cannot hold it.
#[test]
fn an_item_that_is_still_here_says_what_keeps_it() {
    use konedrive_dbus::rows::NotInFolder;
    let line = |path: &str, waits: &str| NotInFolder {
        path: path.to_owned(),
        reason: "name-too-long".to_owned(),
        waits: waits.to_owned(),
        here: if waits.is_empty() { String::new() } else { "/home/u/OneDrive/docs-fedora".to_owned() },
    };
    let listed = crate::text::folder::skipped_text(&[line("/home/u/OneDrive/gone", ""), line("/home/u/OneDrive/long", "uploads:2")], false);
    assert_eq!(
        listed,
        "/home/u/OneDrive/gone\n    The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two).\n\
         /home/u/OneDrive/long\n    The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two).\n    \
         Still on this computer: 2 changes in it wait to be uploaded.\n    It is at /home/u/OneDrive/docs-fedora.\n"
    );
    let said = |waits: &str| super::still_here_text(waits).unwrap();
    assert_eq!(super::still_here_text(""), None);
    assert_eq!(said("uploads:1"), "Still on this computer: 1 change in it waits to be uploaded.");
    assert_eq!(said("open-for-writing:/home/u/OneDrive/docs/new.txt"), "Still on this computer: /home/u/OneDrive/docs/new.txt is open in a program.");
    assert_eq!(said("cycle"), said("a-word-of-a-newer-daemon:x"));
}
