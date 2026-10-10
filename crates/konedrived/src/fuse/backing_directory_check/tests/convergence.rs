//! Convergence: rows and a directory that agree; edits made by hand at random; the check;
//! then
//!
//! - no row is removed whose item OneDrive has, and none is moved: the bindings there were
//!   are all there, each its item's, and such an item is at most not shown any more, with
//!   nothing to be sent ([`assert_no_item_of_onedrive_is_removed_or_moved`]);
//! - every entry on disk has a row at its path, of its kind;
//! - a second run changes nothing, in the rows or on disk.
//!
//! One round in four begins with a check that cannot read a directory chosen at random:
//! the rows in it and below it are as they were.
//!
//! The edits are what a person does with a file manager and a shell while no daemon runs:
//! make, write, move (also over a file), remove, copy with the marks (`cp -a`, of a file
//! and of a directory), a second name of a file (`ln`), an older copy put back with its
//! marks, a file or a directory put at the name of a file that is not downloaded. Between
//! the rounds the rows are changed as a daemon would: an item of the user's own is sent
//! and agreed on, and a file of OneDrive that is not downloaded appears.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use konedrive_tree::{Item, ItemKind, LocalItemId};

use super::unread_parts::is_root;
use super::{assert_no_item_of_onedrive_is_removed_or_moved, bindings, folder, left, set_mtime, Folder, MTIME, ROOT};

/// Numbers that are the same at every run: xorshift, from a seed.
struct Numbers(u64);

impl Numbers {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn pick<'a, T>(&mut self, of: &'a [T]) -> Option<&'a T> {
        (!of.is_empty()).then(|| &of[self.below(of.len())])
    }
}

/// The names the edits use: few, so that they meet.
const NAMES: [&str; 8] = ["a.txt", "b.txt", "c.bin", "notes", "box", "x.tmp", "report", "old"];

/// How deep the edits build.
const DEPTH: usize = 4;

fn is_directory(f: &Folder, rel: &Path) -> bool {
    fs::symlink_metadata(f.disk(rel)).is_ok_and(|metadata| metadata.is_dir())
}

fn is_file(f: &Folder, rel: &Path) -> bool {
    fs::symlink_metadata(f.disk(rel)).is_ok_and(|metadata| metadata.is_file())
}

/// A copy of the directory at `from` made at `to` as `cp -a` makes it.
fn copy_directory_with_marks(f: &Folder, from: &Path, to: &Path) {
    f.make_directory(to);
    if let Some(mark) = f.mark_of(from) {
        f.mark_directory(to, &mark);
    }
    for entry in fs::read_dir(f.disk(from)).unwrap() {
        let name = entry.unwrap().file_name();
        match is_directory(f, &from.join(&name)) {
            true => copy_directory_with_marks(f, &from.join(&name), &to.join(&name)),
            false => f.copy_with_marks(from.join(&name), to.join(&name)),
        }
    }
}

