//! Daemon configuration (`~/.config/konedrive/config.toml`) and file locations.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Locations of the daemon's files. Tests point these into a temp dir.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    pub account_cache: PathBuf,
}

impl Paths {
    /// Standard XDG locations for the current user.
    pub fn from_xdg() -> anyhow::Result<Self> {
        let config = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no XDG config directory"))?;
        let state = dirs::state_dir().ok_or_else(|| anyhow::anyhow!("no XDG state directory"))?;
        Ok(Self {
            config_file: config.join("konedrive").join("config.toml"),
            account_cache: state.join("konedrive").join("account.json"),
        })
    }

    /// Both files directly under `dir`.
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            config_file: dir.join("config.toml"),
            account_cache: dir.join("account.json"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub client_id: String,
    /// The registered sync root, empty when none. Spec §3.1 requires the
    /// registration to be "persisted, so it survives a restart" — and
    /// without it §4.4's startup recovery walk never runs at a startup at
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
}

fn intercepted_by_default() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            sync_root: String::new(),
            sync_root_intercepted: true,
            sync_root_id: String::new(),
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
}

/// Writes `data` to `path` through a temp file in the same directory and a rename.
pub fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    // Both files this is used for (config.toml, account.json) can hold data that is not
    // meant for other local users: config.toml nothing sensitive today, but account.json
    // holds the signed-in name and email. Restrict before the rename, not after, so the
    // file is never briefly world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
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
    }

    #[test]
    fn in_dir_places_both_files_in_the_directory() {
        let paths = Paths::in_dir(Path::new("/tmp/x"));
        assert_eq!(paths.config_file, Path::new("/tmp/x/config.toml"));
        assert_eq!(paths.account_cache, Path::new("/tmp/x/account.json"));
    }
}
