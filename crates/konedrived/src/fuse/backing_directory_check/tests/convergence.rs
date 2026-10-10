//! Convergence: rows and a directory that agree; hand edits made at random; the check; then
//! the rows say what the disk has, and a second run of the check finds nothing changed.
//!
//! The rows saying the disk is not enough: they do so after a wrong removal too. So the
//! test keeps a model of what the person could see and did in the round
//! ([`SeenByThePerson`]): which entry of the backing directory is which item's own, followed
//! through every move made by hand, and what was done to each. It is begun anew with the
//! rows after every round: nothing of an earlier round excuses a finding of this one. After
//! each check it asks, of the rows before and after:
//!
//! - an item whose own entry was not touched has the same row, whole;
//! - an item whose own entry is where it was, edited or without its mark, is still the item
//!   at that path, unless a copy that carries its mark was made;
//! - an item that was moved and not edited has the content, the binding and the presence it
//!   had;
//! - an item with nothing of its own on disk is where it was, with the content it had
//!   unless something was put at its name;
//! - an item of OneDrive that the folder does not show any more was removed here only when
//!   its own entry was removed, moved over or moved without its mark in this round; and
//!   lost its place only for a cause the model knows: an entry stands at its name, or the
//!   row of its directory is gone;
//! - where rows of two drives have one item id and an entry that carries it is not where
//!   one of them says, every row of that id is as it was.
//!
//! A round in five begins with a check that cannot read a directory or a file chosen at
//! random, at any depth: nothing leaves the folder then, and no row whose own entry was not
//! touched changes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::{ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, LocalItemId, LocalVersionChange};

use super::{folder, reference, set_mtime, DiskFindings, Folder, UnresolvedReason, MTIME};

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

/// What the person could see in the backing directory, and what they did to it by hand in
/// one round: since the rows last said the disk.
#[derive(Default)]
struct SeenByThePerson {
    /// The entries that are an item's own: each path with the item whose row was at it when
    /// the rows last said the disk, followed through every move made by hand since.
    own_entries: BTreeMap<PathBuf, LocalItemId>,
    /// Where each of those was when the rows last said the disk.
    paths_at_start: BTreeMap<LocalItemId, PathBuf>,
    /// The items whose own entry was removed or moved over by hand in this round. And those
    /// whose entry went so, or was moved without its mark, in an earlier round whose check
    /// left the row of the item as it was: the removal is still to be found.
    removed_by_hand: HashSet<LocalItemId>,
    /// The items of OneDrive whose own entry does not carry their mark: it had its marks
    /// taken off or changed by hand, or was saved by rename. Nothing tells such an entry
    /// from a new file once it is moved.
    unmarked: HashSet<LocalItemId>,
    /// The items whose own entry had its bytes or its time changed.
    edited: HashSet<LocalItemId>,
}

impl SeenByThePerson {
    /// The rows say the disk: every entry is the own entry of the row at its path, but what
    /// the check left. Nothing was done by hand yet.
    fn begin(folder: &mut Folder, left: &[PathBuf]) -> Self {
        let is_left = |path: &Path| left.iter().any(|left| path.starts_with(left));
        let paths: Vec<PathBuf> = folder.walk_disk().into_iter().filter(|path| !is_left(path)).collect();
        let own_entries: BTreeMap<PathBuf, LocalItemId> = paths.into_iter().map(|path| (path.clone(), folder.id(&path))).collect();
        let paths_at_start = own_entries.iter().map(|(path, item)| (*item, path.clone())).collect();
        let mut seen = SeenByThePerson { own_entries, paths_at_start, ..SeenByThePerson::default() };
        seen.note_unmarked(folder);
        seen
    }

    /// [`begin`](Self::begin) after a round: what was done in it excuses nothing later. But
    /// what the check left as it was is not decided yet, and is kept: the own entry that is
    /// in a directory the check left is that item's still, where its row does not say; and
    /// an entry that went by hand is still gone while the row of its item says, as before,
    /// that it is on disk — the check left that row, or a directory above it.
    fn begin_after(self, folder: &mut Folder, left: &[PathBuf]) -> Self {
        let mut seen = Self::begin(folder, left);
        let is_left = |path: &Path| left.iter().any(|left| path.starts_with(left));
        for (path, item) in &self.own_entries {
            let is_followed_still = is_left(path) && fs::symlink_metadata(folder.disk(path)).is_ok() && !seen.paths_at_start.contains_key(item);
            let row_path = folder.store.item_store().find_path(*item).ok().flatten();
            if let (true, Some(row_path)) = (is_followed_still, row_path) {
                seen.own_entries.insert(path.clone(), *item);
                seen.paths_at_start.insert(*item, row_path);
                // Its row was not written: what was done to its bytes before is not in it.
                seen.edited.insert(*item);
            }
        }
        let gone_by_hand: Vec<LocalItemId> = self.paths_at_start.keys().chain(&self.removed_by_hand).copied().filter(|item| self.removed_by_hand.contains(item) || self.is_moved_unmarked(*item)).collect();
        for item in gone_by_hand {
            let row = folder.store.item_store().find_item(item).unwrap();
            if row.is_some_and(|row| is_shown(&row) && (row.kind == ItemKind::Directory || has_copy_on_disk(&row))) {
                seen.removed_by_hand.insert(item);
            }
        }
        seen.note_unmarked(folder);
        seen
    }

