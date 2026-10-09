use konedrive_graph::quickxor::{decode_base64, QuickXor};
use konedrive_tree::{Kind, Row};

use super::{has_downloaded_version, to_download_result, LandingAttempt};
use crate::fuse::backing_directory::LandingOutcome;
use crate::fuse::directory_contents::tests::row;
use crate::hydration::source::Version;

/// The row the download was asked with: the fixture's `a.txt`, with the ctag `c`.
fn asked() -> Row {
    row("A", Some("R"), "a.txt", Kind::File)
}

/// The quickXorHash of `content`, as a row has it.
fn hash_of(content: &[u8]) -> String {
    let mut hasher = QuickXor::new();
    hasher.update(content);
    hasher.finish_base64()
}

fn version(ctag: &str, content: Option<&[u8]>) -> Version {
    Version { ctag: ctag.into(), quick_xor: content.and_then(|content| decode_base64(&hash_of(content))) }
}

#[test]
fn a_download_that_was_placed_is_downloaded() {
    assert_eq!(to_download_result(LandingAttempt::Attempted(LandingOutcome::Placed)), Ok(()));
}

/// The path holds the file's downloaded copy already: there is nothing left to do.
#[test]
fn a_download_that_was_kept_off_its_path_is_downloaded() {
    assert_eq!(to_download_result(LandingAttempt::Attempted(LandingOutcome::Kept)), Ok(()));
}

#[test]
fn a_download_that_was_refused_at_its_path_fails_as_a_name_that_exists() {
    assert_eq!(to_download_result(LandingAttempt::Attempted(LandingOutcome::Refused)), Err(libc::EEXIST));
}

#[test]
fn a_download_of_a_file_that_moved_meanwhile_fails_as_stale() {
    assert_eq!(to_download_result(LandingAttempt::FileMovedMeanwhile), Err(libc::ESTALE));
}

#[test]
fn a_download_of_a_version_the_store_no_longer_has_fails_as_stale() {
    assert_eq!(to_download_result(LandingAttempt::OneDriveVersionChanged), Err(libc::ESTALE));
}

/// The source said no version: the store's row is the one the download was asked with.
#[test]
fn the_row_that_was_asked_with_is_the_downloaded_version_when_the_source_gave_none() {
    assert!(has_downloaded_version(&asked(), &asked(), None));
}

/// A cycle brought another content while the file was downloaded: another ctag, another
/// hash, or another size.
#[test]
fn a_row_with_another_content_than_the_one_asked_with_is_not_the_downloaded_version() {
    let downloaded = version("c", None);
    for stored in [Row { ctag: Some("another".into()), ..asked() }, Row { quickxor: Some(hash_of(b"other")), ..asked() }, Row { size: 7, ..asked() }] {
        assert!(!has_downloaded_version(&stored, &asked(), None), "{stored:?}");
        assert!(!has_downloaded_version(&stored, &asked(), Some(&downloaded)), "{stored:?}");
    }
}

/// A row that only moved or was renamed has the content that was downloaded.
#[test]
fn a_row_that_was_renamed_meanwhile_is_the_downloaded_version() {
    let renamed = Row { name: "b.txt".into(), etag: Some("renamed".into()), ..asked() };
    assert!(has_downloaded_version(&renamed, &asked(), Some(&version("c", None))));
}

/// OneDrive has a content the store does not have yet: the source gave it, and the store's
/// row is still the one asked with.
#[test]
fn a_version_of_the_source_that_the_store_does_not_have_is_not_the_stored_one() {
    assert!(!has_downloaded_version(&asked(), &asked(), Some(&version("another", None))));
    let with_hash = Row { quickxor: Some(hash_of(b"old")), ..asked() };
    assert!(!has_downloaded_version(&with_hash, &with_hash, Some(&version("c", Some(b"new")))));
}

/// The hashes say it where both have one: the same content is the same version, whatever
/// the ctags.
#[test]
fn a_version_of_the_source_with_the_stored_hash_is_the_stored_one() {
    let with_hash = Row { quickxor: Some(hash_of(b"content")), ..asked() };
    assert!(has_downloaded_version(&with_hash, &with_hash, Some(&version("c", Some(b"content")))));
    assert!(has_downloaded_version(&with_hash, &with_hash, Some(&version("spelled otherwise", Some(b"content")))));
}

/// Without a hash on either side the ctags say it; a row without a ctag says nothing
/// against the version.
#[test]
fn a_version_of_the_source_is_compared_by_ctag_where_a_hash_is_missing() {
    assert!(has_downloaded_version(&asked(), &asked(), Some(&version("c", Some(b"content")))));
    assert!(!has_downloaded_version(&asked(), &asked(), Some(&version("another", Some(b"content")))));
    let without_ctag = Row { ctag: None, ..asked() };
    assert!(has_downloaded_version(&without_ctag, &without_ctag, Some(&version("c", None))));
}
