use super::{decide_listed_item, decide_listed_removal, decide_listed_root, is_listed_removal_outdated, is_listed_version_outdated, ListedChangeOutcome, ListedChangeRefusal, ListedItem};
use crate::item_store::onedrive_versions::listing_under_way::LeftToReadAgain;
use crate::item_store::tests::{place, reference};
use crate::model::{ContentPresence, ContentVersion, Item, ItemKind, ItemVersion, LocalItemId, Placement};

const ROOT: LocalItemId = LocalItemId::ROOT;

/// The row of the file `A`, item 2, stamped at the count `confirmed_at_count`: OneDrive
/// has it under `onedrive_etag`, or does not have it.
fn stamped(confirmed_at_count: u64, onedrive_etag: Option<&str>) -> Item {
    let content = ContentVersion::Hashed { size: 1, mtime: 1000, mtime_nsec: 0, hash: Some("h-1".to_owned()), ctag: Some("c-1".to_owned()) };
    Item {
        local_item_id: LocalItemId::new(2),
        onedrive_item: Some(reference("A")),
        kind: ItemKind::File,
        local: None,
        onedrive: onedrive_etag.map(|_| ItemVersion { place: Some(place(ROOT, "a")), content: Some(content) }),
        onedrive_etag: onedrive_etag.map(str::to_owned),
        onedrive_mime: None,
        synced_place: None,
        synced_content: None,
        content_presence: ContentPresence::NotOnDisk,
        content_before_write: None,
        placement: Placement::Placed,
        is_name_ignored: false,
        confirmed_at_count,
        unsent_since: None,
    }
}

/// The row of the directory `D`, item 3.
fn directory_row() -> Item {
    Item { local_item_id: LocalItemId::new(3), onedrive_item: Some(reference("D")), kind: ItemKind::Directory, onedrive: Some(ItemVersion { place: Some(place(ROOT, "D")), content: None }), ..stamped(0, Some("e-D")) }
}

/// The row of the root, bound to `R`.
fn root_row() -> Item {
    Item { local_item_id: ROOT, onedrive_item: Some(reference("R")), kind: ItemKind::Directory, local: Some(ItemVersion { place: None, content: None }), onedrive: Some(ItemVersion { place: None, content: None }), ..stamped(0, Some("e-R")) }
}

/// What a listing says of the file `A`: in `parent_id`, under the eTag `etag`.
fn listed_file(item_id: &str, parent_id: Option<&str>, etag: &str) -> ListedItem {
    ListedItem {
        item_id: item_id.to_owned(),
        parent_id: parent_id.unwrap_or_default().to_owned(),
        kind: ItemKind::File,
        name: "a".into(),
        content: Some(ContentVersion::Hashed { size: 1, mtime: 1000, mtime_nsec: 0, hash: None, ctag: None }),
        etag: Some(etag.to_owned()),
        mime: None,
        placement: Placement::Placed,
    }
}

/// Whether what a page says an item is, is older than what was confirmed of it, a case
/// each: a row never stamped, or stamped when the fetch began or before, takes the page;
/// one stamped later takes it only when the page says what was confirmed, by its eTag.
#[test]
fn a_listed_version_is_outdated_for_a_row_stamped_later_unless_it_is_what_was_confirmed() {
    for (what, row, listed_etag, outdated) in [
        ("never stamped", stamped(0, Some("e-1")), Some("e-0"), false),
        ("stamped before the fetch began", stamped(4, Some("e-1")), Some("e-0"), false),
        ("stamped when the fetch began", stamped(5, Some("e-1")), Some("e-0"), false),
        ("stamped later, and the page says what was confirmed", stamped(6, Some("e-1")), Some("e-1"), false),
        ("stamped later, and the page says another version", stamped(6, Some("e-1")), Some("e-0"), true),
        ("stamped later, and the page gives no eTag", stamped(6, Some("e-1")), None, true),
        ("stamped later as removed", stamped(6, None), Some("e-0"), true),
        ("stamped later as removed, and the page gives no eTag", stamped(6, None), None, true),
    ] {
        assert_eq!(is_listed_version_outdated(&row, 5, listed_etag), outdated, "{what}");
    }
}

/// Whether a removal a page says is older than what was confirmed of the item, a case
/// each: only for a row stamped after the fetch began that OneDrive was confirmed to have.
#[test]
fn a_listed_removal_is_outdated_for_a_row_stamped_later_that_onedrive_has() {
    for (what, row, outdated) in [
        ("never stamped", stamped(0, Some("e-1")), false),
        ("stamped when the fetch began", stamped(5, Some("e-1")), false),
        ("stamped later as removed", stamped(6, None), false),
        ("stamped later with a version", stamped(6, Some("e-1")), true),
    ] {
        assert_eq!(is_listed_removal_outdated(&row, 5), outdated, "{what}");
    }
}

