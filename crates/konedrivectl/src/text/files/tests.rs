use super::rescue_dirs;
use crate::text::accounts::removed_text;
use konedrive_dbus::rows::Conflict;

/// `account remove` names where the listed conflicts' files were rescued, and nothing it
/// cannot know.
#[test]
fn removal_says_where_the_listed_rescues_are() {
    let conflict = |original: &str, kept: &str, how: &str| Conflict { at: 0, original: original.to_owned(), kept: kept.to_owned(), how: how.to_owned() };
    let rescued = |original: &str, kept: &str| conflict(original, kept, "rescued");
    let conflicts = [
        rescued("/home/u/OneDrive/docs/a.txt", "/data/rescued/id/t1/docs/a.txt"),
        rescued("/home/u/OneDrive/b.txt", "/home/u/.konedrive-rescued-OneDrive/t2/b.txt"),
        rescued("/home/u/OneDrive/c.txt", "/data/rescued/id/t1/c.txt"),
        conflict("/home/u/OneDrive/d.txt", "/home/u/OneDrive/d-fedora.txt", "copy"),
    ];
    let dirs = rescue_dirs("/home/u/OneDrive", &conflicts);
    assert_eq!(dirs, ["/data/rescued/id/t1", "/home/u/.konedrive-rescued-OneDrive/t2"], "a copy stays in the folder");
    let listed = super::conflicts_text(&conflicts);
    assert!(listed.contains("moved to /data/rescued/id/t1/c.txt"), "{listed}");
    assert!(listed.contains("changed here and in OneDrive: yours is kept beside it as /home/u/OneDrive/d-fedora.txt"), "{listed}");
    let said = removed_text("Home", "", &[]);
    assert!(said.contains("It had no folder.") && !said.contains("Rescued"), "{said}");
}

/// The path refused and what pins it travel inside the daemon's sentences (limitations
/// log D35): what the daemon writes today is read back here, so a sentence reworded on
/// one side fails this test.
#[test]
fn the_daemons_sentences_are_read_back() {
    use std::path::Path;

    use konedrived::sync::SyncError;

    let pinned = konedrived::hydration::pin::refusal(Path::new("/f/Docs/a b.txt"), Path::new("/f/Docs"));
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
    let line = |path: &str, waits: &str| NotInFolder { path: path.to_owned(), reason: "name-too-long".to_owned(), waits: waits.to_owned() };
    let listed = crate::text::folder::skipped_text(&[line("/home/u/OneDrive/gone", ""), line("/home/u/OneDrive/long", "uploads:2")], false);
    assert_eq!(
        listed,
        "/home/u/OneDrive/gone\n    The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two).\n\
         /home/u/OneDrive/long\n    The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two).\n    \
         Still on this computer: 2 changes in it wait to be uploaded.\n"
    );
    let said = |waits: &str| super::still_here_text(waits).unwrap();
    assert_eq!(super::still_here_text(""), None);
    assert_eq!(said("uploads:1"), "Still on this computer: 1 change in it waits to be uploaded.");
    assert_eq!(said("changes:/home/u/OneDrive/docs/new.txt"), "Still on this computer: what was done at /home/u/OneDrive/docs/new.txt on this computer has not reached OneDrive yet.");
    assert_eq!(said("mounted-inside:/home/u/OneDrive/docs/sub"), "Still on this computer: another filesystem is mounted at /home/u/OneDrive/docs/sub. Unmount it.");
    assert!(said("moved-in-onedrive:/home/u/OneDrive/docs/a.txt").contains("/home/u/OneDrive/docs/a.txt was moved in OneDrive"));
    assert_eq!(said("cycle"), said("a-word-of-a-newer-daemon:x"));
}
