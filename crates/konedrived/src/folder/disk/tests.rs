use std::time::{Duration, UNIX_EPOCH};

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use konedrive_fs::placeholder::XATTR_ROOT;

use super::*;

/// A directory on the filesystem of the build's `target/` (on this machine
/// `/home`, while temporary directories are on tmpfs).
fn on_target_fs() -> tempfile::TempDir {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::tempdir_in(base.canonicalize().unwrap()).unwrap()
}

fn same_device(a: &Path, b: &Path) -> bool {
    std::fs::metadata(a).unwrap().dev() == std::fs::metadata(b).unwrap().dev()
}

/// The rescue never copies: across filesystems it fails, naming both
/// paths, and leaves the file exactly as it was — content, attributes and
/// mode.
#[test]
fn a_rescue_across_filesystems_fails_and_leaves_the_source_intact() {
    let (_dir, path, disk) = unlocked_root();
    let into = on_target_fs();
    if same_device(&path, into.path()) {
        eprintln!("skipping: {} and {} are on one filesystem", path.display(), into.path().display());
        return;
    }
    let source = path.join("f.txt");
    std::fs::write(&source, b"local work").unwrap();
    xattr::set(&source, XATTR_ITEM_ID, b"F").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o444)).unwrap();
    let root = disk.dir(Path::new("")).unwrap();
    let err = disk.rescue(&root, OsStr::new("f.txt"), Path::new("docs/f.txt"), into.path()).unwrap_err();
    let message = err.to_string();
    assert!(message.contains(&source.display().to_string()), "{message}");
    assert!(message.contains(&into.path().join("docs/f.txt").display().to_string()), "{message}");
    assert_eq!(std::fs::read(&source).unwrap(), b"local work");
    assert_eq!(xattr::get(&source, XATTR_ITEM_ID).unwrap().as_deref(), Some(&b"F"[..]));
    assert_eq!(std::fs::metadata(&source).unwrap().permissions().mode() & 0o7777, 0o444);
    assert!(!into.path().join("docs/f.txt").exists());
}

/// Rescues go where the user expects them whenever a rename can get them
/// there: the preferred place, not yet made, on the root's filesystem.
#[test]
fn the_rescue_base_is_the_preferred_one_on_the_roots_filesystem() {
    let (_dir, path, _disk) = unlocked_root();
    let data = tempfile::tempdir().unwrap();
    if !same_device(&path, data.path()) {
        eprintln!("skipping: {} and {} are on different filesystems", path.display(), data.path().display());
        return;
    }
    let preferred = data.path().join("konedrive/rescued");
    assert_eq!(rescue_base(&path, &preferred), preferred);
}

/// On another filesystem a rescue would need a copy; it goes beside the
/// root instead, where one rename reaches.
#[test]
fn the_rescue_base_is_beside_the_root_when_the_preferred_one_is_on_another_filesystem() {
    let parent = on_target_fs();
    let root = parent.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    let data = tempfile::tempdir().unwrap();
    if same_device(&root, data.path()) {
        eprintln!("skipping: {} and {} are on one filesystem", root.display(), data.path().display());
        return;
    }
    let preferred = data.path().join("konedrive/rescued");
    assert_eq!(rescue_base(&root, &preferred), parent.path().join(".konedrive-rescued-OneDrive"));
}

/// An item id becomes a name in the holding directory, so an id that
/// cannot be one is never taken for ours.
#[test]
fn an_item_id_that_cannot_be_a_file_name_is_not_ours() {
    let (_dir, path, disk) = unlocked_root();
    let root = disk.dir(Path::new("")).unwrap();
    for (n, id) in [&b""[..], b".", b"..", b"a/b", b"a\0b"].into_iter().enumerate() {
        let name = format!("f{n}");
        std::fs::write(path.join(&name), b"").unwrap();
        xattr::set(path.join(&name), XATTR_ITEM_ID, id).unwrap();
        assert_eq!(disk.probe(&root, OsStr::new(&name)).unwrap(), Probe::Unmanaged { is_dir: false }, "{id:?}");
    }
    std::fs::create_dir(path.join("d")).unwrap();
    xattr::set(path.join("d"), XATTR_ITEM_ID, b"..").unwrap();
    assert_eq!(disk.probe(&root, OsStr::new("d")).unwrap(), Probe::Unmanaged { is_dir: true });
    std::fs::write(path.join("ok"), b"").unwrap();
    xattr::set(path.join("ok"), XATTR_ITEM_ID, b"8F6C!101").unwrap();
    assert_eq!(disk.probe(&root, OsStr::new("ok")).unwrap(), Probe::Managed { id: "8F6C!101".into(), is_dir: false });
}

