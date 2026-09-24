//! Daemon configuration (`~/.config/konedrive/config.toml`) and file locations.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Locations of the daemon's files. Tests point these into a temp dir.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    pub account_cache: PathBuf,
    /// The tree store.
    pub tree_db: PathBuf,
    /// Where files are rescued to.
    pub rescue_dir: PathBuf,
    /// The freedesktop thumbnail cache.
    pub thumbnails: PathBuf,
}

impl Paths {
    /// Standard XDG locations for the current user.
    pub fn from_xdg() -> anyhow::Result<Self> {
        let config = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no XDG config directory"))?;
        let state = dirs::state_dir().ok_or_else(|| anyhow::anyhow!("no XDG state directory"))?;
        let data = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("no XDG data directory"))?;
        let cache = dirs::cache_dir().ok_or_else(|| anyhow::anyhow!("no XDG cache directory"))?;
        Ok(Self {
            config_file: config.join("konedrive").join("config.toml"),
            account_cache: state.join("konedrive").join("account.json"),
            tree_db: state.join("konedrive").join("tree.sqlite"),
            rescue_dir: data.join("konedrive").join("rescued"),
            thumbnails: cache.join("thumbnails"),
        })
    }

    /// Every file directly under `dir`.
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            config_file: dir.join("config.toml"),
            account_cache: dir.join("account.json"),
            tree_db: dir.join("tree.sqlite"),
            rescue_dir: dir.join("rescued"),
            thumbnails: dir.join("thumbnails"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub client_id: String,
    /// The registered sync root, empty when none. It has to be
    /// "persisted, so it survives a restart" — and
    /// without it the startup recovery walk never runs at a startup at
    /// all, since nothing else re-registers the folder.
    #[serde(default)]
    pub sync_root: String,
    /// Whether that root is the ordinary intercepted kind. Defaults to
    /// `true` when the key is missing, so the fail-closed mode is what an
    /// older or hand-edited config restores: a root wrongly restored as
    /// intercepted refuses to come up without a helper, while one wrongly
    /// restored as un-intercepted would come up silently serving zeros.
    #[serde(default = "intercepted_by_default")]
    pub sync_root_intercepted: bool,
    /// The root id the folder carried when it was registered — the name the
    /// helper holds an intercepted root under. Recorded so that a root
    /// restored at startup can be held, forgotten and told apart before the
    /// helper is back, without reading anything from the folder. Empty in a
    /// config written before it existed; the folder's own
    /// `user.konedrive.root` stands in for it then.
    #[serde(default)]
    pub sync_root_id: String,
    /// What the folder shows: `"onedrive"` — listed from the
    /// signed-in drive, locked, kept in step — or `"local"`, filled with
    /// `PopulateFromDirectory` as in part 1. A config written before this
    /// existed describes a local folder.
    #[serde(default = "local_source")]
    pub sync_root_source: String,
    /// Whether *this daemon* excluded the root from KDE's Baloo indexer
    /// — `false` when the folder was already excluded (the
    /// user's own doing, or a parent directory's), since a Forget must never
    /// take off an exclusion it did not add. Defaults to `false`, the safe
    /// side for a config written before this field existed: nothing is
    /// removed from Baloo's settings that this daemon cannot be sure it put
    /// there.
    #[serde(default)]
    pub sync_root_baloo_excluded: bool,
    /// Whether a root registered without interception switches to
    /// interception when the helper connects: `true` when it was
    /// registered that way because no helper was connected, `false` when a
    /// helper was and the mode was a choice. Missing in a config written
    /// before this existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_root_upgrade_when_helper: Option<bool>,
    /// The drive a OneDrive folder was listed from, recorded
    /// when its sync first learns it, so the check that the account signed
    /// in is still that drive's survives a tree store rebuilt empty.
    /// Empty until then, for a local folder, and in a
    /// config written before it existed. It goes with `sync_root_id`: a
    /// different root never inherits it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sync_root_drive_id: String,
}

fn intercepted_by_default() -> bool {
    true
}

fn local_source() -> String {
    "local".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            sync_root: String::new(),
            sync_root_intercepted: true,
            sync_root_id: String::new(),
            sync_root_source: local_source(),
            sync_root_baloo_excluded: false,
            sync_root_upgrade_when_helper: None,
            sync_root_drive_id: String::new(),
        }
    }
}

impl Config {
    /// A missing file yields the default configuration.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_atomic(path, toml::to_string(self)?.as_bytes())
    }

    /// `sync_root_upgrade_when_helper`, with a config written before it
    /// existed read as Ruling 4 says: a root without interception
    /// switches — such a config cannot tell a folder registered that way on
    /// purpose from one registered before the helper was installed, and the
    /// second reads as zeros until it switches — and an intercepted root has
    /// nothing to switch.
    pub fn sync_root_upgrades_when_helper(&self) -> bool {
        self.sync_root_upgrade_when_helper.unwrap_or(!self.sync_root_intercepted)
    }
}

