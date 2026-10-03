use std::process::{Command, Stdio};

/// Whether the sign-in page is opened in the browser here ([`konedrivectl::opens_browser`]).
pub(crate) fn open_browser() -> bool {
    use std::io::IsTerminal;
    konedrivectl::opens_browser(std::env::var_os(konedrivectl::NO_BROWSER_VARIABLE).as_deref(), std::io::stdout().is_terminal())
}

/// Opens `url` with xdg-open, without waiting for it.
pub(crate) fn spawn_browser(url: &str) {
    let _ = Command::new("xdg-open").arg(url).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}