    /// The own entries of items of OneDrive that carry no mark of their item.
    fn note_unmarked(&mut self, folder: &mut Folder) {
        for (path, item) in &self.own_entries {
            let binding = folder.item(*item).onedrive_item.map(|reference| reference.item_id.as_str().to_owned());
            if binding.is_some() && folder.mark_of(path) != binding {
                self.unmarked.insert(*item);
            }
        }
    }

    fn list_below(&self, path: &Path) -> Vec<(PathBuf, LocalItemId)> {
        self.own_entries.iter().filter(|(own, _)| own.starts_with(path)).map(|(own, item)| (own.clone(), *item)).collect()
    }

    /// What was at `path`, and all below it, is gone.
    fn remove(&mut self, path: &Path) {
        for (own, item) in self.list_below(path) {
            self.own_entries.remove(&own);
            self.removed_by_hand.insert(item);
        }
    }

    /// What was at `from`, and all below it, is at `to`, where nothing was.
    fn rename(&mut self, from: &Path, to: &Path) {
        for (own, item) in self.list_below(from) {
            self.own_entries.remove(&own);
            // Joining nothing would end the path of the entry itself in a separator.
            self.own_entries.insert(own.strip_prefix(from).unwrap().components().fold(to.to_owned(), |path, name| path.join(name)), item);
        }
    }

    /// The marks of the entry at `path` are not its item's any more. What is in a directory
    /// is found through it: it is not known to be its items' own either.
    fn change_marks(&mut self, path: &Path) {
        for (_, item) in self.list_below(path) {
            self.unmarked.insert(item);
        }
    }

    /// The bytes or the time of the file at `path` are other ones.
    fn edit(&mut self, path: &Path) {
        if let Some(item) = self.own_entries.get(path) {
            self.edited.insert(*item);
        }
    }

    /// Whether the person moved the own entry of `item`, which carries no mark to be found
    /// by: to the check the item is gone from its place, as if removed.
    fn is_moved_unmarked(&self, item: LocalItemId) -> bool {
        self.unmarked.contains(&item) && self.find_where_it_was(item).is_none()
    }

    /// Where the own entry of `item` is, when that is where it was when the rows last said
    /// the disk.
    fn find_where_it_was(&self, item: LocalItemId) -> Option<&Path> {
        let at_start = self.paths_at_start.get(&item)?;
        (self.own_entries.get(at_start) == Some(&item)).then_some(at_start.as_path())
    }

    /// Whether nothing was done to the own entry of `item`, nor to a directory above it: it
    /// is where it was, with its bytes and its marks.
    fn is_untouched(&self, item: LocalItemId) -> bool {
        self.find_where_it_was(item).is_some() && !self.edited.contains(&item) && !self.unmarked.contains(&item)
    }

    /// Whether `item` had an entry of its own when the rows last said the disk.
    fn had_own_entry(&self, item: LocalItemId) -> bool {
        self.paths_at_start.contains_key(&item)
    }
}

/// What a person does by hand in the backing directory, made at random.
struct HandEdits<'a> {
    folder: &'a mut Folder,
    numbers: Numbers,
    /// Raised for each name made, so that a made name is free.
    made: usize,
    log: Vec<String>,
    seen: SeenByThePerson,
    /// What the first check of the round is not to be able to read, by an edit that says so.
    to_lock: Vec<PathBuf>,
}

fn konedrive_marks(path: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let names = xattr::list(path).unwrap().filter(|name| name.as_encoded_bytes().starts_with(b"user.konedrive."));
    names.map(|name| (name.clone(), xattr::get(path, &name).unwrap().unwrap())).collect()
}

/// `cp -a`: the bytes, the time and the marks; a directory with all in it. A symbolic link
/// is left out.
fn copy_with_marks(from: &Path, to: &Path) {
    let metadata = fs::symlink_metadata(from).unwrap();
    if metadata.is_symlink() {
        return;
    }
    if metadata.is_dir() {
        fs::create_dir(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            copy_with_marks(&entry.path(), &to.join(entry.file_name()));
        }
    } else {
        fs::write(to, fs::read(from).unwrap()).unwrap();
        set_mtime(to, metadata.mtime());
    }
    for (name, value) in konedrive_marks(from) {
        xattr::set(to, name, &value).unwrap();
    }
}

/// A symbolic link and a file that is not whole are not picked for an edit of their own:
/// they are left by the check whatever is done. A directory that holds them is edited like
/// any other.
fn is_picked_alone(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    !(name.starts_with("link-") || name.starts_with("partial-"))
}

/// The item ids that rows of two drives have in the folder of the test.
const IDS_IN_TWO_DRIVES: [&str; 3] = ["TA", "TB", "TC"];