/// Writes `data` to `path` through a temp file in the same directory and a rename.
///
/// The temp file is `fsync`ed before the rename, and the directory after it
///: without the first, a crash soon after could
/// leave `path` naming an empty or partial file — `config.toml` holds the
/// registered folder, and one lost is a folder the next start never
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

/// Accepts the canonical GUID form `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (hex digits, any case).
pub fn is_valid_client_id(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_guids_in_any_case() {
        assert!(is_valid_client_id("0f8fad5b-d9cb-469f-a165-70867728950e"));
        assert!(is_valid_client_id("0F8FAD5B-D9CB-469F-A165-70867728950E"));
    }

    #[test]
    fn rejects_malformed_ids() {
        for bad in [
            "",
            "not-a-guid",
            "0f8fad5b-d9cb-469f-a165-70867728950",
            "0f8fad5bd9cb469fa16570867728950e",
            "0f8fad5b-d9cb-469f-a165-70867728950g",
            "0f8fad5b-d9cb-469f-a165-70867728950e-1",
        ] {
            assert!(!is_valid_client_id(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn missing_file_loads_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("config.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn save_creates_directories_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let config = Config {
            client_id: "0f8fad5b-d9cb-469f-a165-70867728950e".into(),
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }

    /// The sync root travels with the client id in the same file, so both
    /// have to survive a round trip — and a config written before the sync
    /// sub-project existed has to keep loading, defaulting to the
    /// fail-closed intercepted mode rather than to the mode that serves
    /// zeros.
    #[test]
    fn the_sync_root_round_trips_and_an_older_config_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config {
            client_id: "0f8fad5b-d9cb-469f-a165-70867728950e".into(),
            sync_root: "/home/someone/OneDrive".into(),
            sync_root_intercepted: false,
            sync_root_id: "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d".into(),
            sync_root_source: "onedrive".into(),
            sync_root_baloo_excluded: true,
            sync_root_upgrade_when_helper: Some(true),
            sync_root_drive_id: "D1".into(),
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);

        std::fs::write(&path, "client_id = \"0f8fad5b-d9cb-469f-a165-70867728950e\"\n").unwrap();
        let older = Config::load(&path).unwrap();
        assert_eq!(older.sync_root, "", "no root was persisted by that version");
        assert_eq!(older.sync_root_id, "", "nor its id");
        assert!(
            older.sync_root_intercepted,
            "a missing mode must read as the fail-closed one, not as the one that serves zeros"
        );
        assert!(
            !older.sync_root_baloo_excluded,
            "a config from before this existed must not have Baloo settings taken off it"
        );

        // A folder recorded by part 1, before a folder could show OneDrive,
        // was filled from a directory: it is a local one.
        std::fs::write(&path, "sync_root = \"/home/someone/Offline\"\nsync_root_intercepted = false\n").unwrap();
        assert_eq!(Config::load(&path).unwrap().sync_root_source, "local");
        assert_eq!(Config::default().sync_root_source, "local");
    }

    /// Ruling 4: a config written before the switch flag existed
    /// cannot say why its root is without interception, and is read as one
    /// to switch when the helper connects — the user's own folder is that
    /// case. An intercepted root has nothing to switch. A written flag is
    /// what it says.
    #[test]
    fn a_missing_switch_flag_reads_as_switch_only_for_a_root_without_interception() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        std::fs::write(&path, "sync_root = \"/home/someone/tools/test\"\nsync_root_intercepted = false\n").unwrap();
        let unintercepted = Config::load(&path).unwrap();
        assert_eq!(unintercepted.sync_root_upgrade_when_helper, None);
        assert!(unintercepted.sync_root_upgrades_when_helper());

        std::fs::write(&path, "sync_root = \"/home/someone/OneDrive\"\n").unwrap();
        assert!(!Config::load(&path).unwrap().sync_root_upgrades_when_helper());

        let written = Config { sync_root_intercepted: false, sync_root_upgrade_when_helper: Some(false), ..Config::default() };
        written.save(&path).unwrap();
        assert!(!Config::load(&path).unwrap().sync_root_upgrades_when_helper(), "a choice is kept");
        assert!(Config::default().save(&path).is_ok());
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("sync_root_upgrade_when_helper"),
            "no root, no flag written"
        );
    }

    #[test]
    fn in_dir_places_every_file_in_the_directory() {
        let paths = Paths::in_dir(Path::new("/tmp/x"));
        assert_eq!(paths.config_file, Path::new("/tmp/x/config.toml"));
        assert_eq!(paths.account_cache, Path::new("/tmp/x/account.json"));
        assert_eq!(paths.tree_db, Path::new("/tmp/x/tree.sqlite"));
        assert_eq!(paths.rescue_dir, Path::new("/tmp/x/rescued"));
        assert_eq!(paths.thumbnails, Path::new("/tmp/x/thumbnails"));
    }
}