/// An unlocked registered root, its path, and the `Disk` on it.
fn unlocked_root() -> (tempfile::TempDir, PathBuf, Disk) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    let root_id = "3a9d5c1e-7f20-4b6a-8e4d-2c1b0a9f8e7d".to_owned();
    xattr::set(&path, XATTR_ROOT, root_id.as_bytes()).unwrap();
    let disk = Disk::open(&SyncRoot { path: path.clone(), root_id }, false).unwrap();
    (dir, path, disk)
}

/// A rename never lands on anything: whatever appeared at the target
/// between the materializer's probe and its rename is left alone, and the
/// move fails instead.
#[test]
fn a_rename_never_replaces_what_is_at_the_target() {
    let (_dir, path, disk) = unlocked_root();
    std::fs::write(path.join("a"), b"ours").unwrap();
    std::fs::write(path.join("b"), b"the user's").unwrap();
    let root = disk.dir(Path::new("")).unwrap();
    let err = disk.rename(&root, OsStr::new("a"), &root, OsStr::new("b")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
    assert_eq!(std::fs::read(path.join("b")).unwrap(), b"the user's");
    assert_eq!(std::fs::read(path.join("a")).unwrap(), b"ours");
}

/// A `swap_in` whose `renameat` fails after its
/// `linkat` already succeeded does not leave the temporary link behind —
/// a leftover a later Full reconcile would find under the same item id
/// as the file it was meant to replace.
#[test]
fn swap_in_removes_its_temporary_link_when_the_rename_fails() {
    let (_dir, path, disk) = unlocked_root();
    let root = disk.dir(Path::new("")).unwrap();
    // Renaming a regular file onto an existing directory always fails
    // (EISDIR), which is enough to exercise the cleanup regardless of
    // filesystem or kernel.
    std::fs::create_dir(path.join("target")).unwrap();
    let file = disk.tmpfile(&root).unwrap();
    let err = disk.swap_in(&root, &file, OsStr::new("temp"), OsStr::new("target")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EISDIR), "{err:?}");
    assert!(!path.join("temp").exists(), "the temporary link does not outlive a failed swap");
    assert!(path.join("target").is_dir(), "the target is untouched");
}

/// A rescue never replaces an earlier one: whatever is already at the
/// destination — rescued before, or put there in a race — stays, and the
/// file goes to the next free `.<n>` name.
#[test]
fn a_rescue_never_replaces_what_is_already_there() {
    let (_dir, path, disk) = unlocked_root();
    std::fs::create_dir(path.join("docs")).unwrap();
    std::fs::write(path.join("docs/f.txt"), b"third").unwrap();
    let into = tempfile::tempdir().unwrap();
    std::fs::create_dir(into.path().join("docs")).unwrap();
    std::fs::write(into.path().join("docs/f.txt"), b"first").unwrap();
    std::fs::write(into.path().join("docs/f.txt.1"), b"second").unwrap();
    let docs = disk.dir(Path::new("docs")).unwrap();
    let dest = disk.rescue(&docs, OsStr::new("f.txt"), Path::new("docs/f.txt"), into.path()).unwrap().expect("local work is rescued");
    assert_eq!(dest, into.path().join("docs/f.txt.2"));
    assert_eq!(std::fs::read(into.path().join("docs/f.txt")).unwrap(), b"first");
    assert_eq!(std::fs::read(into.path().join("docs/f.txt.1")).unwrap(), b"second");
    assert_eq!(std::fs::read(&dest).unwrap(), b"third");
    assert!(!path.join("docs/f.txt").exists());
}

/// A file of ours.
fn managed(path: &Path, content: &[u8], state: placeholder::State) {
    std::fs::write(path, content).unwrap();
    xattr::set(path, XATTR_ITEM_ID, b"F").unwrap();
    placeholder::write_state(&File::open(path).unwrap(), state).unwrap();
}

/// A placeholder is never rescued, wherever it is: stripped of its state it would lie
/// among the rescued files as a file of zeros. Named itself it is removed where it is;
/// inside a rescued directory it is removed there. A downloaded file is rescued whole.
#[test]
fn a_placeholder_is_removed_not_rescued_at_any_depth() {
    use placeholder::State;
    let (_dir, path, disk) = unlocked_root();
    let into = tempfile::tempdir().unwrap();
    let root = disk.dir(Path::new("")).unwrap();

    for (name, state) in [("a", State::OnlineOnly), ("b", State::Hydrating), ("c", State::Dehydrating)] {
        managed(&path.join(name), &[0u8; 4096], state);
        let rescued = disk.rescue(&root, OsStr::new(name), Path::new(name), into.path()).unwrap();
        assert_eq!(rescued, None, "{state:?}");
        assert!(!path.join(name).exists(), "{state:?}: removed from the folder");
        assert!(!into.path().join(name).exists(), "{state:?}: and not left as zeros among the rescued");
    }

    std::fs::create_dir(path.join("docs")).unwrap();
    managed(&path.join("docs/placeholder"), &[0u8; 4096], State::OnlineOnly);
    managed(&path.join("docs/downloaded"), b"content", State::Hydrated);
    std::fs::write(path.join("docs/mine"), b"made here").unwrap();
    let rescued = disk.rescue(&root, OsStr::new("docs"), Path::new("docs"), into.path()).unwrap().unwrap();
    assert!(!rescued.join("placeholder").exists());
    assert_eq!(std::fs::read(rescued.join("downloaded")).unwrap(), b"content");
    assert_eq!(xattr::get(rescued.join("downloaded"), XATTR_ITEM_ID).unwrap(), None, "the user's own now");
    assert_eq!(std::fs::read(rescued.join("mine")).unwrap(), b"made here");
}

