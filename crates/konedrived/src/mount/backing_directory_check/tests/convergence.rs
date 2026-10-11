//! Convergence: rows and a directory that agree; then what can happen while no daemon runs,
//! at random; the check; and then the promise of the check, asked each time:
//!
//! - no row is removed whose item OneDrive has, and none is moved
//!   ([`assert_no_item_of_onedrive_is_removed_or_moved`]);
//! - the plan made from the rows the check left, with what OneDrive has in them, has no
//!   step towards OneDrive that is a removal or a move: nobody removed or moved anything
//!   through the mount ([`Folder::assert_plan_sends_no_removal_or_move`]);
//! - a file its row says is a clean copy holds the bytes that were agreed on;
//! - every entry on disk has a row at its path, of its kind, size and time;
//! - nothing is left unresolved or held, no record stands, and a second run changes
//!   nothing, in the rows or on disk.
//!
//! What happens in a round is one of three things, and then edits by hand:
//!
//! - *a death*: one operation of the daemon that changes a name — a move, a move over a
//!   file, a set-aside, a make, a landing — is cut short at one of its points, its record
//!   written and the disk changed as far as that point. With no edit by hand after it, the
//!   change is undone or finished exactly: the rows and the names on disk are as before it,
//!   or have the one row the operation would have written;
//! - *a rebuilt store*: the rows are lost, the check makes them from the disk, and a
//!   listing then says what OneDrive has. Before it the edits are copies with their marks,
//!   second names and writes, which leave ids that several entries carry;
//! - nothing.
//!
//! The edits by hand are what a person does with a file manager and a shell: make, write,
//! move (also over a file), remove, copy with the marks (`cp -a`, of a file and of a
//! directory), a second name of a file (`ln`), a copy of a file of OneDrive put back with
//! its marks — the content agreed on, or an older one of the same size and time, which
//! only its cTag mark tells —, a file or a directory at the name of a file that is not
//! downloaded. One round in four begins with a check that cannot read a directory chosen
//! at random: the rows in it and below it are as they were. Between the rounds the rows are
//! changed as a daemon would: an item of the user's own is sent and agreed on, and a file
//! of OneDrive that is not downloaded appears.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use konedrive_tree::{ContentPresence, Item, ItemKind, LocalItemId, MadeEntryOwner, PlaceChangeRecord};

use super::unread_parts::is_root;
use super::{assert_no_item_of_onedrive_is_removed_or_moved, bindings, content, folder, left, set_mtime, Folder, MTIME};
use crate::folder::disk::make_aside_entry_name;

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

/// What the test knows beside the folder: the bytes each file of OneDrive was agreed on
/// with, and a time no file was written at yet.
struct World {
    f: Folder,
    numbers: Numbers,
    agreed_bytes: HashMap<String, Vec<u8>>,
    next_onedrive_id: usize,
    next_mtime: i64,
}

fn is_directory(f: &Folder, rel: &Path) -> bool {
    fs::symlink_metadata(f.disk(rel)).is_ok_and(|metadata| metadata.is_dir())
}

fn is_file(f: &Folder, rel: &Path) -> bool {
    fs::symlink_metadata(f.disk(rel)).is_ok_and(|metadata| metadata.is_file())
}

fn is_there(f: &Folder, rel: &Path) -> bool {
    fs::symlink_metadata(f.disk(rel)).is_ok()
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

/// Which edits by hand a round may make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edits {
    Any,
    /// Before a rebuilt store: what leaves an item where OneDrive has it — copies with the
    /// marks, second names, writes, new files. A move by hand of the one entry that carries
    /// an id is the plan's to answer once the rows are made from the marks, not the check's.
    KeepingPlaces,
}

impl World {
    fn new(seed: u64) -> Self {
        let mut f = folder();
        f.onedrive_directory("docs", "D");
        f.copy("docs/a.txt", b"aaa", "A");
        f.not_downloaded("docs/remote.bin", 9, "R");
        f.own_directory("mine");
        f.own_file("mine/own.txt", b"own!");
        f.own_file("top.txt", b"top-5");
        let agreed_bytes = HashMap::from([("A".to_owned(), b"aaa".to_vec()), ("R".to_owned(), b"remote-9!".to_vec())]);
        World { f, numbers: Numbers(seed), agreed_bytes, next_onedrive_id: 0, next_mtime: MTIME + 1000 }
    }

