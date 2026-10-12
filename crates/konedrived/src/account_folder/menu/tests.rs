use std::sync::Arc;

use super::*;
use crate::account_folder::folder::{Sync, View, Why};
use crate::account_folder::staged_folder::{make_kept_parts, make_service, stage_folder};
use crate::mount::item_fixture::ItemFixture;

fn taken(is_dir: bool, own: bool, pinned: bool, hydrated: bool) -> Taken {
    Taken { is_dir, own, pinned, hydrated, in_onedrive: true }
}

fn menu_of(paths: &[&str], parts: Vec<(Vec<usize>, Part)>, account_folder: bool) -> Menu {
    decide(&paths.iter().map(|path| (*path).to_owned()).collect::<Vec<_>>(), &parts, account_folder)
}

fn part(taken: Vec<Option<Taken>>) -> Part {
    Part { taken, ..Part::default() }
}

/// "Always keep on this device": hidden with nothing taken, off unless everything is
/// pinned, on when everything is, and locked when a folder above keeps one of them.
#[test]
fn always_keep_is_checked_when_everything_is_pinned_and_locked_by_a_folder_above() {
    let file = |pinned| taken(false, pinned, pinned, true);
    let cases = [
        (vec![None], None, AlwaysKeep::Hidden),
        (vec![Some(file(false))], None, AlwaysKeep::Off),
        (vec![Some(file(true)), Some(file(false))], None, AlwaysKeep::Off),
        (vec![Some(file(true)), Some(file(true))], None, AlwaysKeep::On),
        (vec![Some(taken(false, false, true, true))], Some("/f/Docs"), AlwaysKeep::OnLocked),
    ];
    for (taken, kept_by, always_keep) in cases {
        let paths: Vec<String> = (0..taken.len()).map(|index| format!("/f/{index}")).collect();
        let indices: Vec<usize> = (0..taken.len()).collect();
        let menu = decide(&paths, &[(indices, Part { taken, kept_by: kept_by.map(Into::into), free_up_refused: kept_by.map(|_| FreeUpWhy::PinnedAbove) })], false);
        assert_eq!(menu.always_keep, always_keep);
        assert_eq!(menu.blocked_by, if kept_by.is_some() { "Docs" } else { "" }, "the folder above is named by its name alone");
    }
}

/// "Free up space": offered for a folder, for a file that is downloaded and for a path with
/// a pin of its own to take off; disabled, with why, when `FreeUp` would refuse.
#[test]
fn free_up_is_offered_for_what_has_something_to_free_and_disabled_with_its_reason() {
    let cases = [
        (taken(false, false, false, false), None, Offer::Hidden, None),
        (taken(false, false, false, true), None, Offer::Enabled, None),
        (taken(true, false, false, false), None, Offer::Enabled, None),
        (taken(false, true, true, false), None, Offer::Enabled, None),
        (taken(false, false, false, true), Some(FreeUpWhy::NotUploaded), Offer::Disabled, Some(FreeUpWhy::NotUploaded)),
        (taken(false, false, true, true), Some(FreeUpWhy::PinnedAbove), Offer::Disabled, Some(FreeUpWhy::PinnedAbove)),
    ];
    for (item, refused, offer, why) in cases {
        let kept_by = (refused == Some(FreeUpWhy::PinnedAbove)).then(|| "/f/Docs".into());
        let menu = menu_of(&["/f/a"], vec![(vec![0], Part { taken: vec![Some(item)], kept_by, free_up_refused: refused })], false);
        assert_eq!((menu.free_up, menu.free_up_why), (offer, why), "{item:?} {refused:?}");
    }
}

/// "Open in OneDrive" is for one selected path: an item OneDrive has, or an account's
/// folder itself; never for a selection of several, and hidden for what no folder shows.
#[test]
fn open_online_is_for_one_path_that_onedrive_has_or_an_accounts_folder() {
    let item = taken(false, false, false, false);
    let one = menu_of(&["/f/a"], vec![(vec![0], part(vec![Some(item)]))], false);
    assert_eq!((one.open_online, one.open_online_path.as_str()), (Offer::Enabled, "/f/a"));
    let not_yet = menu_of(&["/f/a"], vec![(vec![0], part(vec![Some(Taken { in_onedrive: false, ..item })]))], false);
    assert_eq!(not_yet.open_online, Offer::Disabled);
    let two = menu_of(&["/f/a", "/f/b"], vec![(vec![0, 1], part(vec![Some(item), Some(item)]))], false);
    assert_eq!((two.open_online, two.open_online_path.as_str()), (Offer::Hidden, ""));
    let account_folder = menu_of(&["/f"], vec![], true);
    assert_eq!((account_folder.open_online, account_folder.open_online_path.as_str(), account_folder.always_keep), (Offer::Enabled, "/f", AlwaysKeep::Hidden));
    let nobody = menu_of(&["/elsewhere"], vec![], false);
    assert_eq!((nobody.open_online, nobody.free_up, nobody.always_keep), (Offer::Hidden, Offer::Hidden, AlwaysKeep::Hidden));
}

/// The paths the menu answers are those `Pin` takes, in the order they were given, whichever
/// account's part says them.
#[test]
fn the_paths_taken_keep_the_order_of_the_selection_across_accounts() {
    let item = taken(false, false, false, true);
    let parts = vec![(vec![2, 0], part(vec![Some(item), Some(item)])), (vec![1, 3], part(vec![None, Some(item)]))];
    let menu = menu_of(&["/a/0", "/b/1", "/a/2", "/b/3"], parts, false);
    assert_eq!(menu.paths, ["/a/0", "/a/2", "/b/3"]);
}

/// An account's share of a selection, asked of its mount: what each path is, whose pin holds
/// for it, whether it is downloaded, and what `FreeUp` would refuse the selection for.
#[test]
fn an_accounts_part_says_each_path_as_the_mount_has_it() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let inside = fixture.show_onedrive_file("docs/a.txt", b"content");
    fixture.land_copy(inside.local_item_id, b"content");
    fixture.show_onedrive_file("online.txt", b"not here");
    let changed = fixture.show_onedrive_file("changed.txt", b"as in onedrive");
    fixture.land_copy(changed.local_item_id, b"as in onedrive");
    fixture.change_file_here(changed.local_item_id, b"changed here");
    let path = |name: &str| fixture.disk_path(name);
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        assert_eq!(service.menu_part(&[path("online.txt")]).await.taken, [None], "no mount: nothing is taken");
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        stage_folder(&service, &fixture.folder(), Sync::Stopped(Why::NotStarted), Some(kept)).await;
        service.pin(&[path("docs")]).await.unwrap();

        let part = service.menu_part(&[path("docs"), path("docs/a.txt"), path("online.txt"), path("no such name")]).await;
        assert_eq!(part.taken, [Some(taken(true, true, true, false)), Some(taken(false, false, true, true)), Some(taken(false, false, false, false)), None]);
        assert_eq!(part.kept_by, Some(path("docs")), "the folder above that pins one of them");
        assert_eq!(part.free_up_refused, Some(FreeUpWhy::PinnedAbove));

        let part = service.menu_part(&[path("online.txt"), path("changed.txt")]).await;
        assert_eq!((part.kept_by, part.free_up_refused), (None, Some(FreeUpWhy::NotUploaded)));
        let part = service.menu_part(&[path("online.txt")]).await;
        assert_eq!((part.kept_by, part.free_up_refused), (None, None));

        let kept = service.folder.write().await.record_mut().and_then(|record| record.kept.parts.take());
        service.view.send_replace(View::default());
        tokio::task::spawn_blocking(move || drop((kept, mount))).await.unwrap();
    });
}
