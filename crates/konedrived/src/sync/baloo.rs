//! Keeps KDE's file indexer out of a OneDrive folder: Baloo
//! reads every file's content to index it, and a read of a placeholder
//! downloads it — the whole drive, in the background, the moment Baloo
//! walks it.
//!
//! A OneDrive folder is excluded (`config add excludeFolders`) only after
//! checking, read-only, that it is not excluded already: a user's own
//! exclusion, or one on a parent directory, is never touched. That check
//! reads Baloo's own settings file, `baloofilerc`: `balooctl6 config list
//! excludeFolders` was observed printing an empty list while the file held
//! an exclusion, so a registration re-added the user's own exclusion and a
//! Forget would have taken it off. Adding and removing still go through `balooctl6`.
//!
//! Whether *this* daemon is the one that added the exclusion is what decides
//! whether Forget takes it off again (`config rm excludeFolders`) —
//! persisted in `config.toml` (`sync_root_baloo_excluded`,
//! [`SyncService`](super::SyncService)) so a daemon restart in between still
//! gets it right. Every bring-up of a folder that does not record it as
//! excluded asks again (B-I1): an exclusion that failed, timed out or was
//! cut off by a kill, or a Baloo installed later, is caught up then. The
//! cost, in the log: no Baloo file-name search inside the folder, so KRunner
//! and Dolphin's own search do not find files there.
//!
//! [`SyncService`](super::SyncService) starts with [`Baloo::disabled`]
//! (no program, no settings file), which runs and reads nothing at all — a
//! test that forgets to call `set_baloo` must never reach the real
//! `~/.config/baloofilerc`. Only `main` installs [`Baloo::default`]
//! (`balooctl6`, and the user's own `baloofilerc`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a `balooctl6` call is allowed to run before it is treated as
/// unavailable (real Baloo answers in milliseconds; this is only a backstop
/// against a hung one — e.g. blocked on an unresponsive Baloo D-Bus service).
/// Every call runs under `SyncService`'s lifecycle write lock, so a call that
/// never returned would block every registration and every Forget forever.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Baloo {
    /// `None` runs nothing (see the module doc). `Some` is the program to
    /// run — `balooctl6` normally, a fake shell script in tests.
    pub program: Option<PathBuf>,
    /// Baloo's settings file, read to learn what is excluded already. `None`
    /// reads nothing, and nothing counts as excluded.
    pub settings: Option<PathBuf>,
    /// How long one call is allowed to run — [`DEFAULT_TIMEOUT`] normally;
    /// settable in tests, so a hang can be exercised in well under a second.
    pub timeout: Duration,
}

impl Default for Baloo {
    fn default() -> Self {
        let settings = settings_file(std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("HOME"));
        Self { program: Some(PathBuf::from("balooctl6")), settings, timeout: DEFAULT_TIMEOUT }
    }
}

/// `${XDG_CONFIG_HOME:-$HOME/.config}/baloofilerc`, where KConfig keeps
/// Baloo's settings; an `XDG_CONFIG_HOME` that is empty or relative is
/// ignored, as the XDG spec says.
pub fn settings_file(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let config = match xdg_config_home.map(PathBuf::from).filter(|dir| dir.is_absolute()) {
        Some(dir) => dir,
        None => PathBuf::from(home.filter(|home| !home.is_empty())?).join(".config"),
    };
    Some(config.join("baloofilerc"))
}

impl Baloo {
    /// Runs nothing and reads nothing. What every
    /// [`SyncService`](super::SyncService) starts with, until `set_baloo` —
    /// production's `main`, or a test's fake — replaces it.
    pub fn disabled() -> Self {
        Self { program: None, settings: None, timeout: DEFAULT_TIMEOUT }
    }

    /// Whether `folder`, or a directory above it, is already excluded — read
    /// from Baloo's settings file (B-I1b), never written. A missing or
    /// unreadable file, or a disabled `Baloo`, answers "not excluded":
    /// nothing is assumed about settings that cannot be read, so the caller
    /// falls through to trying `exclude` — which is exactly as harmless to
    /// ask twice.
    pub async fn is_excluded(&self, folder: &Path) -> bool {
        let Some(settings) = &self.settings else { return false };
        let text = match tokio::fs::read_to_string(settings).await {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(e) => {
                tracing::warn!("cannot read Baloo's settings in {}: {e}", settings.display());
                return false;
            }
        };
        let variable = |name: &str| std::env::var(name).ok();
        excluded_folders(&text, &variable).iter().any(|excluded| folder.starts_with(excluded))
    }

    /// Adds the exclusion. Returns whether it is now known to be in effect
    /// because of this call — `false` for a missing, failing or hung
    /// program, and for a disabled `Baloo` — which is what the caller
    /// persists to decide whether a later Forget should take it off again.
    pub async fn exclude(&self, folder: &Path) -> bool {
        self.run(&["config", "add", "excludeFolders"], folder).await
    }