    /// A time no file was written at yet: an edit is told by it, as a real one is.
    fn new_mtime(&mut self) -> i64 {
        self.next_mtime += 1;
        self.next_mtime
    }

    /// A file with these bytes, written now.
    fn write(&mut self, rel: &Path, bytes: &[u8]) {
        self.f.write(rel, bytes);
        let mtime = self.new_mtime();
        set_mtime(&self.f.disk(rel), mtime);
    }

    /// A place a new entry can be made at: a name of the few, in a directory on disk.
    fn pick_new_path(&mut self) -> PathBuf {
        let directories: Vec<PathBuf> = std::iter::once(PathBuf::new()).chain(self.f.walk_disk().into_iter().filter(|rel| is_directory(&self.f, rel))).filter(|rel| rel.components().count() < DEPTH).collect();
        directories[self.numbers.below(directories.len())].join(NAMES[self.numbers.below(NAMES.len())])
    }

    /// One edit made by hand, at random. An edit that cannot be made is not made.
    fn edit_by_hand(&mut self, edits: Edits) {
        let entries = self.f.walk_disk();
        let files: Vec<PathBuf> = entries.iter().filter(|rel| is_file(&self.f, rel)).cloned().collect();
        let new_path = self.pick_new_path();
        let is_free = !is_there(&self.f, &new_path);
        let edit = match edits {
            Edits::Any => self.numbers.below(11),
            Edits::KeepingPlaces => [0, 2, 6, 7][self.numbers.below(4)],
        };
        match edit {
            // Make, or write over what is there.
            0 if is_free || (edits == Edits::Any && is_file(&self.f, &new_path)) => {
                let bytes = format!("made {}", self.numbers.below(1000));
                self.write(&new_path, bytes.as_bytes());
            }
            1 if is_free => self.f.make_directory(&new_path),
            // Write in place, as a program that opens and writes: the marks stay.
            2 => {
                let Some(file) = self.numbers.pick(&files).cloned() else { return };
                let path = self.f.disk(&file);
                if self.numbers.below(3) > 0 {
                    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
                    fs::write(&path, format!("written {}", self.numbers.below(100_000))).unwrap();
                }
                let mtime = self.new_mtime();
                set_mtime(&path, mtime);
            }
            // Move: to a free name, or a file over a file.
            3 | 4 => {
                let Some(from) = self.numbers.pick(&entries).cloned() else { return };
                let fits = is_free || (is_file(&self.f, &from) && is_file(&self.f, &new_path) && from != new_path);
                if !new_path.starts_with(&from) && fits {
                    self.f.rename(&from, &new_path);
                }
            }
            // Remove.
            5 => {
                let Some(gone) = self.numbers.pick(&entries) else { return };
                match is_directory(&self.f, gone) {
                    true => fs::remove_dir_all(self.f.disk(gone)).unwrap(),
                    false => fs::remove_file(self.f.disk(gone)).unwrap(),
                }
            }
            // Copy, with the marks.
            6 if is_free => {
                let Some(from) = self.numbers.pick(&entries) else { return };
                match is_directory(&self.f, from) {
                    true if !new_path.starts_with(from) && from.components().count() < 2 => copy_directory_with_marks(&self.f, from, &new_path),
                    true => {}
                    false => self.f.copy_with_marks(from, &new_path),
                }
            }
            // A second name of a file.
            7 if is_free => {
                let Some(from) = self.numbers.pick(&files) else { return };
                self.f.link(from, &new_path);
            }
            // A copy of a file of OneDrive put back with its marks, where its row is or
            // anywhere: the content agreed on, or an older one of the same size and time.
            8 | 10 => {
                let bound: Vec<Item> = self.f.rows().into_iter().filter(|row| row.kind == ItemKind::File && row.onedrive_item.is_some()).collect();
                let Some(row) = self.numbers.pick(&bound).cloned() else { return };
                let id = row.onedrive_item.as_ref().unwrap().item_id.as_str().to_owned();
                let Some(agreed) = self.agreed_bytes.get(&id).cloned() else { return };
                let own_path = self.f.store.item_store().find_path(row.local_item_id).unwrap();
                let at = match (own_path, self.numbers.below(4)) {
                    (Some(own_path), 0..=2) if !is_directory(&self.f, &own_path) && is_directory(&self.f, own_path.parent().unwrap()) => own_path,
                    _ if is_free => new_path,
                    _ => return,
                };
                let is_older = self.numbers.below(2) == 0;
                let size = content(&row).map_or(agreed.len(), |content| content.size() as usize);
                let bytes = if is_older { vec![b'o'; size] } else { agreed };
                self.f.write(&at, &bytes);
                self.f.mark_copy(&at, &id);
                if is_older {
                    xattr::set(self.f.disk(&at), konedrive_fs::placeholder::XATTR_CTAG, b"c-older").unwrap();
                }
                // The time of the copy it was: a row that knows a content knows this time.
                let mtime = content(&row).map(|content| content.mtime().0);
                let mtime = mtime.unwrap_or_else(|| self.new_mtime());
                set_mtime(&self.f.disk(&at), mtime);
            }
            // Something put at the name of a file that is not downloaded.
            9 => {
                let rows = self.f.rows();
                let not_downloaded: Vec<PathBuf> = rows.iter().filter(|row| row.kind == ItemKind::File && row.local.is_some() && row.content_presence == ContentPresence::NotOnDisk).filter_map(|row| self.f.store.item_store().find_path(row.local_item_id).unwrap()).collect();
                let Some(at) = self.numbers.pick(&not_downloaded).cloned() else { return };
                if is_there(&self.f, &at) || !is_directory(&self.f, at.parent().unwrap()) {
                    return;
                }
                match self.numbers.below(2) {
                    0 => self.write(&at, b"put there"),
                    _ => self.f.make_directory(&at),
                }
            }
            _ => {}
        }
    }

