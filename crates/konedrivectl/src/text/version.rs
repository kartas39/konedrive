//! `--version`.

/// The running daemon's build, as `konedrivectl --version` found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonBuild {
    /// No daemon on the session bus (or no session bus): nothing was started to ask.
    NotRunning(String),
    /// A daemon that did not say: a build older than the `Version` property, most likely.
    Unknown(String),
    Running { version: String, commit: String },
}

/// How to restart the daemon, so that it runs the build installed.
pub const RESTART_HINT: &str = "systemctl --user restart konedrived";

/// What `konedrivectl --version` prints: its own line, the daemon's, and, when the daemon runs
/// another build than this one (another version or another commit), a line that says to restart
/// it.
pub fn version_text(version: &str, commit: &str, daemon: &DaemonBuild) -> String {
    use konedrive_dbus::version::line;
    let mut out = line("konedrivectl", version, commit) + "\n";
    let other = match daemon {
        DaemonBuild::NotRunning(why) => {
            out.push_str(&format!("konedrived: not running ({why})\n"));
            false
        }
        DaemonBuild::Unknown(why) => {
            out.push_str(&format!("konedrived: running, version unknown ({why})\n"));
            true
        }
        DaemonBuild::Running { version: daemon_version, commit: daemon_commit } => {
            out.push_str(&line("konedrived", daemon_version, daemon_commit));
            out.push('\n');
            daemon_version != version || daemon_commit != commit
        }
    };
    if other {
        out.push_str(&format!(
            "The daemon runs another build than this one: restart it ({RESTART_HINT}) to use this one.\n"
        ));
    }
    out
}
