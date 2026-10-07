use std::fs::File;
use std::path::Path;

use konedrive_fs::placeholder::XATTR_ROOT;
use xattr::FileExt;

use crate::helper::HelperLink;
use crate::hydration::dehydrate::tests::{asked_to, fake_helper, fake_helper_refusing_everything};
use super::*;

/// A sync root the way `register_root` would have left one: resolved
/// path, root id on the folder.
pub(crate) fn test_root(dir: &Path) -> SyncRoot {
    let path = dir.canonicalize().unwrap();
    let root_id = uuid_v4();
    let handle = File::open(&path).unwrap();
    handle.set_xattr(XATTR_ROOT, root_id.as_bytes()).unwrap();
    SyncRoot { path, root_id }
}

// --- The local guard -------------------------------------------------

/// A value on `user.konedrive.root` that is not text is no id of ours, like any other the
/// daemon could not have minted: an empty folder that carries one is registered, and a
/// fresh id is written over it.
#[tokio::test]
async fn an_empty_folder_with_a_root_attribute_that_is_not_text_gets_a_fresh_id() {
    let dir = tempfile::tempdir().unwrap();
    File::open(dir.path()).unwrap().set_xattr(XATTR_ROOT, &[0xff, 0xfe, 0x00]).unwrap();

    let root = register_root_unprotected(dir.path()).await.unwrap();

    assert!(looks_like_a_root_id(&root.root_id), "{}", root.root_id);
    assert_eq!(xattr::get(dir.path(), XATTR_ROOT).unwrap().as_deref(), Some(root.root_id.as_bytes()));
    assert!(root.open_registered().unwrap().is_some());
}

#[tokio::test]
async fn refuses_a_non_empty_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stray.txt"), b"x").unwrap();
    let error = check_root_candidate(dir.path()).unwrap_err();
    assert!(matches!(error, RegisterError::NotEmpty), "{error:?}");
}

#[tokio::test]
async fn accepts_an_empty_directory_on_a_supported_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    check_root_candidate(dir.path()).unwrap();
}

/// A file, a missing path and a symlink are all "not a folder you can
/// register", and each has to say so as itself rather than as whatever
/// the next syscall along happens to complain about.
#[tokio::test]
async fn refuses_anything_that_is_not_a_real_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    assert!(
        matches!(check_root_candidate(&file).unwrap_err(), RegisterError::NotADirectory),
        "a plain file must be refused as not a directory"
    );
    assert!(
        matches!(
            check_root_candidate(&dir.path().join("nope")).unwrap_err(),
            RegisterError::NotADirectory
        ),
        "a path that does not exist must be refused as not a directory"
    );

    let target = dir.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let error = check_root_candidate(&link).unwrap_err();
    assert!(
        matches!(&error, RegisterError::Unsupported(why) if why.contains("symbolic link")),
        "{error:?}"
    );
}

/// The probe is not decoration: a directory can be empty, be a
/// directory, and still be unable to hold a single placeholder. Here it
/// is one we cannot write into at all — the cheapest unprivileged stand-in
/// for a filesystem that refuses the features, and the one thing that
/// fails if `probe_dir` is dropped from the checks.
#[tokio::test]
async fn refuses_a_directory_the_probe_cannot_use() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let readonly = dir.path().join("readonly");
    std::fs::create_dir(&readonly).unwrap();
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();

    let error = check_root_candidate(&readonly).unwrap_err();

    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(&error, RegisterError::Unsupported(why) if why.contains(&readonly.display().to_string())),
        "{error:?}"
    );
}

/// The empty requirement belongs to *first* registration
/// only. A folder that already carries a valid `user.konedrive.root`
/// is a root being re-registered, and re-registration is expected to
/// find it full of exactly the placeholders and hydrated files this
/// daemon itself put there — refusing it as though it were some other,
/// foreign non-empty folder would make every restart unregister every
/// root.
/// H78 waives the empty check for a folder that "already
/// carries a root id", and nothing ever removes that xattr again — so if
/// any string counts, one `setfattr -n user.konedrive.root -v x` makes a
/// folder full of somebody's existing documents registerable, for good.
/// Only the id form this daemon actually mints may waive it.
#[tokio::test]
async fn a_value_that_is_not_one_of_our_root_ids_does_not_waive_the_empty_check() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("their-thesis.odt"), b"x").unwrap();
    let handle = File::open(dir.path()).unwrap();

    for bogus in [
        "",                                     // present but empty
        "x",                                    // the one-character setfattr
        "not-a-uuid",                           // a word
        "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5",  // 35 characters
        "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d6", // 37
        "1c2e4f5a-0b3c-3d5e-8f60-71829a3b4c5d", // version 3, not 4
        "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4czd", // not hex
        "1c2e4f5a0b3c4d5e8f6071829a3b4c5d6e7f", // 36 characters, no dashes
    ] {
        handle.set_xattr(XATTR_ROOT, bogus.as_bytes()).unwrap();
        let error = check_root_candidate(dir.path()).unwrap_err();
        assert!(
            matches!(error, RegisterError::NotEmpty),
            "{bogus:?} waived the empty check: {error:?}"
        );
    }

    handle.set_xattr(XATTR_ROOT, uuid_v4().as_bytes()).unwrap();
    check_root_candidate(dir.path()).expect("a real root id must still waive it");
}

