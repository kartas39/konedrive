use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_tree::{ItemKind, LocalItemId};

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

fn status(pin_holder: PinHolder) -> ItemPathStatus {
    ItemPathStatus {
        item: LocalItemId::new(7),
        kind: ItemKind::File,
        onedrive_item: ItemFixture::onedrive_reference_of("a.txt"),
        sync_state: None,
        is_pinned: pin_holder != PinHolder::Nobody,
        pin_holder,
        is_downloading: false,
        is_open_for_writing: false,
    }
}

/// What `FreeUp` refuses a path for, by what a free-up decides once the path's own pin is
/// off: a pin that still holds is a directory's above, which the refusal names; a change
/// not uploaded would be lost; everything else is the free-up's own to leave or free.
#[test]
fn a_free_up_is_refused_for_a_pin_from_above_and_for_a_change_that_is_not_uploaded() {
    let path = Path::new("/f/Docs/a.txt");
    let above = PinHolder::DirectoryAbove { path: PathBuf::from("/f/Docs") };
    let pinned_above = refuse_free_up(path, &status(above.clone()), FreeUpDecision::LeavePinned).unwrap_err();
    assert!(matches!(&pinned_above, SyncError::NotAllowed(why) if *why == refusal(path, Path::new("/f/Docs"))), "{pinned_above:?}");
    // A pin of the path's own names the path itself (`Dehydrate`, which takes no pin off).
    for own in [PinHolder::Itself, PinHolder::Nobody] {
        let pinned = refuse_free_up(path, &status(own), FreeUpDecision::LeavePinned).unwrap_err();
        assert!(matches!(&pinned, SyncError::NotAllowed(why) if *why == refusal(path, path)), "{pinned:?}");
    }
    for decision in [FreeUpDecision::LeaveModifiedHere, FreeUpDecision::LeaveOpenForWriting] {
        let refused = refuse_free_up(path, &status(above.clone()), decision).unwrap_err();
        assert!(matches!(&refused, SyncError::NotUploaded(named) if named == "/f/Docs/a.txt"), "{decision:?}: {refused:?}");
    }
    for decision in [FreeUpDecision::Free, FreeUpDecision::LeaveDownloading, FreeUpDecision::NothingToFree] {
        assert!(refuse_free_up(path, &status(PinHolder::Nobody), decision).is_ok(), "{decision:?}");
    }
}

#[test]
fn the_detail_of_a_freed_event_counts_files_and_says_the_size() {
    assert_eq!(freed_detail(1, 0), format!("1 file, {}", activity::human_size(0)));
    assert_eq!(freed_detail(2, 1 << 20), format!("2 files, {}", activity::human_size(1 << 20)));
}

/// Freeing up through the folder's mount: a clean copy is freed and counted; a file with a
/// change that is not uploaded is refused, by `Dehydrate` and by `FreeUp`'s check, before
/// anything is freed; a file with nothing on disk has nothing to free; a path with a pin of
/// its own loses the pin and is freed.
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
        assert!(matches!(service.dehydrate(&path("online.txt")).await, Err(SyncError::NotHydrated)));

        service.pin(&[path("pinned.txt")]).await.unwrap();
        let refused = service.dehydrate(&path("pinned.txt")).await.unwrap_err();
        assert!(matches!(&refused, SyncError::NotAllowed(why) if *why == refusal(&path("pinned.txt"), &path("pinned.txt"))), "{refused:?}");

        // `FreeUp` takes the path's own pin off first, and then frees.
        let freed = service.free_up(&[path("pinned.txt"), path("clean.txt"), path("online.txt")]).await.unwrap();
        assert_eq!((freed.files, freed.bytes > 0, freed.pinned), (2, true, 0));
        assert_eq!(service.state().get().local.pinned_count, 0, "the pin came off");
        for freed in ["pinned.txt", "clean.txt"] {
            assert_eq!(fixture.find_item_at(freed).unwrap().content_presence, konedrive_tree::ContentPresence::NotOnDisk, "{freed}");
        }
        end_scene(&service, mount).await;
    });
}
