use crate::onedrive_steps::tests::StepsFolder;
use crate::mount::item_fixture::ItemFixture;

use super::*;

#[test]
fn the_fate_of_a_made_item_is_by_its_row() {
    let fixture = ItemFixture::new();
    let row = fixture.make_own_file("a.txt", b"a");
    assert_eq!(decide_made_item_fate(Some(&row)), MadeItemFate::LeaveForListing);
    assert_eq!(decide_made_item_fate(None), MadeItemFate::Remove);
}

/// What a step made and its commit did not write: the item, as the fake has it.
fn confirmed(folder: &StepsFolder, path: &str) -> ConfirmedItem {
    let answered = folder.graph.with(|cloud| cloud.at(path).cloned()).expect("the made item");
    ConfirmedItem {
        item: ItemFixture::onedrive_reference_of(&answered.id),
        parent: ItemFixture::onedrive_reference_of(""),
        name: answered.name.clone().into(),
        content: (!answered.folder).then(|| ItemFixture::onedrive_content_of(&answered.content)),
        etag: answered.etag,
    }
}

#[tokio::test]
async fn a_new_item_whose_row_is_gone_is_removed_in_onedrive() {
    let fixture = ItemFixture::new();
    let (file, directory) = (fixture.make_own_file("a.txt", b"a"), fixture.make_own_directory("new"));
    let (file_versions, directory_versions) = (fixture.versions_of(file.local_item_id), fixture.versions_of(directory.local_item_id));
    let folder = StepsFolder::start(fixture).await;
    folder.graph.with(|cloud| {
        cloud.add_file("MADE", "R", "a.txt", b"a");
        cloud.add(crate::fake_onedrive::FakeItem { id: "MADEDIR".into(), parent: Some("R".into()), name: "new".into(), folder: true, content: Vec::new(), hash: None, size: 0, etag: "e-d".into(), ctag: "c-d".into(), mtime: 0 });
    });
    let (made_file, made_directory) = (confirmed(&folder, "a.txt"), confirmed(&folder, "new"));
    folder.fixture.remove_here(file.local_item_id);
    folder.fixture.remove_here(directory.local_item_id);

    assert_eq!(folder.made_items.follow_up_new_item(&file_versions, &made_file).await, RowsChangedFollowUp::MadeItemRemoved(made_file.item.clone()));
    assert_eq!(folder.made_items.follow_up_new_item(&directory_versions, &made_directory).await, RowsChangedFollowUp::MadeItemRemoved(made_directory.item.clone()));
    assert_eq!(folder.graph.with(|cloud| cloud.paths()), Vec::<String>::new());
    // The file's removal is guarded by the eTag of the answer; a directory's has no guard.
    assert_eq!(folder.graph.with(|cloud| cloud.guards.clone()), [("me/drive/items/MADE".to_owned(), made_file.etag.clone())]);
}

#[tokio::test]
async fn a_new_item_whose_row_is_still_there_asks_for_a_cycle() {
    let fixture = ItemFixture::new();
    let file = fixture.make_own_file("a.txt", b"a");
    let versions = fixture.versions_of(file.local_item_id);
    let folder = StepsFolder::start(fixture).await;
    folder.graph.with(|cloud| cloud.add_file("MADE", "R", "a.txt", b"a"));
    let made = confirmed(&folder, "a.txt");

    assert_eq!(folder.made_items.follow_up_new_item(&versions, &made).await, RowsChangedFollowUp::CycleWanted);
    assert_eq!(folder.graph.with(|cloud| cloud.paths()), ["a.txt"]);
}

#[tokio::test]
async fn a_made_item_that_cannot_be_removed_is_left_and_said() {
    let fixture = ItemFixture::new();
    let file = fixture.make_own_file("a.txt", b"a");
    let versions = fixture.versions_of(file.local_item_id);
    let folder = StepsFolder::start(fixture).await;
    folder.graph.with(|cloud| cloud.add_file("MADE", "R", "a.txt", b"a"));
    let made = confirmed(&folder, "a.txt");
    folder.fixture.remove_here(file.local_item_id);
    // Another changed it since it was made: the removal is answered 412.
    folder.graph.with(|cloud| cloud.edit("MADE", b"another's"));

    assert_eq!(folder.made_items.follow_up_new_item(&versions, &made).await, RowsChangedFollowUp::MadeItemLeft(made.item.clone()));
    assert_eq!(folder.graph.with(|cloud| cloud.paths()), ["a.txt"]);
}