/// The other half of H90: a value that is not an id of ours names no
/// registration the helper could be holding, so "never
/// overwrite an existing id" does not apply to it — a real one is minted
/// over the top rather than the junk being offered to the helper as this
/// root's name.
#[tokio::test]
async fn a_bogus_root_id_is_replaced_by_a_real_one() {
    let dir = tempfile::tempdir().unwrap();
    File::open(dir.path()).unwrap().set_xattr(XATTR_ROOT, b"x").unwrap();

    let (_dir, root) = prepare_root(dir.path()).unwrap();

    // Spelled out rather than asked of `looks_like_a_root_id`, which is
    // the function under test: it would agree with itself.
    assert_ne!(root.root_id, "x", "the junk value was offered to the helper as this root");
    assert_eq!(root.root_id.len(), 36, "{}", root.root_id);
    let fields: Vec<&str> = root.root_id.split('-').collect();
    assert_eq!(fields.iter().map(|f| f.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
    assert!(fields[2].starts_with('4'), "{}", root.root_id);
    assert_eq!(
        File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap().as_deref(),
        Some(root.root_id.as_bytes())
    );
}

/// The probe answers first. `read_root_id` is a `getxattr` in
/// the `user.*` namespace — the very thing the probe exists to establish
/// is available — so asking it first replaces the probe's purpose-built
/// message with an errno about an attribute name, on a folder it does not
/// name. Here the folder is both unusable and non-empty, and it is the
/// unusability that must be reported: a folder that cannot hold a
/// placeholder at all cannot be a sync root whether it is empty or not.
#[tokio::test]
async fn an_unusable_directory_is_reported_as_unusable_even_when_it_is_not_empty() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let readonly = dir.path().join("readonly");
    std::fs::create_dir(&readonly).unwrap();
    std::fs::write(readonly.join("stray.txt"), b"x").unwrap();
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();

    let error = check_root_candidate(&readonly).unwrap_err();

    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(&error, RegisterError::Unsupported(why) if why.contains(&readonly.display().to_string())),
        "{error:?}"
    );
}

#[tokio::test]
async fn accepts_a_non_empty_directory_that_already_carries_its_own_root_id() {
    let dir = tempfile::tempdir().unwrap();
    let handle = File::open(dir.path()).unwrap();
    handle.set_xattr(XATTR_ROOT, uuid_v4().as_bytes()).unwrap();
    std::fs::write(dir.path().join("stray.txt"), b"x").unwrap();

    check_root_candidate(dir.path()).unwrap();
}

// --- Root registration -----------------------------------------------

#[tokio::test]
async fn register_root_stamps_the_folder_and_tells_the_helper() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = fake_helper(socket_path.clone(), 0, || {});
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let root = register_root(&link, dir.path()).await.unwrap();

    assert_eq!(root.path, dir.path().canonicalize().unwrap());
    let on_disk = File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap();
    assert_eq!(
        on_disk.as_deref(),
        Some(root.root_id.as_bytes()),
        "the folder must carry the id the helper was told about"
    );
    asked_to(&helper, "RegisterRoot");
}

/// The first attempt fails after the folder has been
/// stamped; the second must offer the helper the *same* id, because the
/// helper may already be holding it — a fresh one is refused as a
/// conflicting registration of the same directory, for good.
#[tokio::test]
async fn register_root_reuses_the_id_already_on_the_folder() {
    let dir = tempfile::tempdir().unwrap();

    let refusing = tempfile::tempdir().unwrap();
    let refusing_socket = refusing.path().join("helper.sock");
    let _refusing = fake_helper_refusing_everything(refusing_socket.clone());
    let (refused_link, _r) = HelperLink::connect(&refusing_socket).await.unwrap();
    let error = register_root(&refused_link, dir.path()).await.unwrap_err();
    assert!(matches!(error, RegisterError::Helper(_)), "{error:?}");

    let first = File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap().unwrap();

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let root = register_root(&link, dir.path()).await.unwrap();

    assert_eq!(
        root.root_id.as_bytes(),
        first.as_slice(),
        "the retry minted a new id; the helper would refuse it as a second registration of \
         the same directory, EINVAL, forever"
    );
    let on_disk = File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap();
    assert_eq!(on_disk.as_deref(), Some(first.as_slice()));
}

/// The actual startup scenario: the daemon registers a
/// folder, populates it (placeholders, hydrated files — anything, here
/// just a plain file stands in), then restarts and registers the same
/// folder again. The second call must not be refused `NotEmpty`.
#[tokio::test]
async fn register_root_accepts_a_populated_folder_on_a_second_registration() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let first = register_root(&link, dir.path()).await.unwrap();

    // Stand in for what a real run would have left behind.
    std::fs::write(dir.path().join("placeholder.bin"), vec![1u8; 4096]).unwrap();

    let second = register_root(&link, dir.path()).await.unwrap();
    assert_eq!(
        second.root_id, first.root_id,
        "re-registration must not mint a new id (Ruling H70)"
    );
}

#[test]
fn uuid_v4_mints_a_fresh_identifier_every_time() {
    let minted: std::collections::HashSet<String> = (0..64).map(|_| uuid_v4()).collect();
    assert_eq!(minted.len(), 64, "root ids must be unique: two folders must never collide");
    for id in &minted {
        assert_eq!(id.len(), 36, "{id}");
        let fields: Vec<&str> = id.split('-').collect();
        assert_eq!(fields.iter().map(|f| f.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()), "{id}");
        assert!(fields[2].starts_with('4'), "version 4 expected: {id}");
        assert!(matches!(&fields[3][0..1], "8" | "9" | "a" | "b"), "variant expected: {id}");
    }
}
