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
    /// merely not allowed to write here (`EROFS`, `EACCES`).
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
mod tests;
