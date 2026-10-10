//! Convergence: rows and a directory that agree; hand edits made at random; the check; then
//! the rows say what the disk has, and a second run of the check finds nothing changed.
//!
//! The rows saying the disk is not enough: they do so after a wrong removal too. So the
//! test keeps a model of what the person could see and did ([`SeenByThePerson`]): which
//! entry of the backing directory is which item's own, and which of those were removed or
//! moved over by hand. After each check, an item of OneDrive that the folder does not show
//! any more is one whose own entry the person removed, or one that lost its place and is
//! not removed; and an item whose own entry is where it was keeps its row.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::{ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, LocalItemId, LocalVersionChange};

use super::{folder, set_mtime, DiskFindings, Folder, UnresolvedReason, MTIME};

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

/// What the person could see in the backing directory, and what they did to it by hand.
#[derive(Default)]
struct SeenByThePerson {
    /// The entries that are an item's own: each path with the item whose row was at it when
    /// the rows last said the disk, followed through every move made by hand since.
    own_entries: BTreeMap<PathBuf, LocalItemId>,
    /// Where each of those was when the rows last said the disk.
    paths_at_start: BTreeMap<LocalItemId, PathBuf>,
    /// The items whose own entry was removed or moved over by hand, ever.
    removed_by_hand: HashSet<LocalItemId>,
    /// The items of OneDrive whose own entry does not carry their mark: it had its marks
    /// taken off or changed by hand, or it is a file the person put where one was not
    /// downloaded. Nothing tells such an entry from a new file once it is moved.
    unmarked: HashSet<LocalItemId>,
}

impl SeenByThePerson {
    /// The rows say the disk: every entry is the own entry of the row at its path.
    fn begin(folder: &mut Folder, left: &[PathBuf]) -> BTreeMap<PathBuf, LocalItemId> {
        let paths: Vec<PathBuf> = folder.walk_disk().into_iter().filter(|path| !left.contains(path)).collect();
        paths.into_iter().map(|path| (path.clone(), folder.id(&path))).collect()
    }

    fn start_over(&mut self, folder: &mut Folder, left: &[PathBuf]) {
        self.own_entries = Self::begin(folder, left);
        self.paths_at_start = self.own_entries.iter().map(|(path, item)| (*item, path.clone())).collect();
        self.note_unmarked(folder);
    }

    /// The own entries of items of OneDrive that do not carry their item's mark.
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

    /// Whether the person moved the own entry of `item`, which carries no mark to be found
    /// by: to the check the item is gone from its place, as if removed.
    fn is_moved_unmarked(&self, item: LocalItemId) -> bool {
        self.unmarked.contains(&item) && self.is_where_it_was(item).is_none()
    }