impl HandEdits<'_> {
    fn entries(&self, directories: bool) -> Vec<PathBuf> {
        let all = self.folder.walk_disk();
        let is_directory = |rel: &PathBuf| fs::symlink_metadata(self.folder.disk(rel)).unwrap().is_dir();
        all.into_iter().filter(|rel| is_picked_alone(rel) && is_directory(rel) == directories).collect()
    }

    fn directory(&mut self) -> PathBuf {
        let mut directories = self.entries(true);
        directories.push(PathBuf::new());
        self.numbers.pick(&directories).unwrap().clone()
    }

    fn entry(&mut self) -> Option<PathBuf> {
        let directories = self.numbers.below(4) == 0;
        let entries = self.entries(directories);
        self.numbers.pick(&entries).cloned()
    }

    /// The paths of the rows that have nothing on disk, in a directory that is there: a file
    /// that is not downloaded, a directory of OneDrive with none on disk. The person does
    /// not see these names.
    fn unseen_paths(&mut self, kind: Option<ItemKind>) -> Vec<PathBuf> {
        let rows = self.folder.rows();
        let mut unseen = Vec::new();
        for row in rows.iter().filter(|row| row.local.as_ref().is_some_and(|local| local.place.is_some()) && kind.is_none_or(|kind| row.kind == kind)) {
            let path = self.folder.store.item_store().find_path(row.local_item_id).unwrap().unwrap();
            let parent_is_there = self.folder.disk(path.parent().unwrap()).is_dir();
            if parent_is_there && fs::symlink_metadata(self.folder.disk(&path)).is_err() {
                unseen.push(path);
            }
        }
        unseen
    }

    /// A path nothing is at, of a name never used: now and then one the upload rules ignore.
    fn new_path(&mut self, prefix: &str) -> PathBuf {
        self.made += 1;
        let name = match self.numbers.below(10) {
            0 if prefix == "made" => format!("made-{}.tmp", self.made),
            1 if prefix == "made" => "cache".to_owned(),
            _ => format!("{prefix}-{}", self.made),
        };
        let path = self.directory().join(name);
        if fs::symlink_metadata(self.folder.disk(&path)).is_ok() {
            return self.new_path(prefix);
        }
        path
    }

    /// A path nothing is at: mostly a name never used, and now and then the path of a row
    /// that has nothing on disk.
    fn free_path(&mut self) -> PathBuf {
        if self.numbers.below(8) == 0 {
            let unseen = self.unseen_paths(None);
            if let Some(path) = self.numbers.pick(&unseen) {
                return path.clone();
            }
        }
        self.new_path("made")
    }

    fn is_below(path: &Path, directory: &Path) -> bool {
        path.starts_with(directory)
    }

    /// Every copy that was put back with an id of two drives is taken away again: what was
    /// left for it can be decided.
    fn remove_strays(&mut self) {
        let strays: Vec<PathBuf> = self.folder.walk_disk().into_iter().filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("stray-"))).collect();
        for stray in strays.iter().filter(|stray| self.folder.disk(stray).is_file()) {
            fs::remove_file(self.folder.disk(stray)).unwrap();
            self.seen.remove(stray);
        }
        if !strays.is_empty() {
            self.log.push("rm of every stray-*".to_owned());
        }
    }

    fn edit(&mut self) {
        let did = match self.numbers.below(19) {
            0 => {
                let path = self.free_path();
                let size = self.numbers.below(40);
                self.folder.write(&path, &vec![b'x'; size]);
                format!("write {path:?}")
            }
            1 => {
                let path = self.free_path();
                self.folder.make_directory(&path);
                format!("mkdir {path:?}")
            }
            2 | 9 => {
                let files = self.entries(false);
                let Some(path) = self.numbers.pick(&files).cloned() else { return };
                let mut bytes = fs::read(self.folder.disk(&path)).unwrap();
                // Longer, or of the same size with other bytes.
                let keeps_its_size = self.numbers.below(2) == 0 && !bytes.is_empty();
                if keeps_its_size {
                    bytes[0] = bytes[0].wrapping_add(1);
                } else {
                    bytes.extend_from_slice(b"+edit");
                }
                // In place: the marks stay, and the time is a later one.
                fs::write(self.folder.disk(&path), bytes).unwrap();
                set_mtime(&self.folder.disk(&path), MTIME + 100 + self.made as i64);
                self.made += 1;
                self.seen.edit(&path);
                format!("edit {path:?} (size kept: {keeps_its_size})")
            }
            3 | 4 => {
                let Some(from) = self.entry() else { return };
                let to = self.free_path();
                if Self::is_below(&to, &from) {
                    return;
                }
                self.folder.rename(&from, &to);
                self.seen.rename(&from, &to);
                format!("mv {from:?} {to:?}")
            }
            5 => {
                let files = self.entries(false);
                let (Some(from), Some(to)) = (self.numbers.pick(&files).cloned(), self.numbers.pick(&files).cloned()) else { return };
                if from == to {
                    return;
                }
                self.folder.rename(&from, &to);
                self.seen.remove(&to);
                self.seen.rename(&from, &to);
                format!("mv over {from:?} {to:?}")
            }
            6 => {
                let Some(path) = self.entry() else { return };
                let on_disk = self.folder.disk(&path);
                if on_disk.is_dir() { fs::remove_dir_all(on_disk).unwrap() } else { fs::remove_file(on_disk).unwrap() }
                self.seen.remove(&path);
                format!("rm -r {path:?}")
            }
            7 => {
                let Some(from) = self.entry() else { return };
                let to = self.free_path();
                if Self::is_below(&to, &from) {
                    return;
                }
                copy_with_marks(&self.folder.disk(&from), &self.folder.disk(&to));
                format!("cp -a {from:?} {to:?}")
            }
            8 => {
                let (Some(first), Some(second)) = (self.entry(), self.entry()) else { return };
                if Self::is_below(&first, &second) || Self::is_below(&second, &first) {
                    return;
                }
                let held = Path::new("held-for-an-exchange");
                self.folder.rename(&first, held);
                self.folder.rename(&second, &first);
                self.folder.rename(held, &second);
                self.seen.rename(&first, held);
                self.seen.rename(&second, &first);
                self.seen.rename(held, &second);
                format!("exchange {first:?} {second:?}")
            }
            10 => {
                // The marks of an entry taken off, or its id changed to one no row has.
                let Some(path) = self.entry() else { return };
                let on_disk = self.folder.disk(&path);
                if konedrive_marks(&on_disk).is_empty() {
                    return;
                }
                let mode = fs::symlink_metadata(&on_disk).unwrap().permissions();
                fs::set_permissions(&on_disk, fs::Permissions::from_mode(mode.mode() | 0o200)).unwrap();
                let takes_off = self.numbers.below(2) == 0;
                if takes_off {
                    for (name, _) in konedrive_marks(&on_disk) {
                        xattr::remove(&on_disk, name).unwrap();
                    }
                } else {
                    self.made += 1;
                    xattr::set(&on_disk, XATTR_ITEM_ID, format!("CHANGED-{}", self.made).as_bytes()).unwrap();
                }
                fs::set_permissions(&on_disk, mode).unwrap();
                self.seen.change_marks(&path);
                format!("marks of {path:?} {}", if takes_off { "taken off" } else { "changed" })
            }
            11 => match self.numbers.below(3) {
                0 => {
                    let path = self.new_path("link");
                    std::os::unix::fs::symlink("nowhere", self.folder.disk(&path)).unwrap();
                    format!("ln -s nowhere {path:?}")
                }
                1 => {
                    // A file that holds nothing, marked with an id no row has.
                    let path = self.new_path("partial");
                    fs::write(self.folder.disk(&path), b"").unwrap();
                    xattr::set(self.folder.disk(&path), XATTR_ITEM_ID, format!("PARTIAL-{}", self.made).as_bytes()).unwrap();
                    format!("a file that is not whole at {path:?}")
                }
                _ => {
                    // The whole copy of an item no row has: a store that lost it.
                    let path = self.new_path("lost");
                    self.folder.write(&path, b"lost and found");
                    self.folder.mark_copy(&path, &format!("LOST-{}", self.made));
                    format!("a marked copy no row knows at {path:?}")
                }
            },
            14 | 15 => {
                // Saved by an editor that writes a new file and renames it over: other
                // bytes now and then, and no marks.
                let files = self.entries(false);
                let Some(path) = self.numbers.pick(&files).cloned() else { return };
                let mut bytes = fs::read(self.folder.disk(&path)).unwrap();
                let keeps_its_bytes = self.numbers.below(3) == 0;
                if !keeps_its_bytes {
                    bytes.extend_from_slice(b"+saved");
                }
                let mtime = fs::metadata(self.folder.disk(&path)).unwrap().mtime();
                self.folder.write(&path, &bytes);
                if keeps_its_bytes {
                    set_mtime(&self.folder.disk(&path), mtime);
                } else {
                    self.seen.edit(&path);
                }
                self.seen.change_marks(&path);
                format!("save by rename {path:?} (bytes kept: {keeps_its_bytes})")
            }
            16 => {
                let id = *self.numbers.pick(&IDS_IN_TWO_DRIVES).unwrap();
                if self.numbers.below(3) == 0 {
                    // The whole copy of an item whose id rows of two drives have, put back
                    // from somewhere: the mark does not say which of the two it is.
                    let path = self.new_path("stray");
                    self.folder.write(&path, b"which of the two");
                    self.folder.mark_copy(&path, id);
                    format!("a copy marked {id} at {path:?}")
                } else {
                    // The file of one of the two saved by rename, where it is: no marks.
                    let own: Vec<PathBuf> = self.seen.own_entries.keys().filter(|path| self.folder.mark_of(path).as_deref() == Some(id)).cloned().collect();
                    let Some(path) = self.numbers.pick(&own).cloned() else { return };
                    self.folder.write(&path, b"one of the two, saved by rename");
                    self.seen.edit(&path);
                    self.seen.change_marks(&path);
                    format!("save by rename {path:?}, whose id {id} is in two drives")
                }
            }
            17 => {
                // The whole copy of a file that is not downloaded, put back from somewhere:
                // nothing of that file was on disk to be moved.
                let unseen = self.unseen_paths(Some(ItemKind::File));
                let Some(of) = self.numbers.pick(&unseen).cloned() else { return };
                let Some(id) = self.folder.row(&of).and_then(|row| row.onedrive_item).map(|reference| reference.item_id.as_str().to_owned()) else { return };
                let path = self.new_path("back");
                self.folder.write(&path, b"put back from a backup");
                self.folder.mark_copy(&path, &id);
                format!("a copy marked {id}, of {of:?} that is not on disk, at {path:?}")
            }
            18 => {
                // A directory copied with all in it, a file of it copied once more, and the
                // directory itself then one that cannot be read: the copies are all that is
                // seen with the marks of what is in it.
                let directories: Vec<PathBuf> = self.entries(true).into_iter().filter(|directory| !self.to_lock.iter().any(|locked| directory.starts_with(locked))).collect();
                let Some(directory) = self.numbers.pick(&directories).cloned() else { return };
                let files: Vec<PathBuf> = self.entries(false).into_iter().filter(|file| file.parent() == Some(directory.as_path())).collect();
                let (copy, file_copy) = (self.new_path("made"), self.new_path("made"));
                if Self::is_below(&copy, &directory) || Self::is_below(&file_copy, &directory) || Self::is_below(&file_copy, &copy) {
                    return;
                }
                copy_with_marks(&self.folder.disk(&directory), &self.folder.disk(&copy));
                if let Some(file) = self.numbers.pick(&files) {
                    copy_with_marks(&self.folder.disk(file), &self.folder.disk(&file_copy));
                }
                self.to_lock.push(directory.clone());
                format!("cp -a {directory:?} {copy:?}, a file of it to {file_copy:?}, and {directory:?} not to be read")
            }
            _ => {
                // An entry of the other kind at a name the person does not see.
                let of_file = self.numbers.below(2) == 0;
                let unseen = self.unseen_paths(Some(if of_file { ItemKind::File } else { ItemKind::Directory }));
                let Some(path) = self.numbers.pick(&unseen).cloned() else { return };
                if of_file {
                    self.folder.make_directory(&path);
                } else {
                    self.folder.write(&path, b"where a directory of OneDrive is");
                }
                format!("{} at the unseen {path:?}", if of_file { "mkdir" } else { "write" })
            }
        };
        self.log.push(did);
    }
}