/// One edit made by hand, at random. An edit that cannot be made is not made.
fn edit_by_hand(f: &mut Folder, numbers: &mut Numbers) {
    let entries = f.walk_disk();
    let directories: Vec<PathBuf> = std::iter::once(PathBuf::new()).chain(entries.iter().filter(|rel| is_directory(f, rel)).cloned()).filter(|rel| rel.components().count() < DEPTH).collect();
    let files: Vec<PathBuf> = entries.iter().filter(|rel| is_file(f, rel)).cloned().collect();
    let new_path = directories[numbers.below(directories.len())].join(NAMES[numbers.below(NAMES.len())]);
    let is_free = fs::symlink_metadata(f.disk(&new_path)).is_err();
    match numbers.below(11) {
        // Make, or write over what is there.
        0 if is_free || is_file(f, &new_path) => f.write(&new_path, format!("made {}", numbers.below(1000)).as_bytes()),
        1 if is_free => f.make_directory(&new_path),
        // Write: other bytes, or the time alone.
        2 => {
            let Some(file) = numbers.pick(&files) else { return };
            match numbers.below(3) {
                0 => set_mtime(&f.disk(file), MTIME + 1 + numbers.below(50) as i64),
                // In place, as a program that opens and writes: the marks stay.
                _ => {
                    let path = f.disk(file);
                    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
                    fs::write(&path, format!("written {}", numbers.below(100_000))).unwrap();
                    set_mtime(&path, MTIME);
                }
            }
        }
        // Move: to a free name, or a file over a file.
        3 | 4 => {
            let Some(from) = numbers.pick(&entries) else { return };
            let is_into_itself = new_path.starts_with(from);
            let fits = is_free || (is_file(f, from) && is_file(f, &new_path) && *from != new_path);
            if !is_into_itself && fits {
                f.rename(from, &new_path);
            }
        }
        // Remove.
        5 => {
            let Some(gone) = numbers.pick(&entries) else { return };
            match is_directory(f, gone) {
                true => fs::remove_dir_all(f.disk(gone)).unwrap(),
                false => fs::remove_file(f.disk(gone)).unwrap(),
            }
        }
        // Copy, with the marks.
        6 if is_free => {
            let Some(from) = numbers.pick(&entries) else { return };
            match is_directory(f, from) {
                true if !new_path.starts_with(from) && from.components().count() < 2 => copy_directory_with_marks(f, from, &new_path),
                true => {}
                false => f.copy_with_marks(from, &new_path),
            }
        }
        // A second name of a file.
        7 if is_free => {
            let Some(from) = numbers.pick(&files) else { return };
            f.link(from, &new_path);
        }
        // An older copy put back with its marks, where a row of that item is or anywhere.
        8 => {
            let bound: Vec<Item> = f.rows().into_iter().filter(|row| row.kind == ItemKind::File && row.onedrive_item.is_some()).collect();
            let Some(row) = numbers.pick(&bound) else { return };
            let own_path = f.store.item_store().find_path(row.local_item_id).unwrap();
            let at = match (own_path, numbers.below(2)) {
                (Some(own_path), 0) if !is_directory(f, &own_path) && is_directory(f, own_path.parent().unwrap()) => own_path,
                _ if is_free => new_path,
                _ => return,
            };
            let id = row.onedrive_item.as_ref().unwrap().item_id.as_str().to_owned();
            f.write(&at, b"an older copy");
            f.mark_copy(&at, &id);
            if numbers.below(2) == 0 {
                xattr::set(f.disk(&at), konedrive_fs::placeholder::XATTR_CTAG, b"c-older").unwrap();
            }
        }
        // Something put at the name of a file that is not downloaded.
        9 => {
            let rows = f.rows();
            let not_downloaded: Vec<PathBuf> = rows.iter().filter(|row| row.kind == ItemKind::File && row.local.is_some() && row.content_presence == konedrive_tree::ContentPresence::NotOnDisk).filter_map(|row| f.store.item_store().find_path(row.local_item_id).unwrap()).collect();
            let Some(at) = numbers.pick(&not_downloaded) else { return };
            if fs::symlink_metadata(f.disk(at)).is_ok() || !is_directory(f, at.parent().unwrap()) {
                return;
            }
            match numbers.below(2) {
                0 => f.write(at, b"put there"),
                _ => f.make_directory(at),
            }
        }
        _ => {}
    }
}

/// What a daemon does to the rows between two stops: some items of the user's own are sent
/// and agreed on, and some files of OneDrive that are not downloaded appear.
fn change_rows_as_a_daemon(f: &mut Folder, numbers: &mut Numbers, next_onedrive_id: &mut usize) {
    let mut new_id = || {
        *next_onedrive_id += 1;
        format!("N{next_onedrive_id}")
    };
    for row in f.rows() {
        let Some(place) = row.local.as_ref().and_then(|local| local.place.as_ref()) else { continue };
        let is_parent_in_onedrive = place.parent_local_item_id == ROOT || f.item(place.parent_local_item_id).onedrive_item.is_some();
        if !is_parent_in_onedrive {
            continue;
        }
        let path = f.store.item_store().find_path(row.local_item_id).unwrap().unwrap();
        // Sent and agreed on: a file with one name, or a directory.
        if row.onedrive_item.is_none() && numbers.below(3) == 0 {
            let id = new_id();
            match row.kind {
                ItemKind::File if fs::metadata(f.disk(&path)).unwrap().nlink() == 1 => {
                    f.agree(row.local_item_id, &id);
                    f.mark_copy(&path, &id);
                }
                ItemKind::File => {}
                ItemKind::Directory => {
                    f.agree(row.local_item_id, &id);
                    f.mark_directory(&path, &id);
                }
            }
        }
        // A file of OneDrive appears in a directory of OneDrive, not downloaded.
        if row.kind == ItemKind::Directory && row.onedrive_item.is_some() && numbers.below(4) == 0 && path.components().count() < DEPTH {
            let id = new_id();
            let rel = path.join(format!("remote-{id}.bin"));
            if f.row(&rel).is_none() && fs::symlink_metadata(f.disk(&rel)).is_err() {
                f.not_downloaded(rel.to_str().unwrap(), 9, &id);
            }
        }
    }
}