    /// Takes the exclusion back off. Best-effort like `exclude`: search
    /// settings are not worth anything failing over, including a Forget.
    pub async fn include_again(&self, folder: &Path) {
        self.run(&["config", "rm", "excludeFolders"], folder).await;
    }

    async fn run(&self, args: &[&str], folder: &Path) -> bool {
        let Some(program) = &self.program else { return false };
        let mut command = tokio::process::Command::new(program);
        command.args(args).arg(folder);
        let description = format!("{} {}", args.join(" "), folder.display());
        let Ok(output) = self.output(command, &description).await else { return false };
        if output.status.success() {
            tracing::info!("Baloo: {description}");
            true
        } else {
            tracing::warn!("Baloo refused `{description}`: {}", String::from_utf8_lossy(&output.stderr).trim());
            false
        }
    }

    /// Runs `command` under `self.timeout`, `kill_on_drop` set so a timed-out
    /// child is killed rather than left running. A missing program, one that
    /// failed to spawn, and one that did not answer in time are all logged
    /// once and treated alike by the caller: `Err(())` here, "carry on"
    /// there — search settings are never worth failing a registration or a
    /// Forget over.
    async fn output(
        &self,
        mut command: tokio::process::Command,
        description: &str,
    ) -> Result<std::process::Output, ()> {
        command.kill_on_drop(true);
        match tokio::time::timeout(self.timeout, command.output()).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(e)) => {
                tracing::info!("Baloo is not available ({e}); nothing to keep out of it");
                Err(())
            }
            Err(_) => {
                tracing::warn!(
                    "Baloo did not answer `{description}` within {:?}; treating it as unavailable",
                    self.timeout
                );
                Err(())
            }
        }
    }
}

/// Every folder a `baloofilerc` excludes: the `[General]` group's `exclude
/// folders` key — with KConfig's `[$e]` marker, as `balooctl6` writes it, or
/// without — as a comma-separated list, with KConfig's escapes undone,
/// `$NAME` and `${NAME}` expanded through `variable` (an unset one is
/// empty, as KConfig has it), and trailing slashes dropped. Only absolute
/// paths are kept; nothing else can hold a folder.
fn excluded_folders(text: &str, variable: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut general = false;
    let mut found = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            general = line == "[General]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let name = key.split('[').next().unwrap_or_default().trim();
        if !general || name != "exclude folders" {
            continue;
        }
        for item in list_items(value) {
            let expanded = expand(&item, variable);
            let path = expanded.trim_end_matches('/');
            if expanded.starts_with('/') {
                found.push(PathBuf::from(if path.is_empty() { "/" } else { path }));
            }
        }
    }
    found
}

/// A KConfig list value's items: split at every comma not escaped as `\,`,
/// with `\s`, `\t`, `\n`, `\r` and `\\` undone.
fn list_items(value: &str) -> Vec<String> {
    let mut items = vec![String::new()];
    let mut chars = value.trim().chars();
    while let Some(c) = chars.next() {
        let item = items.last_mut().expect("never empty");
        match c {
            ',' => items.push(String::new()),
            '\\' => item.push(match chars.next() {
                Some('s') => ' ',
                Some('t') => '\t',
                Some('n') => '\n',
                Some('r') => '\r',
                Some(other) => other,
                None => '\\',
            }),
            other => item.push(other),
        }
    }
    items.retain(|item| !item.is_empty());
    items
}