    /// What a daemon does to the rows between two stops: some items of the user's own are
    /// sent and agreed on, and some files of OneDrive that are not downloaded appear.
    fn change_rows_as_a_daemon(&mut self) {
        for row in self.f.rows() {
            let Some(place) = row.local.as_ref().and_then(|local| local.place.as_ref()) else { continue };
            // Where OneDrive has the directory it is in: an item is sent into nothing else.
            let directory = self.f.item(place.parent_local_item_id);
            let is_directory_where_onedrive_has_it = directory.onedrive.is_some() && directory.local.as_ref().map(|local| &local.place) == directory.onedrive.as_ref().map(|onedrive| &onedrive.place);
            if !is_directory_where_onedrive_has_it {
                continue;
            }
            let path = self.f.store.item_store().find_path(row.local_item_id).unwrap().unwrap();
            // Sent and agreed on: a file with one name, or a directory.
            if row.onedrive_item.is_none() && self.numbers.below(3) == 0 {
                self.next_onedrive_id += 1;
                let id = format!("N{}", self.next_onedrive_id);
                match row.kind {
                    ItemKind::File if fs::metadata(self.f.disk(&path)).unwrap().nlink() == 1 => {
                        self.f.agree(row.local_item_id, &id);
                        self.f.mark_copy(&path, &id);
                        self.agreed_bytes.insert(id, fs::read(self.f.disk(&path)).unwrap());
                    }
                    ItemKind::File => {}
                    ItemKind::Directory => {
                        self.f.agree(row.local_item_id, &id);
                        self.f.mark_directory(&path, &id);
                    }
                }
            }
            // A file of OneDrive appears in a directory of OneDrive, not downloaded.
            if row.kind == ItemKind::Directory && row.onedrive.is_some() && self.numbers.below(4) == 0 && path.components().count() < DEPTH {
                self.next_onedrive_id += 1;
                let id = format!("N{}", self.next_onedrive_id);
                let rel = path.join(format!("remote-{id}.bin"));
                if self.f.row(&rel).is_none() && !is_there(&self.f, &rel) {
                    self.f.not_downloaded(rel.to_str().unwrap(), 9, &id);
                    self.agreed_bytes.insert(id, b"remote-9!".to_vec());
                }
            }
        }
    }

