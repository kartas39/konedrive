//! Changes made at random, from a seed: of the folder, each through the store's own
//! operation for a write of the mount, and of OneDrive, as a listing then says them. Made
//! by hand, with a random source of a few lines: no crate is needed for it.
//!
//! The names come from a small set, so that the two sides come to one name often: names
//! that differ by case alone, and by the form of a composed letter. What is made on purpose
//! beside that: exchanges of two names, saves over a file, a directory removed with what is
//! in it, moves of directories into one another.

use crate::model::{ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, Kind, LocalItemId, LocalVersionChange};
use crate::{TreeError, TreeStore};

use super::drive::{DriveItem, OneDriveChanges, ROOT_ID};
use super::step_stand_in::is_name_ignored;

/// A random source: xorshift64*, from a seed.
pub(crate) struct RandomSource(u64);

impl RandomSource {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 16
    }

    /// A number below `bound`; 0 for a bound of 0.
    pub(crate) fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }

    pub(crate) fn pick<'a, T>(&mut self, of: &'a [T]) -> Option<&'a T> {
        of.get(self.below(of.len()))
    }

    /// The order of `items`, at random.
    pub(crate) fn shuffle<T>(&mut self, items: &mut [T]) {
        for last in (1..items.len()).rev() {
            items.swap(last, self.below(last + 1));
        }
    }
}

/// The names the two sides give their items: few, so that they meet.
const NAMES: [&str; 12] = ["a", "A", "b", "B", "c.txt", "C.TXT", "d", "e.txt", "\u{e9}.txt", "e\u{301}.txt", "f", "g"];
/// Names the upload rules ignore, given in the folder alone.
const IGNORED_NAMES: [&str; 2] = ["~cache", "~x.tmp"];

/// What makes the changes, and says in words what it made.
pub(crate) struct RandomChanges {
    pub(crate) random: RandomSource,
    number: u64,
    /// Whether names the upload rules ignore are given in the folder.
    pub(crate) gives_ignored_names: bool,
    /// Whether OneDrive comes to have items the folder cannot show.
    pub(crate) makes_items_the_folder_cannot_show: bool,
    /// Every change that was made, in words: what a failure is read by.
    pub(crate) said: Vec<String>,
    /// The items of OneDrive the folder's last changes moved, removed or wrote into: the
    /// other client changes one of them now and then, so that the two sides meet in one
    /// item and not only in one name.
    touched_here: Vec<String>,
}

fn name_here(item: &Item) -> String {
    item.local.as_ref().and_then(|local| local.place.as_ref()).map_or_else(|| "/".to_owned(), |place| format!("{}/{}", place.parent_local_item_id, place.name.to_string_lossy()))
}

impl RandomChanges {
    pub(crate) fn new(seed: u64) -> Self {
        Self { random: RandomSource::new(seed), number: 0, gives_ignored_names: true, makes_items_the_folder_cannot_show: true, said: Vec::new(), touched_here: Vec::new() }
    }

    fn next_number(&mut self) -> u64 {
        self.number += 1;
        self.number
    }

    fn name(&mut self, in_folder: bool) -> String {
        match self.random.below(20) {
            0 => format!("n{}", self.next_number()),
            1 if in_folder && self.gives_ignored_names => IGNORED_NAMES[self.random.below(IGNORED_NAMES.len())].to_owned(),
            _ => NAMES[self.random.below(NAMES.len())].to_owned(),
        }
    }

    /// A content no item has yet, as `(size, mtime)`; or, now and then, the size of one
    /// OneDrive has with another time (another content of that size), or both of one
    /// OneDrive has (the same content).
    fn content(&mut self, rows: &[Item]) -> (u64, i64) {
        let in_onedrive: Vec<(u64, i64)> = rows.iter().filter_map(|row| row.onedrive.as_ref()?.content.as_ref()).map(|content| (content.size(), content.mtime().0)).collect();
        let fresh = (100 + self.next_number(), 5000 + self.number as i64);
        match (self.random.below(10), self.random.pick(&in_onedrive).copied()) {
            (0, Some((size, _))) => (size, fresh.1),
            (1 | 2, Some(same)) => same,
            _ => fresh,
        }
    }

