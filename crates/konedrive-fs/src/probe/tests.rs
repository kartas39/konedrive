use super::*;

#[test]
fn a_normal_directory_passes() {
    let dir = tempfile::tempdir().unwrap();
    probe_dir(dir.path()).unwrap();
}

/// Stated as the property rather than as its consequence:
/// the file the probe works on has no name, so there is no window in
/// which a crash can leave an artefact in the folder being registered.
/// `nlink == 0` is the kernel's own answer to "is this reachable by any
/// name", and the directory listing is the same answer from the other
/// side — both taken while the descriptor is still open and the probe is
/// at its most exposed.
#[test]
fn the_probe_file_has_no_name_while_it_is_open() {
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    let file = open_probe_file(dir.path()).unwrap();
    assert_eq!(file.metadata().unwrap().nlink(), 0, "the probe file must be unlinked");
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(entries.is_empty(), "the probe put {entries:?} into the directory");
}

/// Whatever is already sitting under the
/// name the old probe used, the probe no longer cares. A directory is
/// used here because it is the one artefact a `remove_file` cleanup
/// cannot quietly delete — it stands in for "the folder has something
/// under this name and the probe must not be the thing that fails".
#[test]
fn an_artefact_under_the_old_probe_name_does_not_block_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let stale = dir.path().join(".konedrive-probe");
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("left-over"), b"x").unwrap();

    probe_dir(dir.path()).expect("a stale artefact must not make the filesystem look unusable");
}

#[test]
fn a_missing_directory_is_reported_clearly() {
    let error = probe_dir(std::path::Path::new("/definitely/not/here")).unwrap_err();
    assert!(matches!(error, ProbeError::Unusable { .. }), "{error:?}");
}

/// Both dehydration and startup recovery take a write lease before they
/// punch a file's blocks away, and only `EAGAIN` out of a refused
/// `F_SETLEASE` means "try again later" — everything else, including a
/// filesystem that grants no leases at all, is a hard error there. This
/// has to be caught at registration, not discovered the first time a
/// crash-interrupted file exists to recover or a user asks to dehydrate
/// something.
///
/// `probe_dir` itself always works on a fresh, nameless file nothing
/// else can have open, so a genuine refusal cannot be provoked through
/// it without root or a real lease-less filesystem — neither available
/// unprivileged here. `probe_lease` is exercised directly instead,
/// against an ordinary file held open by a second descriptor: the same
/// `F_SETLEASE`, the same `Ok(None)` `EAGAIN` refusal
/// `konedrive_fs::lease`'s own `refused_while_another_descriptor_is_open`
/// measures, going through the mapping this probe adds.
#[test]
fn a_file_a_lease_cannot_be_taken_on_is_reported_unusable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"data").unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    let _other = File::open(&path).unwrap();

    let error = probe_lease(&file, dir.path()).unwrap_err();
    assert!(matches!(error, ProbeError::Unusable { .. }), "{error:?}");
}

// `classify` is exercised directly for both branches below: a genuinely
// unsupported filesystem is only reproducible with a filesystem type
// that lacks the feature (vfat, a tmpfs built without
// CONFIG_TMPFS_XATTR, ...), which needs a real mount and so root; the
// classification logic itself does not, so it is tested with the exact
// errno values the kernel would hand back in each case.

#[test]
fn eopnotsupp_and_enotsup_are_reported_as_missing() {
    let dir = Path::new("/some/dir");
    for errno in [libc::EOPNOTSUPP, libc::ENOTSUP] {
        let error = classify(io::Error::from_raw_os_error(errno), dir, "a feature");
        assert!(
            matches!(error, ProbeError::Missing { feature: "a feature", .. }),
            "errno {errno}: {error:?}"
        );
    }
}

#[test]
fn other_errors_are_reported_as_unusable_with_their_own_text() {
    // ENOSPC and EACCES are two ways a real, fixable-or-not problem with
    // this directory could surface at the exact same call sites that
    // EOPNOTSUPP does; neither means the feature is unsupported, and the
    // caller-visible message must say what actually went wrong instead
    // of claiming the filesystem lacks the feature.
    for errno in [libc::ENOSPC, libc::EACCES] {
        let dir = Path::new("/some/dir");
        let underlying = io::Error::from_raw_os_error(errno);
        let expected_text = underlying.to_string();
        let error = classify(underlying, dir, "a feature");
        match error {
            ProbeError::Unusable { why, .. } => assert_eq!(why, expected_text),
            ProbeError::Missing { .. } => panic!("errno {errno} must not be reported as Missing"),
        }
    }
}
