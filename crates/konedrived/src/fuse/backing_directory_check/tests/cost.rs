//! What the check costs, measured once in a debug build: 100 000 files in 5 000
//! directories, with nothing changed and with one file in a hundred changed. Not run with
//! the other tests: `cargo test -p konedrived --lib backing_directory_check::tests::cost --
//! --ignored --nocapture`.

use std::fs;
use std::time::Instant;

use konedrive_fs::placeholder::{State, XATTR_CTAG, XATTR_ITEM_ID, XATTR_STATE};
use konedrive_tree::{ContentVersion, ItemKind};

use super::super::backing_directory_contents::BackingDirectoryContents;
use super::super::entry_identities::EntryIdentities;
use super::super::item_rows::ItemRows;
use super::super::path_findings::PathFindings;
use super::{folder, found, DiskFinding, MTIME};

const DIRECTORIES: usize = 5_000;
const FILES_IN_EACH: usize = 20;

#[test]
#[ignore = "a measurement, run once by name"]
fn the_cost_of_a_check_of_a_hundred_thousand_files() {
    let mut f = folder();
    let built = Instant::now();
    // Fifty directories of a hundred directories of twenty files: half of the files clean
    // copies of OneDrive's, half the user's own.
    for top in 0..DIRECTORIES / 100 {
        let top_rel = format!("top-{top}");
        f.make_directory(&top_rel);
        f.make_row(&top_rel, ItemKind::Directory, None);
        for n in 0..100 {
            let directory = format!("{top_rel}/directory-{n}");
            f.make_directory(&directory);
            let id = f.make_row(&directory, ItemKind::Directory, None);
            xattr::set(f.disk(&directory), XATTR_ITEM_ID, format!("I{id}").as_bytes()).unwrap();
            for file in 0..FILES_IN_EACH {
                let rel = format!("{directory}/file-{file}");
                f.write(&rel, b"0123456789");
                let content = ContentVersion::ChangedHere { size: 10, mtime: MTIME, mtime_nsec: 0, change_count: 1 };
                let id = f.make_row(&rel, ItemKind::File, Some(content));
                if file % 2 == 0 {
                    let path = f.disk(&rel);
                    xattr::set(&path, XATTR_ITEM_ID, format!("I{id}").as_bytes()).unwrap();
                    xattr::set(&path, XATTR_STATE, State::Hydrated.as_str().as_bytes()).unwrap();
                    xattr::set(&path, XATTR_CTAG, format!("c-I{id}").as_bytes()).unwrap();
                }
            }
        }
    }
    // What is marked is OneDrive's and agreed on, written as it is: no operation of the
    // store writes what was agreed on yet.
    f.store
        .bench_sql(
            "UPDATE items SET onedrive_drive_id = 'd1', onedrive_item_id = 'I' || local_item_id, synced_parent_id = local_parent_id, synced_name = local_name
               WHERE local_item_id != 1 AND (kind = 'directory' AND local_name LIKE 'directory-%' OR kind = 'file' AND CAST(substr(local_name, 6) AS INTEGER) % 2 = 0);
             UPDATE items SET local_hash = 'h', local_ctag = 'c-I' || local_item_id, local_change_count = NULL,
                              synced_size = local_size, synced_mtime = local_mtime, synced_mtime_nsec = 0, synced_hash = 'h', synced_ctag = 'c-I' || local_item_id
               WHERE kind = 'file' AND onedrive_item_id IS NOT NULL;",
        )
        .unwrap();
    let rows = f.rows().len();
    eprintln!("built {rows} rows and their entries in {:?}", built.elapsed());
    assert_eq!(rows, 1 + DIRECTORIES / 100 + DIRECTORIES + DIRECTORIES * FILES_IN_EACH);

    // The parts of a run, each by itself: nothing is written, so they can be run apart.
    let started = Instant::now();
    let read = ItemRows::new(f.store.item_store().list_items().unwrap());
    eprintln!("the rows read whole and put by id, place and binding: {:?}", started.elapsed());
    let started = Instant::now();
    let contents = BackingDirectoryContents::read(&f.backing_directory);
    eprintln!("the backing directory read, {} entries: {:?}", contents.len(), started.elapsed());
    let started = Instant::now();
    let identities = EntryIdentities::settle(&read, &contents, &f.drive_id);
    eprintln!("each entry matched to its item: {:?}", started.elapsed());
    let started = Instant::now();
    let findings = PathFindings::decide(&read, &contents, &identities);
    eprintln!("each path decided: {:?}", started.elapsed());
    assert_eq!(findings.list_rows(), []);

    for run in ["first", "second", "third"] {
        let started = Instant::now();
        let findings = f.check();
        eprintln!("nothing changed ({run}): {:?}", started.elapsed());
        assert_eq!(found(&findings), [(DiskFinding::NothingChanged, rows - 1)]);
    }

    // One file in a hundred edited by hand: the fifth file of every fifth directory.
    let mut edited = 0;
    for top in 0..DIRECTORIES / 100 {
        for n in (0..100).step_by(5) {
            let path = f.disk(format!("top-{top}/directory-{n}/file-{}", 4 + n % 2));
            // In place: the marks stay.
            fs::write(&path, b"edited by hand").unwrap();
            edited += 1;
        }
    }
    let started = Instant::now();
    let findings = f.check();
    eprintln!("{edited} of {} files changed: {:?}", DIRECTORIES * FILES_IN_EACH, started.elapsed());
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, rows - 1 - edited), (DiskFinding::ContentChanged, edited)]);
    let started = Instant::now();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, rows - 1)]);
    eprintln!("nothing changed, after it: {:?}", started.elapsed());
}