    /// One change of the folder, as a write of the mount hands it to the store. A change
    /// the rows refuse (a name that is taken, a directory into itself) is not made.
    pub(crate) fn change_folder(&mut self, store: &mut TreeStore) {
        let rows = store.item_store().list_items().unwrap();
        let shown: Vec<&Item> = rows.iter().filter(|row| row.local.is_some() && row.local_item_id != LocalItemId::ROOT).collect();
        let directories: Vec<LocalItemId> = rows.iter().filter(|row| row.local.is_some() && row.kind == ItemKind::Directory).map(|row| row.local_item_id).collect();
        let files: Vec<&Item> = shown.iter().copied().filter(|row| row.kind == ItemKind::File).collect();
        let directory = self.random.pick(&directories).copied().unwrap_or(LocalItemId::ROOT);
        let name = self.name(true);
        let place = ItemPlace { parent_local_item_id: directory, name: name.clone().into() };
        let is_ignored = is_name_ignored(name.as_ref());
        let (size, mtime) = self.content(&rows);
        let made_content = Some(ContentVersion::ChangedHere { size, mtime, mtime_nsec: 5, change_count: 1 });
        let (kind_of_change, item, file) = (self.random.below(100), self.random.pick(&shown).copied(), self.random.pick(&files).copied());
        let touched = match kind_of_change {
            30..62 => item,
            62..77 | 83..89 => file,
            _ => None,
        };
        self.touched_here.extend(touched.and_then(|touched| touched.onedrive_item.as_ref()).map(|touched| touched.item_id.as_str().to_owned()));
        let (said, changes): (String, Vec<LocalVersionChange>) = match (kind_of_change, item, file) {
            (0..20, _, _) => (format!("makes the file {directory}/{name} of {size} bytes at {mtime}"), vec![LocalVersionChange::Made { kind: ItemKind::File, place, content: made_content, is_name_ignored: is_ignored, replaced: None }]),
            (20..30, _, _) => (format!("makes the directory {directory}/{name}"), vec![LocalVersionChange::Made { kind: ItemKind::Directory, place, content: None, is_name_ignored: is_ignored, replaced: None }]),
            (30..50, Some(item), _) => (format!("moves {} ({}) to {directory}/{name}", item.local_item_id, name_here(item)), vec![LocalVersionChange::Moved { item: item.local_item_id, place, is_name_ignored: is_ignored }]),
            (50..62, Some(item), _) => {
                // A directory with what the folder shows in it, the deepest first.
                let mut removed = vec![item.local_item_id];
                let mut at = 0;
                while at < removed.len() {
                    let directory = removed[at];
                    removed.extend(rows.iter().filter(|row| row.local.as_ref().and_then(|local| local.place.as_ref()).is_some_and(|place| place.parent_local_item_id == directory)).map(|row| row.local_item_id));
                    at += 1;
                }
                (format!("removes {} ({}) with {} in it", item.local_item_id, name_here(item), removed.len() - 1), removed.into_iter().rev().map(|item| LocalVersionChange::Removed { item }).collect())
            }
            (62..77, _, Some(file)) => {
                if file.content_presence == ContentPresence::NotOnDisk {
                    store.item_store().set_content_presence(file.local_item_id, ContentPresence::OnDisk).unwrap();
                }
                let item = file.local_item_id;
                (format!("writes {size} bytes at {mtime} into {item} ({})", name_here(file)), vec![LocalVersionChange::OpenedForWriting { item }, LocalVersionChange::ClosedChanged { item, size, mtime, mtime_nsec: 5 }])
            }
            (77..83, Some(first), _) => {
                let second = self.random.pick(&shown).copied().unwrap_or(first);
                let exchanged = store.item_store().exchange_local_places(first.local_item_id, second.local_item_id);
                self.say_unless_refused(format!("exchanges {} ({}) and {} ({})", first.local_item_id, name_here(first), second.local_item_id, name_here(second)), exchanged);
                return;
            }
            (83..89, _, Some(file)) => {
                // A program saves the file by a rename of a temporary file over it.
                let temporary = ItemPlace { parent_local_item_id: directory, name: format!("save{}", self.next_number()).into() };
                let saved = store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place: temporary, content: made_content, is_name_ignored: false, replaced: None }).unwrap();
                (format!("saves {size} bytes at {mtime} over {} ({}) as {saved}", file.local_item_id, name_here(file)), vec![LocalVersionChange::SavedOver { replacing: saved, replaced: file.local_item_id }])
            }
            (89..92, _, Some(file)) => {
                store.item_store().set_content_presence(file.local_item_id, ContentPresence::OnDisk).unwrap();
                self.said.push(format!("here: the content of {} is on disk", file.local_item_id));
                return;
            }
            (92..94, Some(item), _) => {
                // "Always keep on this device", on a file or on a directory with all in it.
                if let Some(pinned) = &item.onedrive_item {
                    store.set_pin(pinned.item_id.as_str(), true).unwrap();
                    self.said.push(format!("here: pins {} ({})", item.local_item_id, name_here(item)));
                }
                return;
            }
            (_, Some(item), _) => {
                // Another case of its name, or its name with another letter case.
                let own = item.local.as_ref().and_then(|local| local.place.clone()).expect("shown");
                let name = own.name.to_string_lossy();
                let other_case = if name.to_uppercase() != *name { name.to_uppercase() } else { name.to_lowercase() };
                (format!("renames {} ({}) to {other_case}", item.local_item_id, name_here(item)), vec![LocalVersionChange::Moved { item: item.local_item_id, place: ItemPlace { name: other_case.clone().into(), ..own }, is_name_ignored: is_name_ignored(other_case.as_ref()) }])
            }
            _ => return,
        };
        let applied = store.item_store().apply_all(&changes).map(|_| ());
        self.say_unless_refused(said, applied);
    }

    /// A program saves the file `file` by a rename of a temporary file over it; nothing
    /// where the folder does not show it.
    pub(crate) fn save_over(&mut self, store: &mut TreeStore, file: LocalItemId) {
        let Some(directory) = store.item_store().find_item(file).unwrap().and_then(|row| row.local.and_then(|local| local.place)).map(|place| place.parent_local_item_id) else { return };
        let number = self.next_number();
        let content = Some(ContentVersion::ChangedHere { size: 100 + number, mtime: 5000 + number as i64, mtime_nsec: 5, change_count: 1 });
        let temporary = ItemPlace { parent_local_item_id: directory, name: format!("save{number}").into() };
        let saved = store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place: temporary, content, is_name_ignored: false, replaced: None }).unwrap();
        let applied = store.item_store().apply(&LocalVersionChange::SavedOver { replacing: saved, replaced: file }).map(|_| ());
        self.say_unless_refused(format!("saves over {file} as {saved}, on purpose"), applied);
    }

    /// The user removes the directory `directory` with all the folder shows in it.
    pub(crate) fn remove_with_all_in_it(&mut self, store: &mut TreeStore, directory: LocalItemId) {
        let rows = store.item_store().list_items().unwrap();
        if !rows.iter().any(|row| row.local_item_id == directory && row.local.is_some()) {
            return;
        }
        let mut removed = vec![directory];
        let mut at = 0;
        while at < removed.len() {
            let above = removed[at];
            removed.extend(rows.iter().filter(|row| row.local.as_ref().and_then(|local| local.place.as_ref()).is_some_and(|place| place.parent_local_item_id == above)).map(|row| row.local_item_id));
            at += 1;
        }
        let changes: Vec<LocalVersionChange> = removed.into_iter().rev().map(|item| LocalVersionChange::Removed { item }).collect();
        let applied = store.item_store().apply_all(&changes).map(|_| ());
        self.say_unless_refused(format!("removes {directory} with all in it, on purpose"), applied);
    }

    /// The user moves `item` to the root, where the folder shows it in `directory`, and
    /// removes `directory` with all that is left in it.
    pub(crate) fn move_to_root_and_remove(&mut self, store: &mut TreeStore, item: LocalItemId, directory: LocalItemId) {
        let number = self.next_number();
        let is_in_it = store.item_store().find_item(item).unwrap().and_then(|row| row.local.and_then(|local| local.place)).is_some_and(|place| place.parent_local_item_id == directory);
        if is_in_it {
            let moved = store.item_store().apply(&LocalVersionChange::Moved { item, place: ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: format!("back{number}").into() }, is_name_ignored: false }).map(|_| ());
            self.say_unless_refused(format!("moves {item} out of {directory} again, on purpose"), moved);
        }
        self.remove_with_all_in_it(store, directory);
    }

    /// One of the cases a rule of the plan is there for, made on purpose on both sides,
    /// every other time; nothing where the rows have no item for it.
    ///
    /// - A directory comes to be one the folder cannot show while the user makes a file in
    ///   it; and one that is so already, and still in the folder for the user's own, is
    ///   removed here with all in it, or renamed here.
    /// - The user makes a directory with a file in it at the name of a directory another
    ///   client makes, which the folder cannot show.
    /// - Another client removes a directory in which the user makes a file, and out of
    ///   which the user moves one.
    pub(crate) fn make_case_on_purpose(&mut self, store: &mut TreeStore, drive: &mut OneDriveChanges) {
        let (case, number) = (self.random.below(8), self.next_number());
        let rows = store.item_store().list_items().unwrap();
        let in_onedrive = |row: &Item| row.onedrive_item.as_ref().map(|item| item.item_id.as_str().to_owned()).filter(|id| drive.items.contains_key(id));
        let shown_directories: Vec<(LocalItemId, String)> = rows.iter().filter(|row| row.kind == ItemKind::Directory && row.local.is_some() && row.local_item_id != LocalItemId::ROOT).filter_map(|row| Some((row.local_item_id, in_onedrive(row)?))).collect();
        let make_file = |store: &mut TreeStore, directory: LocalItemId, name: String| {
            let content = Some(ContentVersion::ChangedHere { size: 100 + number, mtime: 5000 + number as i64, mtime_nsec: 5, change_count: 1 });
            store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place: ItemPlace { parent_local_item_id: directory, name: name.into() }, content, is_name_ignored: false, replaced: None }).map(|_| ())
        };
        match (case, self.random.pick(&shown_directories).cloned()) {
            (0, Some((directory, id))) if self.makes_items_the_folder_cannot_show => {
                let cannot_be_shown = store.item_store().find_folder_visibility(directory).unwrap() == crate::FolderVisibility::CannotBeShown;
                if !cannot_be_shown {
                    let made = make_file(store, directory, format!("own{number}"));
                    self.say_unless_refused(format!("makes the file {directory}/own{number}, on purpose"), made);
                    let own = drive.items[&id].clone();
                    drive.put(&id, DriveItem { is_placed: false, ..own });
                    self.said.push(format!("OneDrive: {id} is an item the folder cannot show, on purpose"));
                } else if self.random.below(2) == 0 {
                    self.remove_with_all_in_it(store, directory);
                } else {
                    let moved = store.item_store().apply(&LocalVersionChange::Moved { item: directory, place: ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: format!("renamed{number}").into() }, is_name_ignored: false }).map(|_| ());
                    self.say_unless_refused(format!("renames {directory}, which the folder cannot show, on purpose"), moved);
                }
            }
            (1, _) if self.makes_items_the_folder_cannot_show => {
                let name = format!("s{number}");
                if drive.put(&format!("X{number}"), DriveItem { is_placed: false, ..DriveItem::new(ROOT_ID, &name, Kind::Folder, None) }) {
                    self.said.push(format!("OneDrive: makes the directory X{number} at {name}, which the folder cannot show, on purpose"));
                    let made = store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::Directory, place: ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: name.clone().into() }, content: None, is_name_ignored: false, replaced: None });
                    if let Ok(directory) = made {
                        let inside = make_file(store, directory, format!("own{number}"));
                        self.say_unless_refused(format!("makes the directory {name} with a file in it, on purpose"), inside);
                    }
                }
            }
            (2, Some((directory, id))) => {
                let moved_out = rows.iter().find(|row| row.kind == ItemKind::File && row.onedrive.is_some() && row.local.as_ref().and_then(|local| local.place.as_ref()).is_some_and(|place| place.parent_local_item_id == directory));
                let made = make_file(store, directory, format!("own{number}"));
                self.say_unless_refused(format!("makes the file {directory}/own{number}, on purpose"), made);
                if let Some(moved_out) = moved_out {
                    let moved = store.item_store().apply(&LocalVersionChange::Moved { item: moved_out.local_item_id, place: ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: format!("out{number}").into() }, is_name_ignored: false }).map(|_| ());
                    self.say_unless_refused(format!("moves {} out of {directory}, on purpose", moved_out.local_item_id), moved);
                }
                self.said.push(format!("OneDrive: removes {id} with all in it, on purpose"));
                drive.remove(&id);
            }
            _ => {}
        }
    }

    fn say_unless_refused(&mut self, said: String, applied: Result<(), TreeError>) {
        match applied {
            Ok(()) => self.said.push(format!("here: {said}")),
            Err(TreeError::Refused(_)) => {}
            Err(other) => panic!("{said}: {other}"),
        }
    }

    /// One change of OneDrive, made in `drive`, which says it in its next listing.
    pub(crate) fn change_onedrive(&mut self, drive: &mut OneDriveChanges) {
        let ids: Vec<String> = drive.items.keys().cloned().collect();
        let directories: Vec<String> = std::iter::once(ROOT_ID.to_owned()).chain(drive.items.iter().filter(|(_, item)| item.kind == Kind::Folder).map(|(id, _)| id.clone())).collect();
        let files: Vec<String> = drive.items.iter().filter(|(_, item)| item.kind == Kind::File).map(|(id, _)| id.clone()).collect();
        let directory = self.random.pick(&directories).cloned().unwrap_or_else(|| ROOT_ID.to_owned());
        let name = self.name(false);
        let (size, mtime) = (300 + self.next_number(), 9000 + self.number as i64);
        // Every other time, an item the folder touched since, where OneDrive still has one.
        let touched: Vec<String> = self.touched_here.iter().filter(|touched| drive.items.contains_key(*touched)).cloned().collect();
        let touched_files: Vec<String> = touched.iter().filter(|touched| files.contains(touched)).cloned().collect();
        let meets = self.random.below(2) == 0;
        let (ids, files) = (if meets && !touched.is_empty() { touched } else { ids }, if meets && !touched_files.is_empty() { touched_files } else { files });
        match (self.random.below(100), self.random.pick(&ids).cloned(), self.random.pick(&files).cloned()) {
            (0..20, _, _) => {
                let id = format!("X{}", self.next_number());
                if drive.put(&id, DriveItem::new(&directory, &name, Kind::File, Some((size, mtime)))) {
                    self.said.push(format!("OneDrive: makes the file {id} at {directory}/{name} of {size} bytes at {mtime}"));
                }
            }
            (20..32, _, _) => {
                let id = format!("X{}", self.next_number());
                if drive.put(&id, DriveItem::new(&directory, &name, Kind::Folder, None)) {
                    self.said.push(format!("OneDrive: makes the directory {id} at {directory}/{name}"));
                }
            }
            (32..55, Some(id), _) => {
                let moved = DriveItem { parent: directory.clone(), name: name.clone(), ..drive.items[&id].clone() };
                if drive.put(&id, moved) {
                    self.said.push(format!("OneDrive: moves {id} to {directory}/{name}"));
                }
            }
            (55..67, Some(id), _) => {
                self.said.push(format!("OneDrive: removes {id} with all in it"));
                drive.remove(&id);
            }
            (67..85, _, Some(id)) => {
                let changed = DriveItem { content: Some((size, mtime)), sent_content: None, ..drive.items[&id].clone() };
                drive.put(&id, changed);
                self.said.push(format!("OneDrive: writes {size} bytes at {mtime} into {id}"));
            }
            (85..87, Some(id), _) if self.makes_items_the_folder_cannot_show => {
                let own = drive.items[&id].clone();
                drive.put(&id, DriveItem { is_placed: !own.is_placed, ..own });
                self.said.push(format!("OneDrive: {id} is an item the folder {} show", if own.is_placed { "cannot" } else { "can" }));
            }
            (85..93, Some(first), _) => {
                let Some(second) = self.random.pick(&ids).cloned().filter(|second| *second != first) else { return };
                if drive.exchange(&first, &second) {
                    self.said.push(format!("OneDrive: exchanges {first} and {second}"));
                }
            }
            (_, Some(id), _) => {
                let own = drive.items[&id].clone();
                let other_case = if own.name.to_uppercase() != own.name { own.name.to_uppercase() } else { own.name.to_lowercase() };
                if drive.put(&id, DriveItem { name: other_case.clone(), ..own }) {
                    self.said.push(format!("OneDrive: renames {id} to {other_case}"));
                }
            }
            _ => {}
        }
    }
}