/// The rows the folder shows at `path` and below it, as they are now.
fn list_rows_below(f: &mut Folder, path: &Path) -> HashMap<LocalItemId, Item> {
    let rows = f.rows();
    rows.into_iter().filter(|row| f.store.item_store().find_path(row.local_item_id).unwrap().is_some_and(|row_path| row_path.starts_with(path))).map(|row| (row.local_item_id, row)).collect()
}

fn run_rounds(seed: u64, rounds: usize) {
    let mut numbers = Numbers(seed);
    let mut f = folder();
    f.onedrive_directory("docs", "D");
    f.copy("docs/a.txt", b"aaa", "A");
    f.not_downloaded("docs/remote.bin", 9, "R");
    f.own_directory("mine");
    f.own_file("mine/own.txt", b"own");
    f.own_file("top.txt", b"top");
    f.assert_settled();
    let mut next_onedrive_id = 0;

    for round in 0..rounds {
        let said = |what: &str| format!("seed {seed:#x}, round {round}: {what}");
        change_rows_as_a_daemon(&mut f, &mut numbers, &mut next_onedrive_id);
        f.assert_settled();
        for _ in 0..1 + numbers.below(6) {
            edit_by_hand(&mut f, &mut numbers);
        }

        // A check that cannot read one directory.
        let directories: Vec<PathBuf> = f.walk_disk().into_iter().filter(|rel| is_directory(&f, rel)).collect();
        if let (0, false, Some(locked)) = (numbers.below(4), is_root(), numbers.pick(&directories)) {
            let is_row_a_directory = f.row(locked).is_some_and(|row| row.kind == ItemKind::Directory);
            let below = if is_row_a_directory { list_rows_below(&mut f, locked) } else { HashMap::new() };
            let before = f.rows();
            fs::set_permissions(f.disk(locked), fs::Permissions::from_mode(0o000)).unwrap();
            let findings = f.check();
            fs::set_permissions(f.disk(locked), fs::Permissions::from_mode(0o755)).unwrap();
            let after = f.rows();
            assert_no_item_of_onedrive_is_removed_or_moved(&before, &after);
            assert!(findings.has_unread_part() && left(&findings).contains(locked), "{}", said("the directory that was not read is said"));
            for row in &after {
                if let Some(was) = below.get(&row.local_item_id) {
                    assert_eq!(row, was, "{}", said("a row in a directory that was not read changed"));
                }
            }
            assert_eq!(after.iter().filter(|row| below.contains_key(&row.local_item_id)).count(), below.len(), "{}", said("a row in a directory that was not read went"));
            if is_row_a_directory {
                assert!(findings.list_held_items().contains(&f.id(locked)), "{}", said("its row is held"));
            }
        }

        let before = f.rows();
        let findings = f.check();
        let after = f.rows();
        assert_no_item_of_onedrive_is_removed_or_moved(&before, &after);
        assert!(bindings(&after).len() >= bindings(&before).len(), "{}", said("the items OneDrive has are fewer"));
        assert_eq!(findings.unresolved(), [], "{}", said("everything was read: nothing is left"));
        assert_eq!(findings.list_held_items(), [], "{}", said("nothing is held"));
        f.assert_settled();
    }
}

#[test]
fn rows_and_disk_converge_after_edits_by_hand_and_no_item_of_onedrive_is_removed_or_moved() {
    for seed in [0x9e37_79b9_7f4a_7c15, 0xdaa6_6d2c_7ddf_743f, 0x0123_4567_89ab_cdef, 0xfeed_face_cafe_beef, 0x5151_5151_a1a1_a1a1, 0x0bad_c0de_dead_10cc] {
        run_rounds(seed, 30);
    }
}
