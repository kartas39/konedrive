//! The mount's build and its holder, over the fixture's folder: built, never served.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use konedrive_tree::{ItemPlace, LocalItemId, PlaceChangeRecord, TreeError};
use tokio::sync::oneshot;

use super::{ItemMount, ItemMountError};
use crate::folder::disk::make_aside_entry_name;
use crate::mount::item_download_states::{DownloadAsker, DownloadKey, ItemDownloadStart};
use crate::mount::item_fixture::{DiskMarksRead, ItemFixture};
use crate::mount::item_kernel_requests::ItemKernelRequests;
use crate::local::IgnoreList;

fn build(fixture: &ItemFixture) -> (ItemMount, ItemKernelRequests) {
    fixture.build_mount_with_kernel_requests()
}

/// The permission bits of `path` in the fixture's backing directory.
fn mode_of(fixture: &ItemFixture, path: &str) -> u32 {
    fs::metadata(fixture.disk_path(path)).unwrap().permissions().mode() & 0o777
}

/// The names in the root of the fixture's backing directory.
fn list_root_names(fixture: &ItemFixture) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(fixture.folder()).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[test]
fn the_parts_are_built_in_an_order_in_which_each_has_what_it_is_made_with() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let copy = fixture.show_onedrive_file("docs/copy.txt", b"copy");
    fixture.land_copy(copy.local_item_id, b"copy");
    fixture.make_own_file("own.txt", b"own");
    let rows = fixture.list_items();

    let (mount, _kernel_requests) = build(&fixture);

    assert!(!mount.access_mode_switch().is_read_write(), "a mount starts read-only");
    let findings = mount.start_check_findings();
    assert!(findings.held_items.is_empty() && !findings.has_unread_part, "{findings:?}");
    assert_eq!(fixture.list_items(), rows, "the check found the rows as the disk has them");
    assert!(crate::panic::lock(&fixture.take_downs).is_empty());
    assert!(!mount.item_handles().is_open_for_writing(copy.local_item_id));
    assert!(mount.backing_directory().resolve_root_path().join("docs/copy.txt").is_file());
    // The place changes were made with the id of the thread that was started for them.
    let parts = &mount.parts_to_stop;
    let ran_on = parts.write_operation_thread.run_and_wait(|| std::thread::current().id()).blocking_recv().unwrap();
    assert_eq!(ran_on, parts.write_operation_thread.thread_id());
    // The reading connections are of the store the mount was made with.
    assert_eq!(parts.read_connections.read(|reader| reader.find_item(copy.local_item_id)).unwrap(), Some(fixture.find_item(copy.local_item_id).unwrap()));
}

#[test]
fn the_check_at_start_runs_before_any_part_reads_the_rows() {
    let fixture = ItemFixture::new();
    fixture.write_disk_file("kept.txt", b"kept");
    fixture.write_disk_marks("kept.txt", &DiskMarksRead { onedrive_item_id: Some("KEPT".into()), is_whole: true, ctag: Some("c-kept".into()) });

    let (mount, _kernel_requests) = build(&fixture);

    assert_eq!(mount.start_check_findings().bound_by_mark_count, 1);
    // The first read of a part of the mount sees the row the check made.
    let rows = mount.parts_to_stop.read_connections.read(|reader| reader.list_children(LocalItemId::ROOT)).unwrap();
    let names: Vec<_> = rows.iter().filter_map(|row| row.local.as_ref().and_then(|local| local.place.as_ref())).map(|place| place.name.to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["kept.txt"]);
    assert_eq!(rows[0].onedrive_item.as_ref().map(|reference| reference.item_id.as_str()), Some("KEPT"));
}

#[test]
fn the_entries_still_set_aside_are_removed_after_the_check_and_never_before() {
    let fixture = ItemFixture::new();
    // A removal died between its set-aside and its row: the record stands, the person's
    // file is under its aside name in the root.
    let kept = fixture.make_own_file("kept.txt", b"the person's bytes");
    let aside_name = make_aside_entry_name(kept.local_item_id);
    let record = PlaceChangeRecord::SetAside { item: kept.local_item_id, place: ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "kept.txt".into() }, aside_name: aside_name.clone() };
    fixture.write_rows(move |store| store.item_store().record_place_change(&record));
    fs::rename(fixture.disk_path("kept.txt"), fixture.folder().join(&aside_name)).unwrap();
    // And what a failed discard left: an entry set aside that no record names.
    fixture.write_disk_file(".konedrive-aside-999", b"what a discard left");

    let (mount, _kernel_requests) = build(&fixture);

    assert_eq!(fixture.read_disk_file("kept.txt").as_deref(), Some(&b"the person's bytes"[..]), "the check put the entry back from the record: it was not removed before");
    assert_eq!(list_root_names(&fixture), ["kept.txt"], "what was still set aside after the check is gone");
    assert!(fixture.write_rows(|store| store.item_store().list_place_change_records()).is_empty(), "no record stands");
    assert!(mount.start_check_findings().held_items.is_empty());
    assert_eq!(fixture.find_item(kept.local_item_id), Some(kept), "the row is as it was: the removal did not happen");
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_mount_that_stops_aborts_the_downloads_fails_the_waiters_closes_the_reading_connections_and_drops_the_queued_jobs() {
    let fixture = ItemFixture::new();
    let file = fixture.show_onedrive_file("a.txt", b"a");
    let (mount, _kernel_requests) = build(&fixture);
    let parts = &mount.parts_to_stop;
    // A download that runs, with a waiter.
    let key = DownloadKey { item: file.local_item_id, content: ItemFixture::onedrive_content_of(b"a") };
    let (waiter, waited) = oneshot::channel();
    assert!(matches!(parts.download_states.start_or_join(&key, &DownloadAsker::Read, Some(waiter)), ItemDownloadStart::Started { .. }));
    // A job that waits for the thread, which waits for the tree lock.
    let tree = fixture.tree_lock.blocking_lock();
    let has_run = Arc::new(AtomicBool::new(false));
    let queued = {
        let has_run = Arc::clone(&has_run);
        parts.write_operation_thread.run_and_wait(move || has_run.store(true, Ordering::SeqCst))
    };
    assert!(parts.read_connections.read(|reader| reader.find_item(LocalItemId::ROOT)).is_ok());

    mount.stop();
    drop(tree);

    assert_eq!(waited.blocking_recv(), Ok(Err(libc::EIO)), "the download's waiter is failed");
    assert!(!parts.download_states.is_downloading(file.local_item_id), "the download is aborted");
    assert!(queued.blocking_recv().is_err(), "the job that waited was dropped");
    assert!(!has_run.load(Ordering::SeqCst));
    assert!(parts.read_connections.read(|reader| reader.find_item(LocalItemId::ROOT)).is_err(), "the reading connections are closed");
    assert!(parts.write_operation_thread.run_and_wait(|| ()).blocking_recv().is_err(), "the thread runs no job any more");
    // A second stop does nothing.
    mount.stop();
}

