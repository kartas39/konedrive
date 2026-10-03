//! The testable part of `konedrivectl`.
//!
//! The daemon serves one object per account (`konedrive_dbus::account_path`) below the
//! accounts manager (`konedrive_dbus::ACCOUNTS_PATH`). A command acts on one account, chosen
//! as [`choose`] says; `status` and `sync status` show every account when none is chosen; the
//! commands that take a path go through `Files`, which finds the account by the path.

use std::time::Duration;

use konedrive_dbus::accounts::AccountProxy;

mod choice;
mod text;

pub use choice::*;
pub use text::*;

/// The label `konedrivectl login` gives the account it adds when there is none, as the
/// daemon names the account it migrates from a single-account configuration.
pub const FIRST_LABEL: &str = "Personal";

/// The environment variable that chooses the account when `--account` is not given.
pub const ACCOUNT_VARIABLE: &str = "KONEDRIVE_ACCOUNT";

/// The environment variable that, set to anything but empty, keeps the sign-in page from
/// being opened in a browser (issue #21): a sign-in over SSH, with no desktop, or in tests.
pub const NO_BROWSER_VARIABLE: &str = "KONEDRIVE_NO_BROWSER";

/// Whether a command opens the sign-in page in the browser: only when
/// [`NO_BROWSER_VARIABLE`] is unset or empty (`no_browser`, its value) and stdout is a
/// terminal, so that nobody's desktop gets a browser tab nobody looks at. The address is
/// printed either way.
pub fn opens_browser(no_browser: Option<&std::ffi::OsStr>, terminal: bool) -> bool {
    no_browser.is_none_or(|v| v.is_empty()) && terminal
}

/// Whether `error` says that the object called is not there: an account removed while the
/// command ran.
pub fn is_gone(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => is_gone_name(name.as_str()),
        zbus::Error::FDO(error) => {
            use zbus::DBusError;
            is_gone_name(error.name().as_str())
        }
        _ => false,
    }
}

/// Whether `name` is the bus's answer for an object, interface or method that
/// is not there ([`is_gone`]).
pub(crate) fn is_gone_name(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.DBus.Error.UnknownObject"
            | "org.freedesktop.DBus.Error.UnknownMethod"
            | "org.freedesktop.DBus.Error.UnknownInterface"
    )
}

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

/// Polls `proxy` until `SetMode("read-write")`'s sign-in has ended: `Ok` once `Mode` is
/// `read-write`, `Err` with `LastError` when the switch did not go through. The account stays
/// `signed-in` throughout, so `State` cannot tell; it is polled for the same reason
/// [`wait_for_sign_in`] polls.
pub async fn wait_for_read_write(proxy: &AccountProxy<'_>) -> anyhow::Result<()> {
    loop {
        if proxy.mode().await? == "read-write" {
            return Ok(());
        }
        // `SetMode` clears `LastError` before it answers, so anything in it now is why the
        // switch did not go through. Cancelled, it says nothing: the caller stops waiting.
        let last_error = proxy.last_error().await?;
        if !last_error.is_empty() {
            anyhow::bail!("the account stays read-only: {last_error}");
        }
        if proxy.state().await? != "signed-in" {
            anyhow::bail!("the account was signed out; it stays read-only");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Polls `proxy` until the account leaves the `signing-in` state, then reports the outcome.
///
/// Polling (rather than watching the `StateChanged` signal) sidesteps a coalescing hazard:
/// the daemon publishes state through a `tokio::watch` channel, so a fast transition (e.g.
/// `signing-in` -> `signed-in` completing almost immediately, as with an SSO session) can be
/// collapsed and observed only as its final value. A waiter that only starts counting once it
/// has *seen* `signing-in` on the signal stream could then wait forever for a change that
/// already happened. `BeginSignIn` sets the state to `signing-in` before it replies, so the
/// very first poll here is guaranteed to observe either `signing-in` or the terminal state -
/// there is no window in which the relevant transition can be missed.
pub async fn wait_for_sign_in(proxy: &AccountProxy<'_>) -> anyhow::Result<()> {
    loop {
        let state = proxy.state().await?;
        if state != "signing-in" {
            return if state == "signed-in" {
                Ok(())
            } else {
                let last_error = proxy.last_error().await?;
                if last_error.is_empty() {
                    anyhow::bail!("sign-in was cancelled");
                } else {
                    anyhow::bail!("sign-in did not complete: {last_error}");
                }
            };
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests;