/// What becomes of a listing's statement that OneDrive has an item, a case each, from what
/// it says and the two rows it is about; a refusal comes before the guard.
#[test]
fn a_listed_item_is_written_outdated_or_refused_by_what_it_says_and_the_rows_it_is_about() {
    use ListedChangeOutcome::{Outdated, Refused, Written};
    use ListedChangeRefusal::{KindChanged, NoItemId, NoParent, ParentIsFile, PlacedInItself, RootGivenPlace};
    let (file, directory, root) = (stamped(0, Some("e-1")), directory_row(), root_row());
    let a_directory = ListedItem { kind: ItemKind::Directory, content: None, ..listed_file("A", Some("D"), "e-2") };
    for (what, listed, row, parent_row, outcome) in [
        ("an item the store does not know, in a directory it does not know", listed_file("A", Some("D"), "e-2"), None, None, Written),
        ("an item the store does not know, in a directory it knows", listed_file("A", Some("D"), "e-2"), None, Some(&directory), Written),
        ("an item the store knows, in the root", listed_file("A", Some("R"), "e-2"), Some(&file), Some(&root), Written),
        ("a parent that was first mentioned and is listed now", ListedItem { item_id: "D".to_owned(), parent_id: "R".to_owned(), ..a_directory.clone() }, Some(&Item { onedrive: None, onedrive_etag: None, ..directory_row() }), Some(&root), Written),
        ("no id", listed_file("", Some("D"), "e-2"), None, Some(&directory), Refused(NoItemId)),
        ("no parent", listed_file("A", None, "e-2"), Some(&file), None, Refused(NoParent)),
        ("in itself", ListedItem { item_id: "D".to_owned(), ..a_directory.clone() }, Some(&directory), Some(&directory), Refused(PlacedInItself)),
        ("the root given a place", ListedItem { item_id: "R".to_owned(), ..a_directory.clone() }, Some(&root), Some(&directory), Refused(RootGivenPlace)),
        ("a parent that is a file", listed_file("B", Some("A"), "e-2"), None, Some(&file), Refused(ParentIsFile)),
        ("a file listed as a directory", a_directory.clone(), Some(&file), Some(&directory), Refused(KindChanged)),
        ("a directory listed as a file", listed_file("D", Some("R"), "e-2"), Some(&directory), Some(&root), Refused(KindChanged)),
        ("stamped after the fetch began, and another version listed", listed_file("A", Some("D"), "e-0"), Some(&stamped(6, Some("e-1"))), Some(&directory), Outdated),
        ("stamped after the fetch began, and what was confirmed listed", listed_file("A", Some("D"), "e-1"), Some(&stamped(6, Some("e-1"))), Some(&directory), Written),
        ("stamped when the fetch began", listed_file("A", Some("D"), "e-0"), Some(&stamped(5, Some("e-1"))), Some(&directory), Written),
        ("stamped later as removed, and listed", listed_file("A", Some("D"), "e-0"), Some(&stamped(6, None)), Some(&directory), Outdated),
        ("of another kind and stamped later: the refusal comes first", a_directory, Some(&stamped(6, Some("e-1"))), Some(&directory), Refused(KindChanged)),
    ] {
        assert_eq!(decide_listed_item(&listed, row, parent_row, 5), outcome, "{what}");
    }
}

/// What becomes of a listing's statement that an item is gone, a case each.
#[test]
fn a_listed_removal_is_written_outdated_or_refused_by_the_row_it_is_about() {
    use LeftToReadAgain::{Nothing, Something};
    use ListedChangeOutcome::{Outdated, Refused, Written};
    for (what, item_id, row, left_below, outcome) in [
        ("an item the store does not know", "A", None, Nothing, Written),
        ("an item the store knows", "A", Some(stamped(0, Some("e-1"))), Nothing, Written),
        ("stamped when the fetch began", "A", Some(stamped(5, Some("e-1"))), Nothing, Written),
        ("stamped later as removed: the removal is what was confirmed", "A", Some(stamped(6, None)), Nothing, Written),
        ("stamped later with a version", "A", Some(stamped(6, Some("e-1"))), Nothing, Outdated),
        ("a directory with a row below it left to be read again: whole or not at all", "D", Some(directory_row()), Something, Outdated),
        ("the same of a row whose removal was confirmed", "A", Some(stamped(6, None)), Something, Outdated),
        ("no id", "", None, Nothing, Refused(ListedChangeRefusal::NoItemId)),
        ("the root", "R", Some(root_row()), Nothing, Refused(ListedChangeRefusal::RootListedGone)),
        ("the root, with a row below it left", "R", Some(root_row()), Something, Refused(ListedChangeRefusal::RootListedGone)),
    ] {
        assert_eq!(decide_listed_removal(item_id, row.as_ref(), 5, left_below), outcome, "{what}");
    }
}

/// What becomes of a listing's statement of the drive's root, a case each.
#[test]
fn a_listed_root_is_written_or_refused_by_the_row_that_has_its_id() {
    use ListedChangeOutcome::{Refused, Written};
    let first_mention = Item { onedrive: None, onedrive_etag: None, ..directory_row() };
    let shown = Item { local: Some(ItemVersion { place: Some(place(ROOT, "D")), content: None }), ..directory_row() };
    for (what, root_item_id, holder, outcome) in [
        ("no row has the id", "R", None, Written),
        ("row 1 has the id", "R", Some(root_row()), Written),
        ("a directory the folder does not show has the id, as a parent named before the root", "D", Some(first_mention), Written),
        ("no id", "", None, Refused(ListedChangeRefusal::NoItemId)),
        ("a file has the id", "A", Some(stamped(0, Some("e-1"))), Refused(ListedChangeRefusal::RootIdOfAnotherItem)),
        ("a directory the folder shows has the id", "D", Some(shown), Refused(ListedChangeRefusal::RootIdOfAnotherItem)),
    ] {
        assert_eq!(decide_listed_root(root_item_id, holder.as_ref()), outcome, "{what}");
    }
}
