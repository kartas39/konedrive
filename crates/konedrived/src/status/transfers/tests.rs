use super::*;

/// `LargeFiles` (issue #50): each large download once, a file being opened left out, and
/// the large uploads; small ones are not large files.
#[test]
fn large_files_are_the_large_downloads_but_opens_and_the_large_uploads() {
    let transfers = Transfers::default();
    let large = konedrive_graph::pool::LARGE_FROM;
    let _pinned = transfers.start("/r/pinned.iso".into(), large);
    let _opened = transfers.start_as("/r/opened.iso".into(), large, true);
    let _small = transfers.start("/r/small.txt".into(), 10);
    let uploads = vec![("/r/up.iso".to_owned(), 0, large + 1), ("/r/up.txt".to_owned(), 0, 10)];
    let downloads = transfers.subscribe().borrow().clone();
    assert_eq!(large_files(&downloads, &uploads), 2);
}