/// A folder whose rows and disk agree, with every kind of row in it.
fn agreeing_folder() -> Folder {
    let mut f = folder();
    f.onedrive_directory("docs", "D");
    f.onedrive_directory("docs/deep", "DD");
    f.own_directory("mine");
    f.own_directory("mine/more");
    for n in 0..6 {
        f.copy(&format!("docs/copy-{n}"), format!("copy {n}").as_bytes(), &format!("C{n}"));
        f.copy(&format!("docs/deep/copy-{n}"), format!("deep copy {n}").as_bytes(), &format!("DC{n}"));
        f.not_downloaded(&format!("docs/remote-{n}"), 10 + n, &format!("R{n}"));
        f.own_file(&format!("mine/own-{n}"), format!("own {n}").as_bytes());
        f.own_file(&format!("mine/more/own-{n}"), b"more");
    }
    // Changed here through the mount, and not sent.
    for n in 0..3 {
        let edited = f.copy(&format!("docs/edited-{n}"), b"edited", &format!("E{n}"));
        f.store.item_store().apply(&LocalVersionChange::Truncated { item: edited, size: 6, mtime: MTIME, mtime_nsec: 0 }).unwrap();
    }
    f.copy("moved-in", b"a copy in a directory of the user's own", "MI");
    f.move_through_the_mount("moved-in", "mine/moved-in");
    let remote = f.not_downloaded("remote-in", 5, "RI");
    let place = ItemPlace { parent_local_item_id: f.id("mine"), name: "remote-in".into() };
    f.store.item_store().apply(&LocalVersionChange::Moved { item: remote, place, is_name_ignored: false }).unwrap();
    f.not_downloaded("top-remote", 77, "TR");
    // A directory of OneDrive with none on disk, and what is in it.
    f.onedrive_directory("unseen", "U");
    f.onedrive_directory("unseen/sub", "US");
    f.not_downloaded("unseen/remote", 3, "UR");
    f.not_downloaded("unseen/sub/remote", 3, "USR");
    fs::remove_dir_all(f.disk("unseen")).unwrap();
    // One item id in two drives: a shared folder is another drive's.
    f.directory_of_drive("shared", "d2", "S");
    f.file_of_drive("docs/twin-a", "d1", "TA", true);
    f.file_of_drive("shared/twin-a", "d2", "TA", true);
    f.file_of_drive("docs/twin-b", "d1", "TB", false);
    f.file_of_drive("shared/twin-b", "d2", "TB", true);
    // And one the folder shows in one drive only: the row of the other was listed, and is
    // not shown yet.
    f.file_of_drive("shared/twin-c", "d2", "TC", false);
    f.store.item_store().make_onedrive_item(&reference("d1", "TC"), ItemKind::File).unwrap();
    f
}

