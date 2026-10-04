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
