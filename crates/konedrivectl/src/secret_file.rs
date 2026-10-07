//! A secret written to a file of the user's choosing: `dev export-access-token`'s.

/// Writes `data` to `path` as a brand-new file, atomically and privately.
///
/// A temporary file is created in the same directory as `path`
/// (`O_CREAT | O_EXCL | O_NOFOLLOW`, mode 0600 from the instant it exists —
/// so the name can only be *this* call's own new file, never an existing
/// one and never a symlink), written, `fsync`ed, then renamed over `path`.
/// `rename(2)` replaces whatever `path` names — a symlink there included —
/// by swapping the directory entry to the new inode, rather than writing
/// through whatever `path` used to point to; so a symlink at `path` is
/// *replaced*, never followed and never written through, and anyone who
/// already had the old `path` open keeps reading the old inode's content,
/// completely untouched, for as long as they hold it open. The temporary
/// file is removed on any failure along the way, so a half-written one is
/// never left where an unrelated later read could find it.
///
/// This is what `dev export-access-token` uses to write the token: the
/// symlink case matters because `--out` names a path the person running the
/// command chose, which could already be a symlink (by accident, or by
/// something else's doing) to a file they did not mean to touch.
pub fn write_secret_atomically(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name in the given path")
    })?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp_path = dir.join(&tmp_name);

    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // O_CREAT | O_EXCL: this name is ours alone.
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&tmp_path)
        .and_then(|mut file| {
            file.write_all(data)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp_path, path));

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

#[cfg(test)]
mod tests;