fn is_shown(row: &Item) -> bool {
    row.local.is_some()
}

fn is_changed_here(row: &Item) -> bool {
    matches!(row.local.as_ref().and_then(|local| local.content.as_ref()), Some(ContentVersion::ChangedHere { .. }))
}

fn has_copy_on_disk(row: &Item) -> bool {
    row.kind == ItemKind::File && row.content_presence == ContentPresence::OnDisk
}

fn place(row: &Item) -> Option<&ItemPlace> {
    row.local.as_ref().and_then(|local| local.place.as_ref())
}

/// The rows before a check, each with the path it had, the marks the entries carried then,
/// and the rows after it.
#[derive(Clone)]
struct RowsOfACheck {
    before: Vec<Item>,
    paths_before: HashMap<LocalItemId, PathBuf>,
    /// The entries that carried each OneDrive item id before the check.
    marked_before: HashMap<String, Vec<PathBuf>>,
    now: Vec<Item>,
}

impl RowsOfACheck {
    fn before(f: &mut Folder) -> Self {
        let before = f.rows();
        let paths_before = before.iter().filter_map(|row| Some((row.local_item_id, f.store.item_store().find_path(row.local_item_id).unwrap()?))).collect();
        let mut marked_before: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for path in f.walk_disk() {
            // What cannot be read carries what nobody can say.
            if let Ok(Some(id)) = xattr::get(f.disk(&path), XATTR_ITEM_ID) {
                marked_before.entry(String::from_utf8(id).unwrap()).or_default().push(path);
            }
        }
        RowsOfACheck { before, paths_before, marked_before, now: Vec::new() }
    }

    fn after(&self, f: &mut Folder) -> Self {
        RowsOfACheck { now: f.rows(), ..self.clone() }
    }

    /// Whether an entry at another path than `path` carried the id of the binding of `row`
    /// before the check: a copy made by hand, or the item's own that was copied.
    fn was_marked_elsewhere(&self, row: &Item, path: &Path) -> bool {
        let id = row.onedrive_item.as_ref().map(|reference| reference.item_id.as_str()).unwrap_or_default();
        self.marked_before.get(id).is_some_and(|marked| marked.iter().any(|marked| marked != path))
    }

