use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::account_folder::folder::{Sync, View, Why};
use crate::account_folder::staged_folder::{make_kept_parts, make_service, stage_folder};
use crate::mount::item_fixture::ItemFixture;

/// The scene's end: the view and the folder let go of the mount, which is dropped where a
/// thread may be joined.
async fn end_scene(service: &SyncService, mount: Arc<ItemMount>) {
    let kept = service.folder.write().await.record_mut().and_then(|record| record.kept.parts.take());
    service.view.send_replace(View::default());
    tokio::task::spawn_blocking(move || drop((kept, mount))).await.unwrap();
}

#[test]
fn the_refusal_names_the_path_refused_and_what_pins_it() {
    assert_eq!(refusal(Path::new("/f/Docs/a b.txt"), Path::new("/f/Docs")), "/f/Docs/a b.txt is pinned by /f/Docs: unpin it first");
    assert_eq!(refusal(Path::new("/f/a.txt"), Path::new("/f/a.txt")), "/f/a.txt is pinned by /f/a.txt: unpin it first");
}

/// Pins through the folder's mount, kept by a sync that does not run: a pin is counted when
/// it changed, `PinnedCount` follows, and a path a directory above pins is left as it is by
/// `Pin` and refused by `Unpin`, naming that directory.
#[test]
fn a_pin_is_put_on_once_counted_and_a_path_pinned_from_above_is_refused_by_unpin() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    fixture.show_onedrive_file("docs/a.txt", b"content");
    fixture.show_onedrive_file("b.txt", b"other");
    let (docs, a, b) = (fixture.disk_path("docs"), fixture.disk_path("docs/a.txt"), fixture.disk_path("b.txt"));
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        stage_folder(&service, &fixture.folder(), Sync::Stopped(Why::NotStarted), Some(kept)).await;

        assert_eq!(service.pin(&[docs.clone(), b.clone()]).await.unwrap(), 2);
        assert_eq!(service.state().get().local.pinned_count, 2);
        assert_eq!(service.pin(&[docs.clone()]).await.unwrap(), 0, "pinned already");
        assert_eq!(service.pin(&[a.clone()]).await.unwrap(), 0, "a directory above pins it: left as it is");
        assert_eq!(service.state().get().local.pinned_count, 2);

        let refused = service.unpin(&[b.clone(), a.clone()]).await.unwrap_err();
        assert!(matches!(&refused, SyncError::NotAllowed(why) if *why == refusal(&a, &docs)), "{refused:?}");
        assert_eq!(service.state().get().local.pinned_count, 2, "every path is checked before any pin comes off");
        let refused = service.check_free_up(&[a.clone()]).await.unwrap_err();
        assert!(matches!(&refused, SyncError::NotAllowed(why) if *why == refusal(&a, &docs)), "{refused:?}");

        assert_eq!(service.unpin(&[docs.clone(), b.clone()]).await.unwrap(), 2);
        assert_eq!(service.state().get().local.pinned_count, 0);
        assert_eq!(service.unpin(&[docs.clone()]).await.unwrap(), 0, "no pin was there");

        // A path the folder does not show refuses the whole call before any pin is put on.
        let outside = fixture.directory.path().join("elsewhere.txt");
        assert!(matches!(service.pin(&[b.clone(), outside]).await, Err(SyncError::OutsideRoot)));
        assert!(matches!(service.pin(&[b.clone(), fixture.disk_path("no such name")]).await, Err(SyncError::NotManaged)));
        assert_eq!(service.state().get().local.pinned_count, 0);
        end_scene(&service, mount).await;
    });
}

#[tokio::test]
async fn a_pin_needs_a_folder_and_its_mount() {
    let (service, _clock) = make_service();
    assert!(matches!(service.pin(&["/f/a.txt".into()]).await, Err(SyncError::NoRoot)));
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), Sync::Stopped(Why::CannotStart("it cannot".into())), None).await;
    let refused = service.pin(&[folder.path().join("a.txt")]).await.unwrap_err();
    assert!(matches!(&refused, SyncError::NotUp(why) if why.contains("it cannot")), "{refused:?}");
}
