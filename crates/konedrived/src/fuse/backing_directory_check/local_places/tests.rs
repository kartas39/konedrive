use konedrive_tree::ItemKind;

use super::LocalPlaces;
use crate::fuse::backing_directory_check::item_rows::tests::{bound, id, own, removed_here};

fn places() -> LocalPlaces {
    let rows = [own(2, 1, "docs", ItemKind::Directory), own(3, 2, "b.txt", ItemKind::File), own(4, 2, "a.txt", ItemKind::File), removed_here(bound("X", 5, 1, "gone.txt", ItemKind::File))];
    LocalPlaces::new(rows.iter())
}

#[test]
fn the_places_begin_as_the_rows_the_folder_shows_have_them() {
    let places = places();
    assert_eq!(places.find_at(id(2), "a.txt".as_ref()), Some(id(4)));
    assert_eq!(places.find_place(id(3)), Some((id(2), "b.txt".as_ref())));
    assert!(places.is_at(id(2), id(1), "docs".as_ref()) && !places.is_at(id(2), id(1), "other".as_ref()));
    assert_eq!(places.list_inside(id(2)), [id(4), id(3)], "by name");
    assert_eq!((places.find_place(id(5)), places.find_at(id(1), "gone.txt".as_ref())), (None, None), "the folder does not show it");
}

#[test]
fn an_item_that_is_placed_leaves_the_place_it_had_and_one_that_is_removed_has_none() {
    let mut places = places();
    places.set(id(3), id(1), "moved.txt".as_ref());
    assert_eq!((places.find_at(id(2), "b.txt".as_ref()), places.find_at(id(1), "moved.txt".as_ref())), (None, Some(id(3))));
    assert_eq!(places.list_inside(id(2)), [id(4)]);
    places.set(id(9), id(2), "new.txt".as_ref());
    assert_eq!(places.find_place(id(9)), Some((id(2), "new.txt".as_ref())));
    places.remove(id(4));
    assert_eq!((places.find_place(id(4)), places.find_at(id(2), "a.txt".as_ref())), (None, None));
    places.remove(id(4));
    assert_eq!(places.list_inside(id(2)), [id(9)]);
}