    fn find_now(&self, id: LocalItemId) -> Option<&Item> {
        self.now.iter().find(|row| row.local_item_id == id)
    }
}

/// What holds of every check, also of one that could not read a part of the backing
/// directory: an item whose own entry was not touched has the row it had; an item with
/// nothing of its own on disk is not moved, and is not given content unless something is at
/// its name; and where an entry that carries an item id of two drives is not where a row of
/// that id says, every row of the id is as it was.
fn assert_what_was_not_touched_is_as_it_was(seen: &SeenByThePerson, rows: &RowsOfACheck, f: &mut Folder, was_part_unread: bool) {
    for was in rows.before.iter().filter(|row| is_shown(row) && row.local_item_id != LocalItemId::ROOT) {
        let id = was.local_item_id;
        if seen.is_untouched(id) {
            assert_eq!(rows.find_now(id), Some(was), "the entry of this item was not touched, and its row is another");
        }
        // An item of OneDrive with nothing of its own on disk: a file that is not on disk,
        // a directory with none.
        if was.onedrive_item.is_some() && !has_copy_on_disk(was) && !seen.had_own_entry(id) {
            let Some(is) = rows.find_now(id).filter(|is| is_shown(is)) else { continue };
            // A row the check set aside, to a name of its own, waits there for a next check.
            if place(is).is_some_and(|place| crate::folder::disk::daemon_owned(&place.name)) {
                continue;
            }
            assert_eq!((place(is), &is.onedrive_item), (place(was), &was.onedrive_item), "an item with nothing of its own on disk was moved: {was:?}");
            let is_something_at_its_name = rows.paths_before.get(&id).is_some_and(|path| fs::symlink_metadata(f.disk(path)).is_ok());
            assert!(is_something_at_its_name || (is.local == was.local && is.synced_content == was.synced_content), "an item with nothing of its own on disk has another content, and nothing is at its name: {was:?} is now {is:?}");
        }
    }
    for id in IDS_IN_TWO_DRIVES {
        let of_the_id: Vec<&Item> = rows.before.iter().filter(|row| row.onedrive_item.as_ref().is_some_and(|reference| reference.item_id.as_str() == id)).collect();
        // An entry is where a row of the id says when it is the own entry of that row's
        // item, under its name, in the own entry of the directory the row says: also in a
        // directory that was moved by hand.
        let is_where_a_row_says = |path: &Path| {
            let row = seen.own_entries.get(path).and_then(|item| of_the_id.iter().find(|row| row.local_item_id == *item));
            row.and_then(|row| place(row)).is_some_and(|place| {
                let directory = path.parent().unwrap_or(Path::new(""));
                let is_in_its_directory = if place.parent_local_item_id == LocalItemId::ROOT { directory == Path::new("") } else { seen.own_entries.get(directory) == Some(&place.parent_local_item_id) };
                path.file_name() == Some(place.name.as_os_str()) && is_in_its_directory
            })
        };
        let marked: Vec<PathBuf> = rows.marked_before.get(id).cloned().unwrap_or_default();
        let stray = marked.into_iter().find(|path| !is_where_a_row_says(path));
        let Some(stray) = stray else { continue };
        for was in of_the_id {
            // The entry may be any row of the id that had a copy on disk, or the one the
            // folder does not show: each is as it was. But a row whose own entry is where it
            // says, with its mark, is that entry's whatever else carries the id; and a row
            // that had nothing on disk is no entry, and is asked of above.
            let item = was.local_item_id;
            // Where it was, or where its row says in a directory the check took as the one
            // the row is in: a copy of that directory, when the directory lost its mark.
            let now_path = rows.find_now(item).filter(|is| place(is) == place(was)).and_then(|_| f.store.item_store().find_path(item).ok().flatten());
            let is_marked_where_its_row_says_now = now_path.is_some_and(|path| rows.marked_before.get(id).is_some_and(|marked| marked.contains(&path)));
            let is_in_place_with_its_mark = (seen.find_where_it_was(item).is_some() && !seen.unmarked.contains(&item)) || is_marked_where_its_row_says_now;
            // A check that could not read a part may not have seen the entry: a file that
            // stands at the place of a row and carries no mark is then that row's, as it is
            // of any row.
            let is_taken_by_its_place = was_part_unread && rows.paths_before.get(&item).is_some_and(|path| fs::symlink_metadata(f.disk(path)).is_ok());
            if (has_copy_on_disk(was) && !is_in_place_with_its_mark && !is_taken_by_its_place) || !is_shown(was) {
                assert_eq!(rows.find_now(item), Some(was), "{stray:?} carries the id {id}, which rows of two drives have, and is where none of them says: a row of the id that it may be is left as it was");
            }
        }
    }
}