/// `$NAME`, `${NAME}` and `$$` in a KConfig value marked `[$e]`. A command
/// (`$(...)`) is never run: it stays as written, and so matches no folder.
fn expand(item: &str, variable: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = item;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(after) = after.strip_prefix('$') {
            out.push('$');
            rest = after;
        } else if let Some((name, after)) = after.strip_prefix('{').and_then(|braced| braced.split_once('}')) {
            out.push_str(&variable(name).unwrap_or_default());
            rest = after;
        } else {
            let len = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
            if len == 0 {
                out.push('$');
            } else {
                out.push_str(&variable(&after[..len]).unwrap_or_default());
            }
            rest = &after[len..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn fake(dir: &Path) -> (Baloo, PathBuf) {
        let log = dir.join("calls");
        let script = dir.join("balooctl6");
        std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> '{}'\n", log.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (Baloo { program: Some(script), settings: None, timeout: DEFAULT_TIMEOUT }, log)
    }

    fn home(name: &str) -> Option<String> {
        (name == "HOME").then(|| "/home/u".to_owned())
    }

    #[tokio::test]
    async fn the_folder_is_excluded_and_included_again() {
        let dir = tempfile::tempdir().unwrap();
        let (baloo, log) = fake(dir.path());
        assert!(baloo.exclude(Path::new("/home/u/OneDrive")).await);
        baloo.include_again(Path::new("/home/u/OneDrive")).await;
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "config add excludeFolders /home/u/OneDrive\nconfig rm excludeFolders /home/u/OneDrive\n"
        );
    }

    #[tokio::test]
    async fn a_missing_program_is_not_an_error() {
        let baloo = Baloo { program: Some(PathBuf::from("/nonexistent/balooctl6")), settings: None, timeout: DEFAULT_TIMEOUT };
        assert!(!baloo.exclude(Path::new("/home/u/OneDrive")).await);
        baloo.include_again(Path::new("/home/u/OneDrive")).await;
        assert!(!baloo.is_excluded(Path::new("/home/u/OneDrive")).await);
    }

    /// A fake `balooctl6` that hangs (blocked on an unresponsive Baloo
    /// D-Bus service, say) must not block registration or Forget forever:
    /// `timeout` cuts it off, `kill_on_drop` makes sure the child does not
    /// linger, and a timeout is treated exactly like a missing program.
    #[tokio::test]
    async fn a_hung_program_is_killed_and_treated_as_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("balooctl6");
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let baloo = Baloo { program: Some(script.clone()), settings: None, timeout: Duration::from_millis(100) };

        let start = std::time::Instant::now();
        assert!(!baloo.exclude(Path::new("/home/u/OneDrive")).await);
        assert!(!baloo.is_excluded(Path::new("/home/u/OneDrive")).await);
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}, the 5s sleep was not cut off");

        // `kill_on_drop` must have actually killed the child, not just given
        // up waiting for it: nothing matching the fake script is left
        // running (give the kernel a moment to reap it).
        for _ in 0..20 {
            let still_running = std::process::Command::new("pgrep")
                .args(["-f", &script.display().to_string()])
                .output()
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(false);
            if !still_running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the hung balooctl6 was not killed");
    }

    #[tokio::test]
    async fn a_disabled_baloo_runs_no_program_at_all() {
        let dir = tempfile::tempdir().unwrap();
        // Points at a script that would prove it ran, if it ran.
        let (_unused, log) = fake(dir.path());
        let baloo = Baloo::disabled();
        assert!(!baloo.exclude(Path::new("/home/u/OneDrive")).await);
        assert!(!baloo.is_excluded(Path::new("/home/u/OneDrive")).await);
        baloo.include_again(Path::new("/home/u/OneDrive")).await;
        assert!(!log.exists(), "no program ran");
    }

    /// B-I1b: the exclusions come from `baloofilerc` itself — the user's own
    /// line exactly as found on their machine, where `balooctl6 config list
    /// excludeFolders` printed an empty list — in every form KConfig writes.
    #[test]
    fn the_exclusions_are_read_from_baloofilerc_as_kconfig_writes_them() {
        let text = "[Basic Settings]\nexclude folders=/not/general\n\
                    [General]\nexclude folders[$e]=$HOME/tools/test/\nfolders[$e]=$HOME/\n";
        assert_eq!(excluded_folders(text, &home), [PathBuf::from("/home/u/tools/test")]);

        let text = "[General]\nexclude folders[$e]=${HOME}/a/,/srv/b\\,c/,relative/$UNSET,$$HOME/x\n\
                    [General][Nested]\nexclude folders=/nested\n";
        assert_eq!(excluded_folders(text, &home), [PathBuf::from("/home/u/a"), PathBuf::from("/srv/b,c")]);
        assert_eq!(excluded_folders("[General]\nexclude folders=/plain/\n", &home), [PathBuf::from("/plain")]);
    }

    /// Read from the file a `Baloo` names — never `~/.config` in a test —
    /// with a folder counted as excluded only inside an excluded directory,
    /// not beside one that shares its first letters.
    #[tokio::test]
    async fn is_excluded_reads_the_settings_file_and_respects_path_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("baloofilerc");
        let baloo = Baloo { program: None, settings: Some(settings.clone()), timeout: DEFAULT_TIMEOUT };
        assert!(!baloo.is_excluded(Path::new("/srv/u/OneDrive")).await, "no file, nothing excluded");

        std::fs::write(&settings, "[General]\nexclude folders[$e]=/srv/u/,/srv/other\n").unwrap();
        assert!(baloo.is_excluded(Path::new("/srv/u/OneDrive")).await, "inside an excluded directory");
        assert!(baloo.is_excluded(Path::new("/srv/u")).await);
        assert!(!baloo.is_excluded(Path::new("/srv/uu/OneDrive")).await, "a shared prefix is not inside");
    }

    #[test]
    fn the_settings_file_is_under_xdg_config_home_or_home() {
        let file = |xdg: Option<&str>, home: Option<&str>| settings_file(xdg.map(Into::into), home.map(Into::into));
        assert_eq!(file(Some("/x/cfg"), Some("/home/u")), Some(PathBuf::from("/x/cfg/baloofilerc")));
        assert_eq!(file(Some("relative"), Some("/home/u")), Some(PathBuf::from("/home/u/.config/baloofilerc")));
        assert_eq!(file(None, Some("/home/u")), Some(PathBuf::from("/home/u/.config/baloofilerc")));
        assert_eq!(file(None, None), None);
    }
}
