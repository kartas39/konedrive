//! One file replaced in one step.

use std::path::Path;

/// Writes `data` to `path` through a temp file in the same directory and a rename.
///
/// The temp file is `fsync`ed before the rename, and the directory after it: without the
/// first, a crash soon after could leave `path` naming an empty or partial file —
/// `config.toml` holds the registered folder, and one lost is a folder the next start never
/// recovers; without the second, the rename itself could be lost.
pub fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    // Both files this is used for (config.toml, account.json) can hold data that is not
    // meant for other local users: config.toml nothing sensitive today, but account.json
    // holds the signed-in name and email. Private from creation — and made so again, for a
    // temp file a crash left behind — so the file is never briefly world-readable.
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}
