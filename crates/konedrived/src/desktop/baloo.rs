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
//! persisted in `config.toml` (`baloo_excluded` of the folder's entry,
//! `crate::sync::persisted`) so a daemon restart in between still
//! gets it right. Every bring-up of a folder that does not record it as
//! excluded asks again: an exclusion that failed, timed out or was
//! cut off by a kill, or a Baloo installed later, is caught up then. The
//! cost, in the log: no Baloo file-name search inside the folder, so KRunner
//! and Dolphin's own search do not find files there.
//!
//! A folder's wiring (`sync::Wiring::new`) starts with [`Baloo::disabled`]
//! (no program, no settings file), which runs and reads nothing at all — a
//! test that says nothing of Baloo must never reach the real
//! `~/.config/baloofilerc`. Only `main` gives [`Baloo::default`]
//! (`balooctl6`, and the user's own `baloofilerc`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a `balooctl6` call is allowed to run before it is treated as
/// unavailable (real Baloo answers in milliseconds; this is only a backstop
/// against a hung one — e.g. blocked on an unresponsive Baloo D-Bus service).
/// Every call runs with `SyncService`'s `folder` lock held for writing, so a
/// call that never returned would block every registration and every Forget
/// forever.
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
    /// Runs nothing and reads nothing. What a folder's wiring has until
    /// production's `main`, or a test's fake, says another.
    pub fn disabled() -> Self {
        Self { program: None, settings: None, timeout: DEFAULT_TIMEOUT }
    }

    /// Whether `folder`, or a directory above it, is already excluded — read
    /// from Baloo's settings file, never written. A missing or
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
mod tests;
