use super::*;

/// The review's M1b: a new item of the folder and a new row of OneDrive of its kind under its
/// temporary name are one item, once no listing is under way. A case per answer, and per
/// thing that says the listed row is another item.
#[test]
fn an_item_and_the_row_listed_under_its_temporary_name_are_merged_when_the_row_can_only_be_the_item() {
    let one_item = ListedTemporaryName { is_one_kind: true, is_row_new_in_onedrive: true, is_listing_under_way: false };
    for (what, listed, outcome) in [
        ("a new item and a new row of its kind", one_item, ListedTemporaryNameOutcome::IsMerged),
        ("the same while a listing is under way", ListedTemporaryName { is_listing_under_way: true, ..one_item }, ListedTemporaryNameOutcome::WaitsForListing),
        ("a file and a directory", ListedTemporaryName { is_one_kind: false, ..one_item }, ListedTemporaryNameOutcome::IsAnotherItem),
        ("something is agreed on of the row", ListedTemporaryName { is_row_new_in_onedrive: false, ..one_item }, ListedTemporaryNameOutcome::IsAnotherItem),
        ("another item, while a listing is under way", ListedTemporaryName { is_one_kind: false, is_listing_under_way: true, ..one_item }, ListedTemporaryNameOutcome::IsAnotherItem),
    ] {
        assert_eq!(decide_listed_temporary_name(&listed), outcome, "{what}");
    }
}