    /// Where the own entry of `item` is, when that is where it was when the rows last said
    /// the disk.
    fn is_where_it_was(&self, item: LocalItemId) -> Option<&Path> {
        let at_start = self.paths_at_start.get(&item)?;
        (self.own_entries.get(at_start) == Some(&item)).then_some(at_start.as_path())
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

/// A symbolic link, a file that is not whole, and the files of one item id in two drives are
/// not picked for an edit of their own: the first two are left by the check whatever is
/// done, and of the last the mark alone does not say which item a moved one is. A directory
/// that holds them is edited like any other.
fn is_picked_alone(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    !(name.starts_with("link-") || name.starts_with("partial-") || name.starts_with("twin-"))
}

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

    fn edit(&mut self) {
        let did = match self.numbers.below(14) {
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

/// A file of the item `onedrive_id` of the drive `drive`: a clean copy on disk, or one that
/// is not downloaded.
fn file_of_drive(f: &mut Folder, rel: &str, drive: &str, onedrive_id: &str, on_disk: bool) {
    let item = if on_disk { f.own_file(rel, b"twin") } else { f.make_row(rel, ItemKind::File, Some(ContentVersion::ChangedHere { size: 4, mtime: MTIME, mtime_nsec: 0, change_count: 1 })) };
    if on_disk {
        f.mark_copy(rel, onedrive_id);
    }
    f.agree_in(item, drive, onedrive_id);
    if !on_disk {
        f.store.item_store().set_content_presence(item, ContentPresence::NotOnDisk).unwrap();
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
    let shared = f.own_directory("shared");
    f.mark_directory("shared", "S");
    f.agree_in(shared, "d2", "S");
    file_of_drive(&mut f, "docs/twin-a", "d1", "TA", true);
    file_of_drive(&mut f, "shared/twin-a", "d2", "TA", true);
    file_of_drive(&mut f, "docs/twin-b", "d1", "TB", false);
    file_of_drive(&mut f, "shared/twin-b", "d2", "TB", true);
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

/// What the rows may say after a check, of the rows `before` it and of what the person did.
fn assert_nothing_unseen_is_removed(seen: &SeenByThePerson, before: &[Item], now: &[Item], f: &mut Folder) {
    let find = |id: LocalItemId| now.iter().find(|row| row.local_item_id == id);
    let is_another_at = |was: &Item| {
        let place = was.local.as_ref().and_then(|local| local.place.as_ref());
        now.iter().any(|row| row.local_item_id != was.local_item_id && row.local.as_ref().and_then(|local| local.place.as_ref()) == place)
    };
    for was in before.iter().filter(|row| is_shown(row) && row.local_item_id != LocalItemId::ROOT) {
        let id = was.local_item_id;
        // An item whose own entry is where it was, its marks untouched, keeps its row there.
        if let Some(path) = seen.is_where_it_was(id).filter(|_| !seen.unmarked.contains(&id)) {
            assert_eq!(f.row(path).map(|row| row.local_item_id), Some(id), "the entry at {path:?} is where it was, and its item is not: {was:?}");
        }
        let Some(binding) = &was.onedrive_item else {
            // An item OneDrive never had loses its row only when its entry was removed, or
            // moved: it carries no mark to be found by.
            assert!(find(id).is_some() || seen.is_where_it_was(id).is_none(), "the row of {was:?} is gone, and its entry is where it was");
            continue;
        };
        // No item of OneDrive is forgotten.
        let is = now.iter().find(|row| row.onedrive_item.as_ref() == Some(binding)).unwrap_or_else(|| panic!("the binding of {was:?} is gone"));
        if is_shown(is) {
            continue;
        }
        let was_agreed = was.synced_place.is_some();
        let is_removed_here = is.synced_place.is_some();
        if is_removed_here {
            // Removed here, its removal to be sent: only a file whose copy the person had
            // before them and removed or put something over.
            assert!(has_copy_on_disk(was), "an item of OneDrive with nothing of its own on disk is removed here: {was:?}");
            // Or moved, when it carries no mark: nothing says then that the file at the new
            // place is the item.
            assert!(seen.removed_by_hand.contains(&id) || seen.is_moved_unmarked(id), "an item of OneDrive is removed here whose entry nobody removed: {was:?}");
            assert!(is_changed_here(was) || is_another_at(was), "a clean copy that is gone is removed here, with nothing put at its name: {was:?}");
        } else if was_agreed {
            // Its place lost: nothing of it is to be sent, and it was not a file changed
            // here that the person had before them.
            assert!(!(has_copy_on_disk(was) && is_changed_here(was)), "a file changed here lost its place instead of being removed: {was:?}");
            assert_eq!((is.local_item_id, is.unsent_since, is.content_presence), (id, None, ContentPresence::NotOnDisk), "{is:?}");
        } else {
            // Bound by its mark and never agreed on: its row cannot tell a removal from a
            // lost place. A clean copy that is merely gone stays shown, as not on disk.
            let parent_is_shown = was.local.as_ref().and_then(|local| local.place.as_ref()).and_then(|place| find(place.parent_local_item_id)).is_some_and(is_shown);
            assert!(is_another_at(was) || !parent_is_shown || (is_changed_here(was) && (seen.removed_by_hand.contains(&id) || seen.is_moved_unmarked(id))), "a copy bound by its mark is not shown any more, and nothing took its place: {was:?}");
        }
    }
}

/// The paths a check may leave whatever the person did: a symbolic link, and a file that is
/// not whole and marked with an id no row has.
fn list_left(findings: &DiskFindings, said: &dyn Fn(&str) -> String) -> Vec<PathBuf> {
    for left in findings.unresolved() {
        assert!(matches!(left.reason, UnresolvedReason::NotFileOrDirectory | UnresolvedReason::PartialCopy), "{}", said(&format!("{left:?} was left")));
    }
    findings.unresolved().iter().map(|left| left.path.clone()).collect()
}

fn converge(seed: u64) {
    // SAFETY: no arguments, no failure.
    let is_root = unsafe { libc::geteuid() == 0 };
    let mut f = agreeing_folder();
    f.assert_settled();
    let mut edits = HandEdits { folder: &mut f, numbers: Numbers(seed), made: 0, log: Vec::new(), seen: SeenByThePerson::default() };
    edits.seen.start_over(edits.folder, &[]);
    for round in 0..25 {
        edits.log.clear();
        for _ in 0..12 {
            edits.edit();
        }
        let before = edits.folder.rows();
        // Now and then a directory cannot be read by a first check, and a second can read
        // everything: the start after the person put the directory right.
        let locked = if !is_root && edits.numbers.below(5) == 0 { edits.entries(true).first().cloned() } else { None };
        if let Some(locked) = &locked {
            fs::set_permissions(edits.folder.disk(locked), fs::Permissions::from_mode(0o000)).unwrap();
            let findings = edits.folder.check();
            fs::set_permissions(edits.folder.disk(locked), fs::Permissions::from_mode(0o755)).unwrap();
            let said = |what: &str| format!("seed {seed:#x}, round {round}, unreadable {locked:?}: {what}\nedits: {:#?}\nfindings: {:?}\nleft: {:?}", edits.log, super::found(&findings), findings.unresolved());
            // Nothing leaves the folder while part of it was not read: every item it showed
            // it shows still.
            assert!(!findings.is_read_whole(), "{}", said("a directory could not be read, and the check says it read everything"));
            let now = edits.folder.rows();
            for was in before.iter().filter(|row| is_shown(row)) {
                assert!(now.iter().any(|row| row.local_item_id == was.local_item_id && is_shown(row)), "{}", said(&format!("{was:?} left the folder while a directory was not read")));
            }
        }
        let findings = edits.folder.check();
        let said = |what: &str| format!("seed {seed:#x}, round {round}, unreadable at first {locked:?}: {what}\nedits: {:#?}\nfindings: {:?}\nleft: {:?}", edits.log, super::found(&findings), findings.unresolved());
        assert!(findings.is_read_whole(), "{}", said("the backing directory was not read whole"));
        let now = edits.folder.rows();
        let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let left = list_left(&findings, &said);
            assert_nothing_unseen_is_removed(&edits.seen, &before, &now, edits.folder);
            edits.folder.assert_settled_but(&left);
            left
        }));
        match checked {
            Ok(left) => edits.seen.start_over(edits.folder, &left),
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
