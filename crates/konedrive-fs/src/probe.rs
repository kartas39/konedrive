//! Checks that a directory's filesystem can host placeholders at all.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

use crate::lease::WriteLease;
use crate::placeholder::punch_all;

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// Something about this directory, right now, stopped the probe.
    /// `errno` is the underlying OS error where there was one, so a caller can
    /// tell a filesystem that cannot host placeholders from a caller that is
    /// merely not allowed to write here — the privileged helper runs under
    /// `ProtectHome=read-only` and will see `EROFS` for a directory that is
    /// otherwise perfectly good.
    #[error("{path}: {why}")]
    Unusable { path: String, why: String, errno: Option<i32> },
    #[error("{path}: the filesystem does not support {feature}")]
    Missing { path: String, feature: &'static str },
}

/// Classifies an I/O error from one of the feature checks below. Only
/// `EOPNOTSUPP`/`ENOTSUP` (the two names Linux uses interchangeably for "the
/// filesystem does not implement this operation at all") genuinely indicate
/// an unsupported feature; anything else — `ENOSPC`, a permissions problem,
/// `EROFS`, whatever — is a real, fixable-or-not problem with this
/// particular directory right now, not a feature gap, and must not be
/// reported as one: `probe_dir` runs when the user registers a folder, and
/// this is the error text they see.
fn classify(e: io::Error, dir: &Path, feature: &'static str) -> ProbeError {
    match e.raw_os_error() {
        // EOPNOTSUPP and ENOTSUP share the same numeric value on Linux, so
        // this is a single arm at runtime — written as two names (a match
        // guard, not two patterns, or the second would be flagged
        // unreachable) because both are the documented errno for "operation
        // not supported" and callers may reasonably expect either to be
        // handled.
        Some(errno) if errno == libc::EOPNOTSUPP || errno == libc::ENOTSUP => {
            ProbeError::Missing { path: dir.display().to_string(), feature }
        }
        _ => ProbeError::Unusable {
            path: dir.display().to_string(),
            why: e.to_string(),
            errno: e.raw_os_error(),
        },
    }
}

/// Opens the nameless file the probe works on.
///
/// `O_TMPFILE` gives a real inode on the real filesystem, with a real
/// directory as its parent, and no directory entry anywhere — the same
/// construction `placeholder::create_placeholder` uses, minus the `linkat`
/// that would give it a name. It is unlinked from birth, so it disappears
/// when the descriptor closes, *including* when that happens because the
/// process was killed mid-probe.
///
/// The name-based version this replaces could not say that. It created
/// `.konedrive-probe` with `create_new`, and a crash between that and the
/// `remove_file` at the end left the artefact behind — in a folder the user
/// is registering, which is required to be empty and is checked for
/// emptiness *before* the probe ever runs. The folder was then rejected
/// forever with "the folder must be empty" while `ls` showed nothing,
/// because the leftover is a dotfile. Not creating a name at all removes the
/// failure rather than narrowing its window.
fn open_probe_file(dir: &Path) -> io::Result<File> {
    let fd = nix::fcntl::open(
        dir,
        OFlag::O_TMPFILE | OFlag::O_RDWR | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )?;
    Ok(File::from(fd))
}

/// Creates one nameless temporary file in `dir` and exercises every feature
/// a placeholder needs: sparse size, hole punching, user xattrs and a write
/// lease.
pub fn probe_dir(dir: &Path) -> Result<(), ProbeError> {
    let unusable = |e: io::Error| ProbeError::Unusable {
        path: dir.display().to_string(),
        why: e.to_string(),
        errno: e.raw_os_error(),
    };

    // `O_TMPFILE` is one "also used" feature, available on every
    // local Linux filesystem and on all three supported ones; a filesystem
    // that genuinely lacks it says so with `EOPNOTSUPP` and is reported like
    // any other missing feature, while `EROFS`/`EACCES` here still mean what
    // they meant before (this directory, right now) and stay `Unusable`.
    let mut file =
        open_probe_file(dir).map_err(|e| classify(e, dir, "temporary files (O_TMPFILE)"))?;
    file.write_all(&[1u8; 4096]).map_err(unusable)?;
    file.set_len(1 << 20).map_err(unusable)?;
    punch_all(&file).map_err(|e| classify(e, dir, "sparse files with hole punching"))?;
    xattr::FileExt::set_xattr(&file, crate::placeholder::XATTR_STATE, b"probe")
        .map_err(|e| classify(e, dir, "user.* extended attributes"))?;
    probe_lease(&file, dir)?;
    Ok(())
}

/// Exercises `F_SETLEASE` on the probe's own file.
///
/// Both dehydration (`root::punch_clean_file`) and startup recovery
/// (`root::reset_interrupted`) take a write lease before they punch a file's
/// blocks away, and `interpret_setlease_failure` treats only `EAGAIN`
/// (somebody else has the file open) as "try again later" — every other
/// refusal, including one that means "this filesystem does not grant leases
/// at all", is a hard error there. Without this probe that error surfaces
/// for the first time on the first crash-interrupted file startup recovery
/// meets (every one of them `failed`, forever) or the first dehydration a
/// user asks for, on a folder registration had already accepted. The probe's
/// own file has never been opened anywhere else, so `EAGAIN` is as
/// impossible here as `EACCES` (this process owns it) — either one reaching
/// this function is exactly the filesystem-level refusal a real dehydration
/// or recovery would meet later, reported now, against the folder the user
/// is trying to register.
fn probe_lease(file: &File, dir: &Path) -> Result<(), ProbeError> {
    match WriteLease::take(file) {
        Ok(Some(lease)) => {
            drop(lease);
            Ok(())
        }
        Ok(None) => Err(ProbeError::Unusable {
            path: dir.display().to_string(),
            why: "a write lease (F_SETLEASE) on a file nothing else has open was refused".into(),
            errno: None,
        }),
        Err(e) => Err(classify(e, dir, "write leases (F_SETLEASE)")),
    }
}

#[cfg(test)]
mod tests {
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

    /// The other half of: whatever is already sitting under the
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
}