/// What the rows may say after a check that read the whole backing directory, of the rows
/// before it and of what the person did.
fn assert_nothing_unseen_is_removed(seen: &SeenByThePerson, rows: &RowsOfACheck, f: &mut Folder) {
    let find = |id: LocalItemId| rows.find_now(id);
    let is_another_at = |was: &Item| rows.now.iter().any(|row| row.local_item_id != was.local_item_id && place(row).is_some() && place(row) == place(was));
    for was in rows.before.iter().filter(|row| is_shown(row) && row.local_item_id != LocalItemId::ROOT) {
        let id = was.local_item_id;
        // An item whose own entry is where it was is the item at that path still, edited or
        // without its mark. But when a copy carries its mark and it does not: the copy is
        // taken as the item, moved.
        if let Some(path) = seen.find_where_it_was(id).filter(|path| !(seen.unmarked.contains(&id) && rows.was_marked_elsewhere(was, path))) {
            let is = f.row(path);
            assert_eq!(is.as_ref().map(|row| row.local_item_id), Some(id), "the entry at {path:?} is where it was, and its item is not: {was:?}");
            if was.onedrive_item.is_some() {
                assert_eq!(is.map(|row| row.onedrive_item), Some(was.onedrive_item.clone()), "the entry at {path:?} is where it was, and its item is another item of OneDrive");
            }
        }
        let Some(binding) = &was.onedrive_item else {
            // An item OneDrive never had loses its row only when its entry was removed, or
            // moved: it carries no mark to be found by.
            assert!(find(id).is_some() || seen.find_where_it_was(id).is_none(), "the row of {was:?} is gone, and its entry is where it was");
            continue;
        };
        // No item of OneDrive is forgotten.
        let is = rows.now.iter().find(|row| row.onedrive_item.as_ref() == Some(binding)).unwrap_or_else(|| panic!("the binding of {was:?} is gone"));
        if is_shown(is) {
            continue;
        }
        let was_agreed = was.synced_place.is_some();
        let is_removed_here = is.synced_place.is_some();
        let parent_is_shown = place(was).and_then(|place| find(place.parent_local_item_id)).is_some_and(is_shown);
        if is_removed_here {
            // Removed here, its removal to be sent: only a file whose copy the person had
            // before them and removed or put something over.
            assert!(has_copy_on_disk(was), "an item of OneDrive with nothing of its own on disk is removed here: {was:?}");
            // Or moved, when it carries no mark: nothing says then that the file at the new
            // place is the item.
            assert!(seen.removed_by_hand.contains(&id) || seen.is_moved_unmarked(id), "an item of OneDrive is removed here whose entry nobody removed in this round: {was:?}");
            assert!(is_changed_here(was) || is_another_at(was), "a clean copy that is gone is removed here, with nothing put at its name: {was:?}");
        } else if was_agreed {
            // Its place lost: nothing of it is to be sent, and it was not a file changed
            // here that the person had before them.
            assert!(!(has_copy_on_disk(was) && is_changed_here(was)), "a file changed here lost its place instead of being removed: {was:?}");
            assert_eq!((is.local_item_id, is.unsent_since, is.content_presence), (id, None, ContentPresence::NotOnDisk), "{is:?}");
            // And for a cause: the row of its directory is gone; or, of an item with
            // nothing of its own on disk, another item has its name.
            assert!(!parent_is_shown || (!has_copy_on_disk(was) && is_another_at(was)), "an item of OneDrive lost its place, its directory is shown and nothing took its name: {was:?}");
        } else {
            // Bound by its mark and never agreed on: its row cannot tell a removal from a
            // lost place. A clean copy that is merely gone stays shown, as not on disk.
            assert!(is_another_at(was) || !parent_is_shown || (is_changed_here(was) && (seen.removed_by_hand.contains(&id) || seen.is_moved_unmarked(id))), "a copy bound by its mark is not shown any more, and nothing took its place: {was:?}");
        }
    }
    // An item that was moved by hand and not edited, and that the check followed, has what
    // it had: a clean copy stays clean.
    for (path, id) in &seen.own_entries {
        let is_moved_alone = seen.paths_at_start.get(id) != Some(path) && !seen.edited.contains(id) && !seen.unmarked.contains(id);
        let (Some(was), Some(is)) = (rows.before.iter().find(|row| row.local_item_id == *id), f.row(path).filter(|row| row.local_item_id == *id)) else { continue };
        if is_moved_alone {
            let content = |row: &Item| (row.local.as_ref().and_then(|local| local.content.clone()), row.onedrive_item.clone(), row.content_presence, row.synced_content.clone(), row.synced_place.clone());
            assert_eq!(content(&is), content(was), "the item at {path:?} was moved and nothing else, and its row says more");
        }
    }
}

/// The paths a check may leave whatever the person did: a symbolic link, a file that is
/// not whole and marked with an id no row has, and what depends on which row an entry is
/// whose id rows of two drives have.
fn list_left(findings: &DiskFindings, said: &dyn Fn(&str) -> String) -> Vec<PathBuf> {
    // What waits for the place of a row that is left for its item id waits with it; so
    // does the directory such a row is in, which cannot go while the row stays, and the row
    // of an item that was moved into a directory that waits.
    let is_something_left_for_two_drives = findings.unresolved().iter().any(|left| left.reason == UnresolvedReason::ItemIdInTwoDrives);
    for left in findings.unresolved() {
        let waits_with_what_is_left = left.reason == UnresolvedReason::Blocked && is_something_left_for_two_drives;
        assert!(waits_with_what_is_left || matches!(left.reason, UnresolvedReason::NotFileOrDirectory | UnresolvedReason::PartialCopy | UnresolvedReason::ItemIdInTwoDrives), "{}", said(&format!("{left:?} was left")));
    }
    let mut left: Vec<PathBuf> = findings.unresolved().iter().map(|left| left.path.clone()).collect();
    left.dedup();
    left
}

/// A directory or a file chosen at random, at any depth, that cannot be read until it is
/// given its mode back.
fn lock_at_random(edits: &mut HandEdits<'_>) -> Option<(PathBuf, fs::Permissions)> {
    let of_directories = edits.numbers.below(3) != 0;
    let entries = edits.entries(of_directories);
    let locked = edits.numbers.pick(&entries)?.clone();
    let mode = fs::symlink_metadata(edits.folder.disk(&locked)).unwrap().permissions();
    fs::set_permissions(edits.folder.disk(&locked), fs::Permissions::from_mode(0o000)).unwrap();
    Some((locked, mode))
}

