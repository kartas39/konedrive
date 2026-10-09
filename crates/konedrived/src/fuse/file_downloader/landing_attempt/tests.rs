use super::{to_download_result, LandingAttempt};
use crate::fuse::backing_directory::LandingOutcome;

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