    /// One operation of the daemon that changes a name, cut short by a death: its record
    /// written, and the disk changed as far as the point it died at. What the check must
    /// leave when no edit by hand follows.
    fn die_in_an_operation(&mut self) -> AfterDeath {
        let shown: Vec<(PathBuf, Item)> = self.f.walk_disk().into_iter().filter_map(|rel| Some((rel.clone(), self.f.row(&rel)?))).collect();
        let shown_files: Vec<(PathBuf, Item)> = shown.iter().filter(|(_, row)| row.kind == ItemKind::File).cloned().collect();
        let new_path = self.pick_new_path();
        let is_free = !is_there(&self.f, &new_path) && self.f.row(&new_path).is_none();
        let point = self.numbers.below(3);
        match self.numbers.below(5) {
            // A move to a place nothing is at.
            0 => {
                let Some((from, row)) = self.numbers.pick(&shown).cloned() else { return AfterDeath::AsBefore };
                if !is_free || new_path.starts_with(&from) {
                    return AfterDeath::AsBefore;
                }
                let record = PlaceChangeRecord::MoveEntry { item: row.local_item_id, from: self.f.place(&from), to: self.f.place(&new_path) };
                self.f.store.item_store().record_place_change(&record).unwrap();
                if point > 0 {
                    self.f.rename(&from, &new_path);
                }
                AfterDeath::AsBefore
            }
            // A removal: the entry set aside.
            1 => {
                let Some((path, row)) = self.numbers.pick(&shown).cloned() else { return AfterDeath::AsBefore };
                let aside_name = make_aside_entry_name(row.local_item_id);
                let record = PlaceChangeRecord::SetAside { item: row.local_item_id, place: self.f.place(&path), aside_name: aside_name.clone() };
                self.f.store.item_store().record_place_change(&record).unwrap();
                if point > 0 {
                    self.f.rename(&path, &aside_name);
                }
                AfterDeath::AsBefore
            }
            // A move of a file over a file: the one set aside, then the other moved.
            2 => {
                let (Some((from, moved)), Some((to, replaced))) = (self.numbers.pick(&shown_files).cloned(), self.numbers.pick(&shown_files).cloned()) else { return AfterDeath::AsBefore };
                if from == to {
                    return AfterDeath::AsBefore;
                }
                let aside_name = make_aside_entry_name(replaced.local_item_id);
                let record = PlaceChangeRecord::MoveEntryOver { item: moved.local_item_id, from: self.f.place(&from), to: self.f.place(&to), replaced: replaced.local_item_id, aside_name: aside_name.clone() };
                self.f.store.item_store().record_place_change(&record).unwrap();
                if point > 0 {
                    self.f.rename(&to, &aside_name);
                }
                if point > 1 {
                    self.f.rename(&from, &to);
                }
                AfterDeath::AsBefore
            }
            // A make of a file or a directory.
            3 => {
                if !is_free {
                    return AfterDeath::AsBefore;
                }
                let kind = if self.numbers.below(2) == 0 { ItemKind::File } else { ItemKind::Directory };
                let record = PlaceChangeRecord::MakeEntry { kind, place: self.f.place(&new_path), owner: MadeEntryOwner::NewItem { replaced: None } };
                self.f.store.item_store().record_place_change(&record).unwrap();
                if point == 0 {
                    return AfterDeath::AsBefore;
                }
                match kind {
                    ItemKind::File => self.write(&new_path, b""),
                    ItemKind::Directory => self.f.make_directory(&new_path),
                }
                AfterDeath::Made(new_path)
            }
            // The landing of a download.
            _ => {
                let not_downloaded: Vec<Item> = self.f.rows().into_iter().filter(|row| row.kind == ItemKind::File && row.local.is_some() && row.content_presence == ContentPresence::NotOnDisk && row.unsent_since.is_none()).collect();
                let Some(row) = self.numbers.pick(&not_downloaded).cloned() else { return AfterDeath::AsBefore };
                let path = self.f.store.item_store().find_path(row.local_item_id).unwrap().unwrap();
                let (Some(downloaded), Some(reference)) = (content(&row), row.onedrive_item.clone()) else { return AfterDeath::AsBefore };
                let Some(bytes) = self.agreed_bytes.get(reference.item_id.as_str()).cloned() else { return AfterDeath::AsBefore };
                if is_there(&self.f, &path) || !is_directory(&self.f, path.parent().unwrap()) || bytes.len() as u64 != downloaded.size() {
                    return AfterDeath::AsBefore;
                }
                let record = PlaceChangeRecord::LinkFile { item: row.local_item_id, place: self.f.place(&path), content: downloaded.clone() };
                self.f.store.item_store().record_place_change(&record).unwrap();
                if point == 0 {
                    return AfterDeath::AsBefore;
                }
                self.f.write(&path, &bytes);
                self.f.mark_copy(&path, reference.item_id.as_str());
                set_mtime(&self.f.disk(&path), downloaded.mtime().0);
                AfterDeath::Landed(row.local_item_id)
            }
        }
    }

