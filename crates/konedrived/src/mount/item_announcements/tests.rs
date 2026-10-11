use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::*;

fn item(number: i64) -> LocalItemId {
    LocalItemId::new(number)
}

fn place(parent: i64, name: &str) -> ItemPlace {
    ItemPlace { parent_local_item_id: item(parent), name: name.into() }
}

#[test]
fn a_name_that_is_vanishing_is_answered_with_the_item_that_had_it() {
    for kind in [ItemKind::File, ItemKind::Directory] {
        let vanished = ItemAnnouncement::Vanished { item: item(7), former_place: place(3, "old"), kind };
        assert_eq!(find_announced_name(Some(&vanished), item(3), OsStr::new("old")), Some(AnnouncedName::Vanishing { item: item(7), kind }));
    }
}

#[test]
fn a_name_that_is_appearing_is_not_there_yet_for_the_bridge() {
    let appeared = ItemAnnouncement::Appeared { item: item(7), place: place(3, "new") };
    assert_eq!(find_announced_name(Some(&appeared), item(3), OsStr::new("new")), Some(AnnouncedName::Appearing));
}

#[test]
fn any_other_name_is_answered_as_for_everyone() {
    let vanished = ItemAnnouncement::Vanished { item: item(7), former_place: place(3, "old"), kind: ItemKind::File };
    let appeared = ItemAnnouncement::Appeared { item: item(7), place: place(3, "new") };
    assert_eq!(find_announced_name(None, item(3), OsStr::new("old")), None, "nothing is being announced");
    for in_progress in [&vanished, &appeared] {
        assert_eq!(find_announced_name(Some(in_progress), item(3), OsStr::new("other")), None, "another name of the directory");
        assert_eq!(find_announced_name(Some(in_progress), item(4), OsStr::new("old")), None, "the name in another directory");
        assert_eq!(find_announced_name(Some(in_progress), item(4), OsStr::new("new")), None);
        assert_eq!(find_announced_name(Some(in_progress), item(3), OsStr::new("OLD")), None, "a name is compared as it was given");
    }
    // The directories above the name are looked up on the bridge's thread too.
    assert_eq!(find_announced_name(Some(&appeared), LocalItemId::ROOT, OsStr::new("docs")), None);
    // A file whose content changed is there before and after: answered from its row.
    let changed = ItemAnnouncement::ContentChanged { item: item(7), place: place(3, "new") };
    assert_eq!(find_announced_name(Some(&changed), item(3), OsStr::new("new")), None);
}

#[test]
fn an_item_moved_is_one_vanishing_and_one_appearing() {
    let announcements = describe_move(item(7), ItemKind::Directory, place(3, "old"), place(4, "new"));
    assert_eq!(announcements, [ItemAnnouncement::Vanished { item: item(7), former_place: place(3, "old"), kind: ItemKind::Directory }, ItemAnnouncement::Appeared { item: item(7), place: place(4, "new") }], "the vanishing first");
}

#[test]
fn each_announcement_is_one_system_call_at_the_path_of_its_place() {
    let docs = || Some(PathBuf::from("docs"));
    let appeared = ItemAnnouncement::Appeared { item: item(7), place: place(3, "new") };
    assert_eq!(decide_system_call(&appeared, docs(), Some(ItemKind::File)), Some(AnnouncementSystemCall::Mknod("docs/new".into())));
    assert_eq!(decide_system_call(&appeared, docs(), Some(ItemKind::Directory)), Some(AnnouncementSystemCall::Mkdir("docs/new".into())), "the kind of an appearing is the row's");
    let vanished = |kind| ItemAnnouncement::Vanished { item: item(7), former_place: place(3, "old"), kind };
    // The rows may not have the item any more, or have it elsewhere: the kind travels.
    for kind_now in [None, Some(ItemKind::File), Some(ItemKind::Directory)] {
        assert_eq!(decide_system_call(&vanished(ItemKind::File), docs(), kind_now), Some(AnnouncementSystemCall::Unlink("docs/old".into())));
        assert_eq!(decide_system_call(&vanished(ItemKind::Directory), docs(), kind_now), Some(AnnouncementSystemCall::Rmdir("docs/old".into())));
    }
    let changed = ItemAnnouncement::ContentChanged { item: item(7), place: place(3, "new") };
    assert_eq!(decide_system_call(&changed, docs(), Some(ItemKind::File)), Some(AnnouncementSystemCall::Utimensat("docs/new".into())));
    // In the root the path is the name; a name that is not UTF-8 stays as it was given.
    assert_eq!(decide_system_call(&appeared, Some(PathBuf::new()), Some(ItemKind::File)), Some(AnnouncementSystemCall::Mknod("new".into())));
    let odd_name = OsStr::from_bytes(b"caf\xe9");
    let odd = ItemAnnouncement::Appeared { item: item(7), place: ItemPlace { parent_local_item_id: item(3), name: odd_name.to_owned() } };
    assert_eq!(decide_system_call(&odd, docs(), Some(ItemKind::File)), Some(AnnouncementSystemCall::Mknod(Path::new("docs").join(odd_name))));
}

#[test]
fn an_announcement_whose_directory_or_item_the_rows_no_longer_show_is_no_system_call() {
    let appeared = ItemAnnouncement::Appeared { item: item(7), place: place(3, "new") };
    let vanished = ItemAnnouncement::Vanished { item: item(7), former_place: place(3, "old"), kind: ItemKind::File };
    let changed = ItemAnnouncement::ContentChanged { item: item(7), place: place(3, "new") };
    for announcement in [&appeared, &vanished, &changed] {
        assert_eq!(decide_system_call(announcement, None, Some(ItemKind::File)), None, "the directory is gone: the name went with it");
    }
    assert_eq!(decide_system_call(&appeared, Some("docs".into()), None), None, "the item is gone again: its vanishing is announced by itself");
    assert_eq!(decide_system_call(&changed, Some("docs".into()), None), None);
    assert_eq!(decide_system_call(&changed, Some("docs".into()), Some(ItemKind::Directory)), None, "a directory has no content");
}
