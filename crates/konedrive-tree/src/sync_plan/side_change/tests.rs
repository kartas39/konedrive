use super::*;
use crate::model::ItemKind;
use crate::sync_plan::tests::{blank, hashed, hashed_here, listed, made_directory, made_file, moved_here, moved_in_onedrive, removed_here, removed_in_onedrive, synced_directory, synced_file, with_content_here, with_content_in_onedrive, written_here, ROOT};

const UNCHANGED: SideChange = SideChange::IsThere { place: PlaceChange::Same, content: ContentChange::Same };

#[test]
fn a_side_that_has_the_item_as_was_agreed_on_did_nothing() {
    for item in [synced_file(2, ROOT, "a.txt"), synced_directory(3, ROOT, "d")] {
        assert_eq!((SideChange::of_local(&item), SideChange::of_onedrive(&item)), (UNCHANGED, UNCHANGED));
    }
}

#[test]
fn a_side_that_does_not_have_what_was_agreed_on_removed_it() {
    let item = removed_here(synced_file(2, ROOT, "a.txt"));
    assert_eq!((SideChange::of_local(&item), SideChange::of_onedrive(&item)), (SideChange::Removed, UNCHANGED));
    let item = removed_in_onedrive(synced_directory(2, ROOT, "d"));
    assert_eq!((SideChange::of_local(&item), SideChange::of_onedrive(&item)), (UNCHANGED, SideChange::Removed));
    let item = removed_in_onedrive(removed_here(synced_file(2, ROOT, "a.txt")));
    assert_eq!((SideChange::of_local(&item), SideChange::of_onedrive(&item)), (SideChange::Removed, SideChange::Removed));
}

#[test]
fn a_side_that_does_not_have_an_item_nothing_was_agreed_on_for_never_had_it() {
    let new = SideChange::IsThere { place: PlaceChange::Moved, content: ContentChange::Changed };
    let item = made_file(2, ROOT, "a.txt");
    assert_eq!((SideChange::of_local(&item), SideChange::of_onedrive(&item)), (new, SideChange::NeverWas));
    let item = listed(2, ItemKind::File, ROOT, "a.txt");
    assert_eq!((SideChange::of_local(&item), SideChange::of_onedrive(&item)), (SideChange::NeverWas, new));
    let item = blank(2, ItemKind::File);
    assert_eq!((SideChange::of_local(&item), SideChange::of_onedrive(&item)), (SideChange::NeverWas, SideChange::NeverWas));
    // A new directory has no content to change.
    let item = made_directory(2, ROOT, "d");
    assert_eq!(SideChange::of_local(&item), SideChange::IsThere { place: PlaceChange::Moved, content: ContentChange::Same });
}

#[test]
fn a_side_that_has_the_item_in_another_directory_or_under_another_name_moved_it() {
    let moved = SideChange::IsThere { place: PlaceChange::Moved, content: ContentChange::Same };
    let directory = synced_directory(3, ROOT, "d").local_item_id;
    let item = moved_here(synced_file(2, ROOT, "a.txt"), directory, "a.txt");
    assert_eq!((SideChange::of_local(&item), SideChange::of_onedrive(&item)), (moved, UNCHANGED));
    let item = moved_in_onedrive(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt");
    assert_eq!((SideChange::of_local(&item), SideChange::of_onedrive(&item)), (UNCHANGED, moved));
    // A name is compared as it was given: another case of a letter is another name.
    let item = moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "A.txt");
    assert_eq!(SideChange::of_local(&item), moved);
}

#[test]
fn the_content_here_is_compared_by_its_hash_where_one_is_known() {
    let content_here = |content| match SideChange::of_local(&with_content_here(synced_file(2, ROOT, "a.txt"), content)) {
        SideChange::IsThere { place: PlaceChange::Same, content } => content,
        other => panic!("{other:?}"),
    };
    // The synced content of the file is `hashed("h2")`.
    assert_eq!(content_here(hashed("h2")), ContentChange::Same);
    assert_eq!(content_here(hashed_here("h2")), ContentChange::Same, "the same hash, whatever the time and the cTag");
    assert_eq!(content_here(hashed_here("other")), ContentChange::Changed);
    assert_eq!(content_here(written_here()), ContentChange::NotKnownWithoutHash, "a program wrote it, and it is of the same size");
    let longer = ContentVersion::ChangedHere { size: 4, mtime: 200, mtime_nsec: 7, change_count: 1 };
    assert_eq!(content_here(longer), ContentChange::Changed, "another size is another content");
}

#[test]
fn the_content_here_against_a_content_with_no_hash_is_changed_unless_it_is_that_content_version() {
    let no_hash = ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: None, ctag: Some("c1".to_owned()) };
    assert_eq!(compare_local_content(Some(&no_hash), Some(&no_hash)), ContentChange::Same);
    assert_eq!(compare_local_content(Some(&written_here()), Some(&no_hash)), ContentChange::Changed);
    assert_eq!(compare_local_content(Some(&hashed_here("mine")), Some(&no_hash)), ContentChange::Changed);
    assert_eq!(compare_local_content(Some(&written_here()), None), ContentChange::Changed);
    assert_eq!(compare_local_content(None, None), ContentChange::Same);
}

