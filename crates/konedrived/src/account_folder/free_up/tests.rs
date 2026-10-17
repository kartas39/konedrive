use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::account_folder::folder::{Sync, View, Why};
use crate::account_folder::staged_folder::{make_kept_parts, make_service, stage_folder};
use crate::mount::item_fixture::ItemFixture;
use crate::mount::ItemMount;

async fn end_scene(service: &SyncService, mount: Arc<ItemMount>) {
    let kept = service.folder.write().await.record_mut().and_then(|record| record.kept.parts.take());
    service.view.send_replace(View::default());
    tokio::task::spawn_blocking(move || drop((kept, mount))).await.unwrap();
}

/// What `FreeUp` refuses a path for, by what a free-up decides once no pin holds for the
/// path: a change not uploaded would be lost; everything else is the free-up's own to
/// leave or free.
#[test]
fn a_free_up_is_refused_for_a_change_that_is_not_uploaded() {
    let path = Path::new("/f/Docs/a.txt");
    for decision in [FreeUpDecision::LeaveModifiedHere, FreeUpDecision::LeaveOpenForWriting] {
        let refused = refuse_free_up(path, decision).unwrap_err();
        assert!(matches!(&refused, SyncError::NotUploaded(named) if named == "/f/Docs/a.txt"), "{decision:?}: {refused:?}");
    }
    for decision in [FreeUpDecision::Free, FreeUpDecision::LeavePinned, FreeUpDecision::LeaveDownloading, FreeUpDecision::NothingToFree] {
        assert!(refuse_free_up(path, decision).is_ok(), "{decision:?}");
    }
}

#[test]
fn the_detail_of_a_freed_event_counts_files_and_says_the_size() {
    assert_eq!(freed_detail(1, 0), format!("1 file, {}", activity::human_size(0)));
    assert_eq!(freed_detail(2, 1 << 20), format!("2 files, {}", activity::human_size(1 << 20)));
}

/// Freeing up through the folder's mount: a clean copy is freed and counted; a file with a
/// change that is not uploaded is refused, by `Dehydrate` and by `FreeUp`'s check, before
/// anything is freed; a file with nothing on disk is already freed; a pinned path loses
/// the pin and is freed.
#[test]
fn a_clean_copy_is_freed_and_a_change_that_is_not_uploaded_refuses_the_free_up() {
    let fixture = ItemFixture::new();
    let clean = fixture.show_onedrive_file("clean.txt", b"clean content");
    fixture.land_copy(clean.local_item_id, b"clean content");
    let changed = fixture.show_onedrive_file("changed.txt", b"as in onedrive");
    fixture.land_copy(changed.local_item_id, b"as in onedrive");
    fixture.change_file_here(changed.local_item_id, b"changed here, longer");
    fixture.show_onedrive_file("online.txt", b"not here");
    let pinned = fixture.show_onedrive_file("pinned.txt", b"kept here");
    fixture.land_copy(pinned.local_item_id, b"kept here");
    let path = |name: &str| fixture.disk_path(name);
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        stage_folder(&service, &fixture.folder(), Sync::Stopped(Why::NotStarted), Some(kept)).await;

        let refused = service.dehydrate(&path("changed.txt")).await.unwrap_err();
        assert!(matches!(&refused, SyncError::NotUploaded(named) if *named == path("changed.txt").display().to_string()), "{refused:?}");
        assert!(matches!(service.check_free_up(&[path("clean.txt"), path("changed.txt")]).await, Err(SyncError::NotUploaded(_))));
        assert!(matches!(service.free_up(&[path("clean.txt"), path("changed.txt")]).await, Err(SyncError::NotUploaded(_))));
        assert!(fixture.read_disk_file("clean.txt").is_some(), "nothing was freed by a call that was refused");
        // A file that is not on disk is already freed: answered, and nothing changes.
        service.dehydrate(&path("online.txt")).await.unwrap();
        assert_eq!(fixture.find_item_at("online.txt").unwrap().content_presence, konedrive_tree::ContentPresence::NotOnDisk);

        service.pin(&[path("pinned.txt")]).await.unwrap();
        let refused = service.dehydrate(&path("pinned.txt")).await.unwrap_err();
        assert!(matches!(&refused, SyncError::NotAllowed(why) if *why == refusal(&path("pinned.txt"))), "{refused:?}");

        // `FreeUp` takes the pin off first, and then frees.
        let freed = service.free_up(&[path("pinned.txt"), path("clean.txt"), path("online.txt")]).await.unwrap();
        assert_eq!((freed.files, freed.bytes > 0, freed.pinned), (2, true, 0));
        assert_eq!(service.state().get().local.pinned_count, 0, "the pin came off");
        for freed in ["pinned.txt", "clean.txt"] {
            assert_eq!(fixture.find_item_at(freed).unwrap().content_presence, konedrive_tree::ContentPresence::NotOnDisk, "{freed}");
        }
        end_scene(&service, mount).await;
    });
}

/// A file in a pinned folder is freed up on its own: it gets an exception, its copy goes,
/// and the folder stays pinned with the rest of it kept. Pinned again, the exception is
/// off. A free-up of the folder lets all of it go, a file pinned by itself in it too.
#[test]
fn a_file_in_a_pinned_folder_is_freed_up_and_the_folder_stays_pinned() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    for name in ["docs/big.iso", "docs/kept.txt"] {
        let file = fixture.show_onedrive_file(name, b"content");
        fixture.land_copy(file.local_item_id, b"content");
    }
    let path = |name: &str| fixture.disk_path(name);
    let is_on_disk = |name: &str| fixture.find_item_at(name).unwrap().content_presence == konedrive_tree::ContentPresence::OnDisk;
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        stage_folder(&service, &fixture.folder(), Sync::Stopped(Why::NotStarted), Some(kept)).await;
        let is_pinned = |name: &'static str| {
            let (mount, path) = (Arc::clone(&mount), path(name));
            async move { mount.files_interface().find_path_status(&path).await.unwrap().is_pinned }
        };

        service.pin(&[path("docs")]).await.unwrap();
        service.check_free_up(&[path("docs/big.iso")]).await.expect("a pin above refuses nothing");
        let freed = service.free_up(&[path("docs/big.iso")]).await.unwrap();
        assert_eq!((freed.files, freed.pinned), (1, 0));
        assert!(!is_on_disk("docs/big.iso") && is_on_disk("docs/kept.txt"));
        assert_eq!([is_pinned("docs").await, is_pinned("docs/big.iso").await, is_pinned("docs/kept.txt").await], [true, false, true]);
        assert_eq!(service.state().get().local.pinned_count, 1, "the folder's pin; an exception is none");

        // "Always keep on this device" on the file takes the exception off.
        assert_eq!(service.pin(&[path("docs/big.iso")]).await.unwrap(), 1);
        assert!(is_pinned("docs/big.iso").await);
        assert_eq!(service.state().get().local.pinned_count, 1);

        // The folder is let go whole.
        let freed = service.free_up(&[path("docs")]).await.unwrap();
        assert_eq!((freed.files, freed.pinned), (1, 0));
        assert_eq!([is_pinned("docs").await, is_pinned("docs/kept.txt").await, is_on_disk("docs/kept.txt")], [false, false, false]);
        assert_eq!(service.state().get().local.pinned_count, 0);
        end_scene(&service, mount).await;
    });
}
