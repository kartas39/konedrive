//! The command line, and what it points at: the two token files and the daemon's `config.toml`.
//! Every flag is required, so a run cannot start by accident.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use konedrived::config::Config;
use url::Url;

use crate::guard::Caps;
use crate::harness::{Options, GRAPH};

/// Checks, against OneDrive itself, what konedrive's uploads assume of the service. It writes,
/// so it runs only against a test account, and refuses to start unless every guard holds: the
/// drive both tokens reach is --graph-test-drive and is in write_test_drive_ids; it has less than
/// 1 GiB in use and fewer than 1000 items; every write stays in /konedrive-write-test/<run id>/,
/// which the run makes and puts into the recycle bin at the end; at most 64 MiB per file,
/// 200 MiB and 500 requests per run. See docs/design/writes.md, "Running against the test
/// account".
#[derive(Debug, Parser)]
#[command(name = "konedrive-write-test")]
pub struct Args {
    /// The test account's drive id: GET /me/drive must answer it, and write_test_drive_ids must
    /// list it.
    #[arg(long, value_name = "ID")]
    pub graph_test_drive: String,
    /// A 0600 file holding the token `konedrivectl dev export-access-token --read-write` wrote.
    #[arg(long, value_name = "FILE")]
    pub graph_token: PathBuf,
    /// A 0600 file holding the token `konedrivectl dev export-access-token` wrote after the
    /// account was switched back to read-only.
    #[arg(long, value_name = "FILE")]
    pub graph_read_only_token: PathBuf,
    /// The daemon's config.toml, whose write_test_drive_ids must list --graph-test-drive.
    #[arg(long, value_name = "FILE")]
    pub daemon_config: PathBuf,
}

/// The run the arguments describe, or why it is refused.
pub fn options(args: &Args) -> Result<Options, String> {
    let token = read_token(&args.graph_token)?;
    let read_only_token = read_token(&args.graph_read_only_token)?;
    let config = read_config(&args.daemon_config)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    Ok(Options {
        upstream: Url::parse(GRAPH).expect("Graph's base"),
        token,
        read_only_token,
        test_drive: args.graph_test_drive.clone(),
        config,
        caps: Caps::default(),
        run_id: format!("run-{now}-{}", std::process::id()),
        delta_wait: Duration::from_secs(60),
    })
}

/// A token file as `konedrivectl dev export-access-token` writes it: a regular file, not a
/// symlink, mode `0600`, owned by the user running this, holding one token.
pub fn read_token(path: &Path) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let shown = path.display();
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{shown}: {e}"))?;
    if !meta.file_type().is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    if meta.mode() & 0o777 != 0o600 {
        return Err(format!("{shown} is mode {:03o}; a token file must be 0600", meta.mode() & 0o777));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(format!("{shown} is not the file of the user running this"));
    }
    let token = std::fs::read_to_string(path).map_err(|e| format!("{shown}: {e}"))?;
    let token = token.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return Err(format!("{shown} does not hold one token"));
    }
    Ok(token.to_owned())
}

/// The daemon's configuration, read as the daemon reads it (only for its
/// `write_test_drive_ids`).
pub fn read_config(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{} is not a config.toml the daemon reads: {e}", path.display()))
}
