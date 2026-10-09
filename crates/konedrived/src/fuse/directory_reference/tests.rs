use std::path::PathBuf;

use super::DirectoryReference;

fn reference(onedrive_id: Option<&str>, path: &str) -> DirectoryReference {
    DirectoryReference { onedrive_id: onedrive_id.map(str::to_owned), path: PathBuf::from(path) }
}

/// A directory of OneDrive is found by its id, wherever the reference last saw it.
#[test]
fn a_reference_with_a_onedrive_id_refers_to_the_directory_that_has_the_id() {
    let docs = reference(Some("D"), "docs");
    assert!(docs.matches(&reference(Some("D"), "docs")));
    assert!(docs.matches(&reference(Some("D"), "papers")), "moved since the reference was written");
    assert!(!docs.matches(&reference(Some("E"), "docs")));
    assert!(!reference(None, "docs").matches(&reference(Some("D"), "docs")), "a directory without an id is not the one that has it");
}

/// A directory that was not in OneDrive when the reference was written is found by its path,
/// also once it has its id.
#[test]
fn a_reference_without_a_onedrive_id_refers_to_the_directory_at_its_path() {
    let reference_by_path = reference(None, "new");
    assert!(reference(None, "new").matches(&reference_by_path));
    assert!(reference(Some("N"), "new").matches(&reference_by_path), "it got its id since");
    assert!(!reference(None, "other").matches(&reference_by_path));
}