fn converge(seed: u64) {
    // SAFETY: no arguments, no failure.
    let is_root = unsafe { libc::geteuid() == 0 };
    let mut f = agreeing_folder();
    f.assert_settled();
    let seen = SeenByThePerson::begin(&mut f, &[]);
    let mut edits = HandEdits { folder: &mut f, numbers: Numbers(seed), made: 0, log: Vec::new(), seen, to_lock: Vec::new() };
    for round in 0..25 {
        edits.log.clear();
        if edits.numbers.below(2) == 0 {
            edits.remove_strays();
        }
        for _ in 0..12 {
            edits.edit();
        }
        let rows_before = RowsOfACheck::before(edits.folder);
        let before = rows_before.before.clone();
        // Now and then a first check cannot read a directory or a file, or two of them, and
        // a second can read everything: the start after the person put them right.
        let mut locked = Vec::new();
        // What an edit said is not to be read, where it is still: the outermost first.
        let mut to_lock: Vec<PathBuf> = std::mem::take(&mut edits.to_lock).into_iter().filter(|path| edits.folder.disk(path).is_dir()).collect();
        to_lock.sort();
        for path in to_lock {
            if !is_root && !locked.iter().any(|(outer, _): &(PathBuf, fs::Permissions)| path.starts_with(outer)) {
                let mode = fs::symlink_metadata(edits.folder.disk(&path)).unwrap().permissions();
                fs::set_permissions(edits.folder.disk(&path), fs::Permissions::from_mode(0o000)).unwrap();
                locked.push((path, mode));
            }
        }
        if !is_root && locked.is_empty() && edits.numbers.below(5) == 0 {
            locked.extend(lock_at_random(&mut edits));
            // Now and then two: not one that is in the first, which cannot be reached.
            let first_is_a_file = locked.first().is_some_and(|(first, _)| !edits.folder.disk(first).is_dir());
            if first_is_a_file && edits.numbers.below(2) == 0 {
                let second = lock_at_random(&mut edits).filter(|(second, _)| locked.iter().all(|(first, _)| first != second));
                locked.extend(second);
            }
        }
        if !locked.is_empty() {
            let findings = edits.folder.check();
            for (path, mode) in locked.iter().rev() {
                fs::set_permissions(edits.folder.disk(path), mode.clone()).unwrap();
            }
            let said = |what: &str| format!("seed {seed:#x}, round {round}, unreadable {:?}: {what}\nedits: {:#?}\nfindings: {:?}\nleft: {:?}", locked.iter().map(|(path, _)| path).collect::<Vec<_>>(), edits.log, super::found(&findings), findings.unresolved());
            assert!(findings.has_unread_part(), "{}", said("a part could not be read, and the check says it read everything"));
            let rows = rows_before.after(edits.folder);
            // Nothing leaves the folder while part of it was not read: every item it showed
            // it shows still, and has still what OneDrive agreed on.
            for was in before.iter().filter(|row| is_shown(row)) {
                let is = rows.find_now(was.local_item_id).filter(|is| is_shown(is));
                assert!(is.is_some(), "{}", said(&format!("{was:?} left the folder while a part was not read")));
                // (An item OneDrive did not have may be bound, by the mark of its entry.)
                if was.onedrive_item.is_some() {
                    assert_eq!(is.map(|is| (&is.onedrive_item, &is.synced_place)), Some((&was.onedrive_item, &was.synced_place)), "{}", said("an item is another item of OneDrive"));
                }
                // Nor is a copy said to be on disk no longer: it may be in that part.
                assert!(!(has_copy_on_disk(was) && !has_copy_on_disk(is.unwrap())), "{}", said(&format!("{was:?} is said to be on disk no longer while a part was not read")));
            }
            let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| assert_what_was_not_touched_is_as_it_was(&edits.seen, &rows, edits.folder, true)));
            if let Err(panic) = checked {
                panic!("{}\n{}", said("a row whose entry was not touched is another after a check that did not read everything"), crate::panic::message(panic));
            }
        }
        let findings = edits.folder.check();
        let said = |what: &str| format!("seed {seed:#x}, round {round}, unreadable at first {:?}: {what}\nedits: {:#?}\nfindings: {:?}\nleft: {:?}", locked.iter().map(|(path, _)| path).collect::<Vec<_>>(), edits.log, super::found(&findings), findings.unresolved());
        assert!(!findings.has_unread_part(), "{}", said("the backing directory was not read whole"));
        let rows = rows_before.after(edits.folder);
        let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let left = list_left(&findings, &said);
            assert_what_was_not_touched_is_as_it_was(&edits.seen, &rows, edits.folder, !locked.is_empty());
            assert_nothing_unseen_is_removed(&edits.seen, &rows, edits.folder);
            edits.folder.assert_settled_but(&left);
            left
        }));
        match checked {
            Ok(left) => {
                let seen = std::mem::take(&mut edits.seen);
                edits.seen = seen.begin_after(edits.folder, &left);
            }
            Err(panic) => panic!("{}\n{}", said("the rows do not say the disk, something unseen was removed, or a second run found something"), crate::panic::message(panic)),
        }
    }
}

#[test]
fn hand_edits_made_at_random_are_found_and_nothing_the_person_did_not_remove_is_removed() {
    for seed in (1..=8u64).map(|n| n.wrapping_mul(0x9E3779B97F4A7C15) | 1) {
        converge(seed);
    }
}