    /// A file its row says is a clean copy holds the bytes that were agreed on. A file with
    /// several names is told by size and time alone, as the design has it.
    fn assert_clean_copies_hold_what_was_agreed_on(&mut self, said: &str) {
        for row in self.f.rows() {
            let is_said_clean = row.kind == ItemKind::File && row.content_presence == ContentPresence::OnDisk && row.synced_content.is_some() && content(&row) == row.synced_content;
            let (true, Some(reference), Some(path)) = (is_said_clean, &row.onedrive_item, self.f.store.item_store().find_path(row.local_item_id).unwrap()) else { continue };
            let (Some(agreed), true) = (self.agreed_bytes.get(reference.item_id.as_str()), fs::metadata(self.f.disk(&path)).unwrap().nlink() == 1) else { continue };
            assert_eq!(&fs::read(self.f.disk(&path)).unwrap(), agreed, "{said}: {path:?} is said to be a clean copy and holds other bytes");
        }
    }
}

/// What a death in an operation must leave once the check ran, when nothing was edited by
/// hand after it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AfterDeath {
    /// The rows and the names on disk as they were before the operation: it was undone, or
    /// never reached the disk.
    AsBefore,
    /// The one row of the entry that was made, and the rest as before.
    Made(PathBuf),
    /// The content of that file on disk, clean, and the rest as before.
    Landed(LocalItemId),
}

/// The rows the folder shows at `path` and below it, as they are now.
fn list_rows_below(f: &mut Folder, path: &Path) -> HashMap<LocalItemId, Item> {
    let rows = f.rows();
    rows.into_iter().filter(|row| f.store.item_store().find_path(row.local_item_id).unwrap().is_some_and(|row_path| row_path.starts_with(path))).map(|row| (row.local_item_id, row)).collect()
}

