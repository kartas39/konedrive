//! The name pre-check (`docs/design/writes.md` §4.4): names OneDrive refuses, found
//! before a request is spent on them.
//!
//! A refused name is `blocked` and listed for the user to rename, as Windows
//! does; nothing is substituted. The service decides in the end — whatever
//! this misses is refused with `400` and blocked with the service's message
//! — so the check stays conservative: it blocks only what Microsoft's page
//! names, never a guess.

use std::ffi::OsStr;

/// The largest file OneDrive takes (Microsoft: 250 GB). Taken as GiB: a file
/// between 250 GB and 250 GiB, if refused, is blocked by the service's answer.
pub const MAX_FILE_SIZE: u64 = 250 << 30;

/// Why a row is blocked before any request. The codes are what `reason`
/// holds; the window and the CLI word them.
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
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Characters => "name-characters",
            Self::Spaces => "name-spaces",
            Self::Reserved => "name-reserved",
            Self::NotUtf8 => "name-not-utf8",
            Self::TooLarge => "too-large",
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

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    #[test]
    fn names_onedrive_refuses_are_found_and_others_pass() {
        let cases = [
            ("a:b.txt", Some(Refused::Characters)),
            ("what?", Some(Refused::Characters)),
            ("pipe|.txt", Some(Refused::Characters)),
            (" lead", Some(Refused::Spaces)),
            ("trail ", Some(Refused::Spaces)),
            ("CON", Some(Refused::Reserved)),
            ("com7", Some(Refused::Reserved)),
            ("LPT0", Some(Refused::Reserved)),
            ("Desktop.ini", Some(Refused::Reserved)),
            (".lock", Some(Refused::Reserved)),
            ("my_vti_file", Some(Refused::Reserved)),
            ("~$doc.docx", Some(Refused::Reserved)),
            ("CON.txt", None),
            ("COM10", None),
            ("console", None),
            ("Отчёт 2024.docx", None),
            ("a b", None),
            (".hidden", None),
        ];
        for (name, expected) in cases {
            assert_eq!(refused(OsStr::new(name)), expected, "{name:?}");
        }
        assert_eq!(refused(OsStr::from_bytes(b"caf\xe9")), Some(Refused::NotUtf8));
    }
}
