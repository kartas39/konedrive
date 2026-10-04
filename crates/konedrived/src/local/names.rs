//! The name pre-check (`docs/design/writes.md` §4.4): names OneDrive refuses, found
//! before a request is spent on them.
//!
//! A refused name is `blocked` and listed for the user to rename, as Windows
//! does; nothing is substituted. The service decides in the end — whatever
//! this misses is refused with `400` and blocked with the service's message
//! — so the check stays conservative: it blocks only what Microsoft's page
//! names, never a guess.

use std::ffi::OsStr;

use konedrive_tree::outbox::Reason;

/// The largest file OneDrive takes (Microsoft: 250 GB). Taken as GiB: a file
/// between 250 GB and 250 GiB, if refused, is blocked by the service's answer.
pub const MAX_FILE_SIZE: u64 = 250 << 30;

/// Why a row is blocked before any request. Its [`Reason`] is what the row
/// holds; the window and the CLI word it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// One of `" * : < > ? \ |`.
    Characters,
    /// A leading or trailing space.
    Spaces,
    /// `.lock`, `CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`,
    /// `desktop.ini`, `_vti_` anywhere, a `~$` prefix.
    Reserved,
    /// Linux allows it; JSON cannot carry it.
    NotUtf8,
    /// Over [`MAX_FILE_SIZE`].
    TooLarge,
}

impl Refused {
    /// The reason a row blocked for it holds.
    pub fn reason(self) -> Reason {
        match self {
            Self::Characters => Reason::NameCharacters,
            Self::Spaces => Reason::NameSpaces,
            Self::Reserved => Reason::NameReserved,
            Self::NotUtf8 => Reason::NameNotUtf8,
            Self::TooLarge => Reason::TooLarge,
        }
    }
}

const FORBIDDEN: &[char] = &['"', '*', ':', '<', '>', '?', '\\', '|'];

/// Whether OneDrive refuses `name`, and why.
pub fn refused(name: &OsStr) -> Option<Refused> {
    let Some(name) = name.to_str() else {
        return Some(Refused::NotUtf8);
    };
    if name.contains(FORBIDDEN) {
        return Some(Refused::Characters);
    }
    if name.starts_with(' ') || name.ends_with(' ') {
        return Some(Refused::Spaces);
    }
    let upper = name.to_ascii_uppercase();
    let device = |prefix: &str| upper.strip_prefix(prefix).is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit());
    if matches!(upper.as_str(), ".LOCK" | "CON" | "PRN" | "AUX" | "NUL" | "DESKTOP.INI")
        || device("COM")
        || device("LPT")
        || upper.contains("_VTI_")
        || name.starts_with("~$")
    {
        return Some(Refused::Reserved);
    }
    None
}

/// `name` with the machine's name added before its extension (write design
/// §6): `Report.docx` → `Report-fedora.docx`, `archive.tar.gz` →
/// `archive.tar-fedora.gz`, `.bashrc` → `.bashrc-fedora`; the `n`th try adds
/// `-n` (`Report-fedora-2.docx`). The stem is shortened to keep the name
/// within 255 bytes.
pub fn copy_name(name: &str, machine: &str, n: u32) -> String {
    let suffix = if n <= 1 { format!("-{machine}") } else { format!("-{machine}-{n}") };
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    let room = konedrive_fs::NAME_MAX.saturating_sub(suffix.len() + ext.len());
    let mut cut = stem.len().min(room);
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{suffix}{ext}", &stem[..cut])
}

/// The machine's name for conflict copies (`docs/design/writes.md` §7): the host name
/// up to its first dot, with any character OneDrive refuses replaced by
/// `-`, at most 32 characters; `linux` when there is none.
pub fn machine_name(host: &str) -> String {
    let first = host.trim().split('.').next().unwrap_or_default();
    let cleaned: String = first
        .chars()
        .map(|c| if matches!(c, '"' | '*' | ':' | '<' | '>' | '?' | '/' | '\\' | '|') || c.is_control() { '-' } else { c })
        .take(32)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "linux".into()
    } else {
        cleaned.to_owned()
    }
}

/// [`machine_name`] of this host.
pub fn default_machine_name() -> String {
    machine_name(&std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default())
}

#[cfg(test)]
mod tests;
