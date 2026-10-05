//! The testable part of `konedrivectl`: what is decided and what is said, with nothing read
//! from the daemon and nothing printed. The program (`main.rs`, `commands/`, `daemon.rs`,
//! `read.rs`, `wait.rs`) reads the daemon into the types here and prints what they say.
//!
//! - [`choice`]: the account a command acts on;
//! - [`text`]: what is printed, by topic;
//! - [`secret_file`]: a secret written to a file.
//!
//! The daemon serves one object per account (`konedrive_dbus::account_path`) below the
//! accounts manager (`konedrive_dbus::ACCOUNTS_PATH`). A command acts on one account, chosen
//! as [`choice::choose`] says; `status` and `sync status` show every account when none is
//! chosen; the commands that take a path go through `Files`, which finds the account by the
//! path.

pub mod choice;
pub mod secret_file;
pub mod text;

/// The label `konedrivectl login` gives the account it adds when there is none, as the
/// daemon names the account it migrates from a single-account configuration.
pub const FIRST_LABEL: &str = "Personal";

/// The environment variable that chooses the account when `--account` is not given.
pub const ACCOUNT_VARIABLE: &str = "KONEDRIVE_ACCOUNT";

/// The environment variable that, set to anything but empty, keeps the sign-in page from
/// being opened in a browser: a sign-in over SSH, with no desktop, or in tests.
pub const NO_BROWSER_VARIABLE: &str = "KONEDRIVE_NO_BROWSER";

/// Whether a command opens the sign-in page in the browser: only when
/// [`NO_BROWSER_VARIABLE`] is unset or empty (`no_browser`, its value) and stdout is a
/// terminal, so that nobody's desktop gets a browser tab nobody looks at. The address is
/// printed either way.
pub fn opens_browser(no_browser: Option<&std::ffi::OsStr>, terminal: bool) -> bool {
    no_browser.is_none_or(|v| v.is_empty()) && terminal
}

#[cfg(test)]
mod tests;