#[test]
fn the_content_in_onedrive_is_the_same_or_changed_and_never_not_known() {
    let content_in_onedrive = |content| match SideChange::of_onedrive(&with_content_in_onedrive(synced_file(2, ROOT, "a.txt"), content)) {
        SideChange::IsThere { place: PlaceChange::Same, content } => content,
        other => panic!("{other:?}"),
    };
    assert_eq!(content_in_onedrive(hashed("h2")), ContentChange::Same);
    assert_eq!(content_in_onedrive(hashed("theirs")), ContentChange::Changed);
    let retagged = ContentVersion::Hashed { size: 3, mtime: 500, mtime_nsec: 0, hash: Some("h2".to_owned()), ctag: Some("c-retagged".to_owned()) };
    assert_eq!(content_in_onedrive(retagged), ContentChange::Same, "the same hash under another cTag and time");
    let no_hash = |ctag: &str, size| ContentVersion::Hashed { size, mtime: 500, mtime_nsec: 0, hash: None, ctag: Some(ctag.to_owned()) };
    assert_eq!(content_in_onedrive(no_hash("c-h2", 3)), ContentChange::Same, "no hash, and the cTag agreed on");
    assert_eq!(content_in_onedrive(no_hash("c-other", 3)), ContentChange::Changed);
    assert_eq!(content_in_onedrive(no_hash("c-h2", 4)), ContentChange::Changed, "another size");
    assert_eq!(compare_onedrive_content(Some(&hashed("h2")), Some(&written_here())), ContentChange::Changed, "what OneDrive has is not hashed here");
    // A time alone is no change of a content, whatever tells the content.
    let at = |mtime, hash: Option<&str>, ctag: Option<&str>| ContentVersion::Hashed { size: 3, mtime, mtime_nsec: 0, hash: hash.map(str::to_owned), ctag: ctag.map(str::to_owned) };
    for (hash, ctag) in [(Some("h"), Some("c")), (None, Some("c")), (Some("h"), None), (None, None)] {
        assert_eq!(compare_onedrive_content(Some(&at(500, hash, ctag)), Some(&at(100, hash, ctag))), ContentChange::Same, "{hash:?} {ctag:?}");
    }
}

#[test]
fn two_contents_are_one_but_for_the_time_when_all_else_that_tells_them_is_the_same() {
    let other_time = ContentVersion::Hashed { size: 3, mtime: 5000, mtime_nsec: 7, hash: Some("h2".to_owned()), ctag: Some("c-h2".to_owned()) };
    assert!(is_same_content_but_for_time(Some(&hashed("h2")), Some(&hashed("h2"))));
    assert!(is_same_content_but_for_time(Some(&other_time), Some(&hashed("h2"))));
    assert!(is_same_content_but_for_time(None, None), "a directory has no content");
    assert!(!is_same_content_but_for_time(Some(&hashed("h2")), None));
    assert!(!is_same_content_but_for_time(Some(&hashed_here("h2")), Some(&hashed("h2"))), "one has a cTag and the other none");
    assert!(!is_same_content_but_for_time(Some(&hashed("theirs")), Some(&hashed("h2"))));
    assert!(!is_same_content_but_for_time(Some(&written_here()), Some(&hashed("h2"))));
    let written_later = ContentVersion::ChangedHere { size: 3, mtime: 900, mtime_nsec: 0, change_count: 1 };
    assert!(is_same_content_but_for_time(Some(&written_here()), Some(&written_later)));
    assert!(!is_same_content_but_for_time(Some(&written_here()), Some(&ContentVersion::ChangedHere { size: 3, mtime: 200, mtime_nsec: 7, change_count: 2 })));
}

/// A copy's cTag mark survives an edit in place, so a content the rows
/// took from a mark (a cTag and no hash) is another content's only when the size and the
/// time in whole seconds are the same too.
#[test]
fn the_content_here_taken_from_a_mark_is_the_same_only_with_the_same_size_and_whole_second() {
    let from_mark = |ctag: &str, size, mtime| ContentVersion::Hashed { size, mtime, mtime_nsec: 500, hash: None, ctag: Some(ctag.to_owned()) };
    // `hashed("h2")` is of size 3, of the time 100 and of the cTag `c-h2`.
    assert_eq!(compare_local_content(Some(&from_mark("c-h2", 3, 100)), Some(&hashed("h2"))), ContentChange::Same, "the part of a second is the disk's alone");
    assert_eq!(compare_local_content(Some(&from_mark("c-h2", 3, 101)), Some(&hashed("h2"))), ContentChange::NotKnownWithoutHash, "edited in place: the mark stayed, the time did not");
    assert_eq!(compare_local_content(Some(&from_mark("c-h2", 4, 100)), Some(&hashed("h2"))), ContentChange::Changed, "another size");
    assert_eq!(compare_local_content(Some(&from_mark("c-other", 3, 100)), Some(&hashed("h2"))), ContentChange::NotKnownWithoutHash, "another mark says nothing");
    let no_hash = ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: None, ctag: Some("c-h2".to_owned()) };
    assert_eq!(compare_local_content(Some(&from_mark("c-h2", 3, 100)), Some(&no_hash)), ContentChange::Same, "against a content OneDrive gave no hash for");
    assert_eq!(compare_local_content(Some(&from_mark("c-h2", 3, 101)), Some(&no_hash)), ContentChange::Changed);
}