fn run_rounds(seed: u64, rounds: usize) {
    let mut world = World::new(seed);
    world.f.assert_settled();

    for round in 0..rounds {
        world.change_rows_as_a_daemon();
        world.f.assert_settled();
        let kind = world.numbers.below(6);
        let said = |what: &str| format!("seed {seed:#x}, round {round} (kind {kind}): {what}");
        let rows_at_start = world.f.rows();
        let disk_at_start = world.f.walk_disk();

        // A death in an operation of the daemon; a store that is rebuilt; or neither.
        let after_death = (kind == 0).then(|| world.die_in_an_operation());
        let is_store_rebuilt = kind == 1;
        let edits = if is_store_rebuilt { Edits::KeepingPlaces } else { Edits::Any };
        let edit_count = if after_death.is_some() && world.numbers.below(2) == 0 { 0 } else { 1 + world.numbers.below(6) };
        for _ in 0..edit_count {
            world.edit_by_hand(edits);
        }

        // A check that cannot read one directory.
        let directories: Vec<PathBuf> = world.f.walk_disk().into_iter().filter(|rel| is_directory(&world.f, rel)).collect();
        if let (0, false, false, Some(locked)) = (world.numbers.below(4), is_root(), is_store_rebuilt, world.numbers.pick(&directories).cloned()) {
            let f = &mut world.f;
            let is_row_a_directory = f.row(&locked).is_some_and(|row| row.kind == ItemKind::Directory);
            let below = if is_row_a_directory { list_rows_below(f, &locked) } else { HashMap::new() };
            let before = f.rows();
            fs::set_permissions(f.disk(&locked), fs::Permissions::from_mode(0o000)).unwrap();
            let findings = f.check();
            fs::set_permissions(f.disk(&locked), fs::Permissions::from_mode(0o755)).unwrap();
            let after = f.rows();
            assert_no_item_of_onedrive_is_removed_or_moved(&before, &after);
            assert!(findings.has_unread_part() && left(&findings).contains(&locked), "{}", said("the directory that was not read is said"));
            for row in &after {
                if let Some(was) = below.get(&row.local_item_id) {
                    assert_eq!(row, was, "{}", said("a row in a directory that was not read changed"));
                }
            }
            assert_eq!(after.iter().filter(|row| below.contains_key(&row.local_item_id)).count(), below.len(), "{}", said("a row in a directory that was not read went"));
            if is_row_a_directory {
                assert!(findings.list_held_items().contains(&f.id(&locked)), "{}", said("its row is held"));
            }
            f.assert_plan_sends_no_removal_or_move(&said("after a check that could not read a directory"));
        }

        let onedrive = is_store_rebuilt.then(|| world.f.rebuild_store());
        let before = world.f.rows();
        let findings = world.f.check();
        let after = world.f.rows();
        match onedrive {
            // What OneDrive has comes with the listing that follows a rebuilt store.
            Some(onedrive) => world.f.write_listing(&onedrive),
            None => {
                assert_no_item_of_onedrive_is_removed_or_moved(&before, &after);
                assert!(bindings(&after).len() >= bindings(&before).len(), "{}", said("the items OneDrive has are fewer"));
            }
        }
        assert_eq!(findings.unresolved(), [], "{}", said("everything was read: nothing is left"));
        assert_eq!(findings.list_held_items(), [], "{}", said("nothing is held"));
        assert_eq!(world.f.store.item_store().list_place_change_records().unwrap(), [], "{}", said("no record stands"));
        world.f.assert_plan_sends_no_removal_or_move(&said("after the check"));
        world.assert_clean_copies_hold_what_was_agreed_on(&said("after the check"));

        // A death with nothing done by hand after it: exactly that change is undone or finished.
        if let (Some(after_death), 0) = (&after_death, edit_count) {
            let rows = world.f.rows();
            match after_death {
                AfterDeath::AsBefore => {
                    assert_eq!(rows, rows_at_start, "{}", said("a change that was cut short is undone: the rows are as before it"));
                    assert_eq!(world.f.walk_disk(), disk_at_start, "{}", said("a change that was cut short is undone: the names on disk are as before it"));
                }
                AfterDeath::Made(path) => {
                    let made = world.f.id(path);
                    let others: Vec<Item> = rows.into_iter().filter(|row| row.local_item_id != made).collect();
                    assert_eq!(others, rows_at_start, "{}", said("a make that was cut short is finished: its row, and the others as before"));
                }
                AfterDeath::Landed(item) => {
                    let expected: Vec<Item> = rows_at_start.iter().cloned().map(|row| if row.local_item_id == *item { Item { content_presence: ContentPresence::OnDisk, ..row } } else { row }).collect();
                    assert_eq!(rows, expected, "{}", said("a landing that was cut short is finished: on disk, and nothing else changed"));
                }
            }
        }
        world.f.assert_settled();
    }
}

#[test]
fn rows_and_disk_converge_after_deaths_rebuilt_stores_and_edits_by_hand_and_nothing_is_removed_or_moved_in_onedrive() {
    for seed in [0x9e37_79b9_7f4a_7c15, 0xdaa6_6d2c_7ddf_743f, 0x0123_4567_89ab_cdef, 0xfeed_face_cafe_beef, 0x5151_5151_a1a1_a1a1, 0x0bad_c0de_dead_10cc] {
        run_rounds(seed, 40);
    }
}
