//! Convergence: rows and a directory that agree; hand edits made at random; the check; then
//! the rows say what the disk has, and a second run of the check finds nothing changed.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use konedrive_tree::{ContentPresence, ItemKind};

use super::{folder, set_mtime, Folder, MTIME};

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

/// What a person does by hand in the backing directory, made at random.
struct HandEdits<'a> {
    folder: &'a mut Folder,
    numbers: Numbers,
    /// Raised for each name made, so that a made name is free.
    made: usize,
    log: Vec<String>,
}

fn konedrive_marks(path: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let names = xattr::list(path).unwrap().filter(|name| name.as_encoded_bytes().starts_with(b"user.konedrive."));
    names.map(|name| (name.clone(), xattr::get(path, &name).unwrap().unwrap())).collect()
}

/// `cp -a`: the bytes, the time and the marks; a directory with all in it.
fn copy_with_marks(from: &Path, to: &Path) {
    let metadata = fs::symlink_metadata(from).unwrap();
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

impl HandEdits<'_> {
    fn entries(&self, directories: bool) -> Vec<PathBuf> {
        let all = self.folder.walk_disk();
        all.into_iter().filter(|rel| self.folder.disk(rel).is_dir() == directories).collect()
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

    /// A path nothing is at: mostly a name never used, now and then one the upload rules
    /// ignore, and now and then the path of a row that has nothing on disk.
    fn free_path(&mut self) -> PathBuf {
        if self.numbers.below(8) == 0 {
            let rows = self.folder.rows();
            let mut bare = Vec::new();
            for row in rows.iter().filter(|row| row.local.as_ref().is_some_and(|local| local.place.is_some())) {
                let path = self.folder.store.item_store().find_path(row.local_item_id).unwrap().unwrap();
                let parent_is_there = self.folder.disk(path.parent().unwrap()).is_dir();
                if parent_is_there && fs::symlink_metadata(self.folder.disk(&path)).is_err() {
                    bare.push(path);
                }
            }
            if let Some(path) = self.numbers.pick(&bare) {
                return path.clone();
            }
        }
        self.made += 1;
        let name = match self.numbers.below(10) {
            0 => format!("made-{}.tmp", self.made),
            1 => "cache".to_owned(),
            _ => format!("made-{}", self.made),
        };
        let path = self.directory().join(name);
        if fs::symlink_metadata(self.folder.disk(&path)).is_ok() {
            return self.free_path();
        }
        path
    }

    fn is_below(path: &Path, directory: &Path) -> bool {
        path.starts_with(directory)
    }

    fn edit(&mut self) {
        let did = match self.numbers.below(9) {
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
            2 => {
                let files = self.entries(false);
                let Some(path) = self.numbers.pick(&files).cloned() else { return };
                let mut bytes = fs::read(self.folder.disk(&path)).unwrap();
                bytes.extend_from_slice(b"+edit");
                // In place: the marks stay, and the time is a later one.
                fs::write(self.folder.disk(&path), bytes).unwrap();
                set_mtime(&self.folder.disk(&path), MTIME + 100 + self.made as i64);
                self.made += 1;
                format!("edit {path:?}")
            }
            3 | 4 => {
                let Some(from) = self.entry() else { return };
                let to = self.free_path();
                if Self::is_below(&to, &from) {
                    return;
                }
                self.folder.rename(&from, &to);
                format!("mv {from:?} {to:?}")
            }
            5 => {
                let files = self.entries(false);
                let (Some(from), Some(to)) = (self.numbers.pick(&files).cloned(), self.numbers.pick(&files).cloned()) else { return };
                if from == to {
                    return;
                }
                self.folder.rename(&from, &to);
                format!("mv over {from:?} {to:?}")
            }
            6 => {
                let Some(path) = self.entry() else { return };
                let on_disk = self.folder.disk(&path);
                if on_disk.is_dir() { fs::remove_dir_all(on_disk).unwrap() } else { fs::remove_file(on_disk).unwrap() }
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
            _ => {
                let (Some(first), Some(second)) = (self.entry(), self.entry()) else { return };
                if Self::is_below(&first, &second) || Self::is_below(&second, &first) {
                    return;
                }
                let held = self.folder.disk("held-for-an-exchange");
                fs::rename(self.folder.disk(&first), &held).unwrap();
                fs::rename(self.folder.disk(&second), self.folder.disk(&first)).unwrap();
                fs::rename(&held, self.folder.disk(&second)).unwrap();
                format!("exchange {first:?} {second:?}")
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
    f.copy("moved-in", b"a copy in a directory of the user's own", "MI");
    f.move_through_the_mount("moved-in", "mine/moved-in");
    f.not_downloaded("top-remote", 77, "TR");
    f
}

fn converge(seed: u64) {
    let mut f = agreeing_folder();
    f.assert_settled();
    let mut edits = HandEdits { folder: &mut f, numbers: Numbers(seed), made: 0, log: Vec::new() };
    for round in 0..25 {
        edits.log.clear();
        for _ in 0..12 {
            edits.edit();
        }
        let before = edits.folder.rows();
        let findings = edits.folder.check();
        let said = |what: &str| format!("seed {seed:#x}, round {round}: {what}\nedits: {:#?}\nfindings: {:?}", edits.log, super::found(&findings));
        assert_eq!(findings.unresolved(), [], "{}", said("a path was left"));
        let settled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| edits.folder.assert_settled()));
        if let Err(panic) = settled {
            panic!("{}\n{}", said("the rows do not say the disk, or a second run found something"), crate::panic::message(panic));
        }
        // No item of OneDrive is forgotten: its row is shown, or waits for its removal to be sent.
        let now = edits.folder.rows();
        for row in before.iter().filter(|row| row.onedrive_item.is_some()) {
            assert!(now.iter().any(|kept| kept.onedrive_item == row.onedrive_item), "{}", said(&format!("the binding of {row:?} is gone")));
        }
        // A file that is not downloaded stays so, unless something was put at its name or
        // its directory was removed or replaced by hand.
        for row in before.iter().filter(|row| row.local.is_some() && row.kind == ItemKind::File && row.content_presence == ContentPresence::NotOnDisk) {
            let kept = now.iter().find(|kept| kept.local_item_id == row.local_item_id).unwrap();
            let is_as_before = kept.local.is_some() && kept.content_presence == ContentPresence::NotOnDisk;
            let was_touched = edits.log.iter().any(|edit| !edit.starts_with("edit "));
            assert!(is_as_before || was_touched, "{}", said(&format!("{row:?} was not downloaded and is {kept:?}")));
        }
    }
}

#[test]
fn hand_edits_made_at_random_are_found_and_a_second_run_finds_nothing() {
    for seed in (1..=8u64).map(|n| n.wrapping_mul(0x9E3779B97F4A7C15) | 1) {
        converge(seed);
    }
}