#[test]
fn after_a_stop_and_the_drop_of_the_mount_no_clone_of_the_store_is_held_by_it() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_file("a.txt", b"a");
    let before = fixture.store.count_handles();

    let (mount, kernel_requests) = build(&fixture);
    assert!(fixture.store.count_handles() > before, "the parts hold the store");
    mount.stop();
    drop(mount);
    drop(kernel_requests);

    assert_eq!(fixture.store.count_handles(), before);
}

#[test]
fn a_switch_to_read_only_is_made_on_the_write_operation_thread() {
    let fixture = ItemFixture::new();
    let (mount, _kernel_requests) = build(&fixture);
    assert!(mount.switch_to_read_write(), "it was read-only");
    assert!(!mount.switch_to_read_write(), "and is read-write already");
    // The thread waits for the tree lock: no job of it runs.
    let tree = fixture.tree_lock.blocking_lock();
    fixture.runtime().block_on(async {
        let mut switching = Box::pin(mount.switch_to_read_only());
        assert!(futures_util::poll!(&mut switching).is_pending(), "the switch waits for the thread");
        assert!(mount.access_mode_switch().is_read_write(), "and is not made before the thread gets to it");
        drop(tree);
        assert!(switching.await.expect("the thread runs"), "it was read-write");
    });
    assert!(!mount.access_mode_switch().is_read_write());
    assert!(!fixture.runtime().block_on(mount.switch_to_read_only()).expect("the thread runs"), "and is read-only already");
}

#[test]
fn a_build_that_fails_leaves_the_folder_without_its_write_bits() {
    let fixture = ItemFixture::new();
    fixture.make_own_file("own.txt", b"own");
    // The store fails under the check at start.
    fixture.write_rows(|store| store.bench_sql("DROP TABLE place_change_records"));

    let failed = fixture.try_build_mount().err().expect("the build fails");

    assert!(matches!(failed, ItemMountError::Store(_)), "{failed:?}");
    assert_eq!((mode_of(&fixture, ""), mode_of(&fixture, "own.txt")), (0o555, 0o444));
}

#[test]
fn a_stopped_mount_leaves_the_folder_without_its_write_bits_and_a_forget_gives_them_back() {
    let fixture = ItemFixture::new();
    fixture.make_own_directory("own");
    fixture.make_own_file("own/file.txt", b"own");
    let modes = |fixture: &ItemFixture| (mode_of(fixture, ""), mode_of(fixture, "own"), mode_of(fixture, "own/file.txt"));

    let (mount, _kernel_requests) = build(&fixture);
    assert_eq!(modes(&fixture), (0o755, 0o755, 0o644), "a built mount's folder is the daemon's to change");
    mount.stop();
    assert_eq!(modes(&fixture), (0o555, 0o555, 0o444), "no mount lies over it: nothing is put into it");
    drop(mount);

    let (mount, _kernel_requests) = build(&fixture);
    mount.stop_and_restore_write_bits();
    assert_eq!(modes(&fixture), (0o755, 0o755, 0o644), "a forgotten folder is the person's again");
    drop(mount);
    assert_eq!(modes(&fixture), (0o755, 0o755, 0o644), "the drop of a stopped mount changes nothing");
}

#[test]
fn the_flags_of_ignored_names_are_gone_over_as_a_job_of_the_write_operation_thread() {
    let fixture = ItemFixture::new();
    let notes = fixture.make_own_file("notes.txt", b"notes");
    assert!(!notes.is_name_ignored);
    let (mount, _kernel_requests) = build(&fixture);
    *fixture.ignore.write().unwrap() = IgnoreList::new(["notes.txt"]);

    let tree = fixture.tree_lock.blocking_lock();
    let rewritten = fixture.runtime().block_on(async {
        let mut rewriting = Box::pin(mount.rewrite_ignored_names());
        assert!(futures_util::poll!(&mut rewriting).is_pending(), "the go-over waits for the thread, and so for the tree lock");
        drop(tree);
        rewriting.await
    });

    assert_eq!(rewritten.unwrap(), 1);
    assert!(fixture.find_item(notes.local_item_id).unwrap().is_name_ignored);
    mount.stop();
    assert!(matches!(fixture.runtime().block_on(mount.rewrite_ignored_names()), Err(TreeError::Io(_))), "a stopped mount goes over nothing");
}