/// The scan is the reconcile's picture of the folder: a directory it cannot look into
/// fails it, with an error that names the directory, rather than being left out as if it
/// were empty.
///
/// A directory set to mode `000` fails at the look its parent takes at it, as it did
/// before the scan had one rule. What the rule adds — a directory that can be looked at
/// and then not opened or listed (`EIO`, `EMFILE`, a swap mid-walk) — no unprivileged test
/// can make happen (the limitations log, D51).
#[test]
fn a_scan_that_cannot_read_a_directory_fails_and_names_it() {
    let (_dir, path, disk) = unlocked_root();
    std::fs::create_dir(path.join("open")).unwrap();
    std::fs::write(path.join("open/f"), b"x").unwrap();
    assert_eq!(disk.scan("ROOT").unwrap().len(), 2);

    std::fs::create_dir(path.join("shut")).unwrap();
    std::fs::set_permissions(path.join("shut"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let scanned = disk.scan("ROOT");
    std::fs::set_permissions(path.join("shut"), std::fs::Permissions::from_mode(0o755)).unwrap();
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let message = scanned.unwrap_err().to_string();
    assert!(message.contains("cannot scan shut"), "{message}");
}

/// Putting the lock back passes over a directory it cannot look into, and locks the rest
/// and the folder itself.
#[test]
fn lock_tree_passes_over_what_it_cannot_enter_and_locks_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    let root_id = "3a9d5c1e-7f20-4b6a-8e4d-2c1b0a9f8e7d".to_owned();
    xattr::set(&path, XATTR_ROOT, root_id.as_bytes()).unwrap();
    for name in ["docs", "shut"] {
        std::fs::create_dir(path.join(name)).unwrap();
        xattr::set(path.join(name), XATTR_ITEM_ID, name.as_bytes()).unwrap();
    }
    managed(&path.join("docs/f"), b"content", placeholder::State::Hydrated);
    std::fs::set_permissions(path.join("shut"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let mode = |name: &str| std::fs::metadata(path.join(name)).unwrap().permissions().mode() & 0o7777;

    let disk = Disk::open(&SyncRoot { path: path.clone(), root_id }, true).unwrap();
    let locked = disk.lock_tree(|_| Ok(Some(())));
    let modes = (mode("docs"), mode(""), mode("shut"));
    // So that the temporary directory can be removed.
    for name in ["shut", "docs", ""] {
        std::fs::set_permissions(path.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let file_mode = mode("docs/f");

    locked.unwrap();
    assert_eq!(modes, (LOCKED_DIR_MODE, LOCKED_DIR_MODE, 0o000));
    assert_eq!(file_mode, LOCKED_FILE_MODE);
}

/// Two folders do not wait for each other's windows, and every `Disk` of one folder
/// shares that folder's lock.
#[test]
fn the_modes_lock_is_one_for_a_folder_and_another_for_the_next() {
    let (_a_dir, a_path, _a_disk) = unlocked_root();
    let (_b_dir, b_path, _b_disk) = unlocked_root();
    let a_root = SyncRoot { path: a_path, root_id: String::new() };
    let b_root = SyncRoot { path: b_path, root_id: String::new() };
    let (a_modes, b_modes) = (Modes::of_root(&a_root).unwrap(), Modes::of_root(&b_root).unwrap());
    assert!(Arc::ptr_eq(&a_modes, &Modes::of_root(&a_root).unwrap()));
    assert!(!Arc::ptr_eq(&a_modes, &b_modes));

    let _a_held = a_modes.hold();
    std::thread::spawn(move || drop(b_modes.hold())).join().unwrap();
}

#[test]
fn a_rescue_stamp_is_the_utc_date_and_time() {
    assert_eq!(rescue_stamp(UNIX_EPOCH + Duration::from_secs(1_714_557_600)), "2024-05-01T10-00-00Z");
}

#[test]
fn the_epoch_is_the_first_stamp() {
    assert_eq!(rescue_stamp(UNIX_EPOCH), "1970-01-01T00-00-00Z");
}
