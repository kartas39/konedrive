use super::{rescue_dirs, skip_reason_text};
use crate::removed_text;

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

/// the outbox on the bus: a `FreeUp` of several paths refused `NotUploaded` names the
/// one path that has a change waiting, not all of them.
#[test]
fn a_free_up_refused_not_uploaded_names_the_one_path() {
    let detail = "/f/B/y is not uploaded yet, so freeing it up would lose the changes made here";
    assert_eq!(super::refused_path_of("org.konedrive.Error.NotUploaded", detail), Some("/f/B/y"));
    assert_eq!(super::refused_path_of("org.konedrive.Error.Failed", detail), None);
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
}
