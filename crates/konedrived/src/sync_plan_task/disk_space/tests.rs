use super::*;

const GB: u64 = 1 << 30;

#[test]
fn every_download_that_fits_beyond_the_reserve_is_started() {
    assert_eq!(decide_downloads_held_for_space(&[GB, 2 * GB], 0, Some(13 * GB), 10 * GB), vec![false, false]);
}

#[test]
fn a_download_that_does_not_fit_is_held_and_a_smaller_one_after_it_is_not() {
    assert_eq!(decide_downloads_held_for_space(&[GB, 5 * GB, GB, 2 * GB], 0, Some(12 * GB), 10 * GB), vec![false, true, false, true]);
}

#[test]
fn the_downloads_that_run_count_with_their_whole_size() {
    assert_eq!(decide_downloads_held_for_space(&[GB], 2 * GB, Some(12 * GB), 10 * GB), vec![true]);
    assert_eq!(decide_downloads_held_for_space(&[GB], GB, Some(12 * GB), 10 * GB), vec![false]);
}

#[test]
fn with_less_free_than_the_reserve_every_download_with_content_is_held() {
    assert_eq!(decide_downloads_held_for_space(&[1, GB], 0, Some(9 * GB), 10 * GB), vec![true, true]);
}

#[test]
fn an_empty_file_is_never_held() {
    assert_eq!(decide_downloads_held_for_space(&[0], 0, Some(0), 10 * GB), vec![false]);
}

#[test]
fn free_space_that_cannot_be_measured_holds_nothing() {
    assert_eq!(decide_downloads_held_for_space(&[GB, 100 * GB], 0, None, 10 * GB), vec![false, false]);
}

#[test]
fn the_shortage_counts_the_held_downloads_and_none_is_no_shortage() {
    let sizes = [GB, 5 * GB, 3 * GB];
    assert_eq!(describe_shortage(&sizes, &[false, true, true], Some(12 * GB), 10 * GB), Some(DiskSpaceShortage { count: 2, bytes: 8 * GB, free_space: 12 * GB, reserve: 10 * GB }));
    assert_eq!(describe_shortage(&sizes, &[false, false, false], Some(40 * GB), 10 * GB), None);
    assert_eq!(count_reserve_bytes(10), 10 * GB);
}
