//! The ignore list (`docs/design/writes.md` §4.4): names that stay local.
//!
//! Shell globs (`*`, `?`, `[...]`, `\` to escape) match the name, not the
//! path, case-sensitively, and `*` matches a leading dot too (a vim swap file
//! is `.name.swp`). The list applies only to local files without an item id:
//! an item that came from the cloud syncs whatever its name.

use std::ffi::OsStr;
use std::sync::{Arc, RwLock};

/// Editors' and browsers' temporary files, lock files and the like.
pub const DEFAULT_PATTERNS: &[&str] = &[
    "*.swp",
    "*.swo",
    "*.swx",
    "~$*",
    ".~lock.*#",
    "*.part",
    "*.crdownload",
    "*.tmp",
    "*~",
    ".#*",
    "#*#",
    ".goutputstream-*",
    "*.kate-swp",
    ".fuse_hidden*",
    ".nfs*",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreList {
    patterns: Vec<String>,
}

impl Default for IgnoreList {
    fn default() -> Self {
        Self::new(DEFAULT_PATTERNS.iter().copied())
    }
}

/// An account's ignore list as its watcher and its settings share it:
/// `SetIgnorePatterns` changes what the next examination uses.
pub type SharedIgnore = Arc<RwLock<IgnoreList>>;

impl IgnoreList {
    pub fn new<S: Into<String>>(patterns: impl IntoIterator<Item = S>) -> Self {
        Self { patterns: patterns.into_iter().map(Into::into).collect() }
    }

    /// The account's list: `patterns` from its config, or the defaults.
    pub fn configured(patterns: Option<&[String]>) -> Self {
        match patterns {
            Some(patterns) => Self::new(patterns.iter().cloned()),
            None => Self::default(),
        }
    }

    /// This list, to share.
    pub fn shared(self) -> SharedIgnore {
        Arc::new(RwLock::new(self))
    }

    /// Why `pattern` is no pattern: empty, a `/` (a pattern matches a name,
    /// never a path), a NUL, or longer than a name can be.
    pub fn invalid(pattern: &str) -> Option<&'static str> {
        if pattern.trim().is_empty() {
            Some("a pattern cannot be empty")
        } else if pattern.contains('/') {
            Some("a pattern matches a name, not a path: it cannot hold '/'")
        } else if pattern.contains('\0') {
            Some("a pattern cannot hold a NUL")
        } else if pattern.len() > 255 {
            Some("a pattern cannot be longer than a name (255 bytes)")
        } else {
            None
        }
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// Whether `name` stays local. A name that is not UTF-8 is matched as
    /// its lossy form.
    pub fn matches(&self, name: &OsStr) -> bool {
        let name: Vec<char> = name.to_string_lossy().chars().collect();
        self.patterns.iter().any(|pattern| glob(&pattern.chars().collect::<Vec<_>>(), &name))
    }
}

/// `[...]` at `p[start]` against `c`: whether it matches, and where the
/// pattern goes on. `None` when there is no closing `]`: the `[` is literal.
fn class(p: &[char], start: usize, c: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negate = matches!(p.get(i), Some('!' | '^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == ']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        let low = p[i];
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            matched |= low <= c && c <= p[i + 2];
            i += 3;
        } else {
            matched |= low == c;
            i += 1;
        }
    }
    None
}

fn glob(p: &[char], n: &[char]) -> bool {
    let (mut pi, mut ni) = (0, 0);
    // The last `*` and where in the name it stands: backtracking goes there.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        let step = match p.get(pi) {
            Some('*') => {
                star = Some((pi, ni));
                pi += 1;
                continue;
            }
            Some('?') => Some(pi + 1),
            Some('[') => match class(p, pi, n[ni]) {
                Some((true, next)) => Some(next),
                Some((false, _)) => None,
                None => (n[ni] == '[').then_some(pi + 1),
            },
            Some('\\') if pi + 1 < p.len() => (p[pi + 1] == n[ni]).then_some(pi + 2),
            Some(&c) => (c == n[ni]).then_some(pi + 1),
            None => None,
        };
        match step {
            Some(next) => {
                pi = next;
                ni += 1;
            }
            None => match star {
                Some((sp, sn)) => {
                    pi = sp + 1;
                    ni = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignored(name: &str) -> bool {
        IgnoreList::default().matches(OsStr::new(name))
    }

    #[test]
    fn the_defaults_catch_editors_temporary_files_and_nothing_else() {
        for name in [
            ".report.txt.swp",
            "notes.txt~",
            ".~lock.budget.ods#",
            "~$budget.xlsx",
            "#draft.org#",
            ".#draft.org",
            ".goutputstream-ABC123",
            "file.kate-swp",
            "movie.mkv.part",
            "setup.exe.crdownload",
            "lu12345abc.tmp",
            ".fuse_hidden0001",
            ".nfs000123",
        ] {
            assert!(ignored(name), "{name}");
        }
        for name in ["report.txt", "swp", "a.swp.txt", "~notes", "lock.budget.ods#", "draft#", "tmp"] {
            assert!(!ignored(name), "{name}");
        }
    }

    #[test]
    fn globs_have_classes_escapes_and_are_case_sensitive() {
        let list = IgnoreList::new(["[a-c]?.log", "[!x]z", "\\*star", "*.TMP"]);
        assert!(list.matches(OsStr::new("b1.log")));
        assert!(!list.matches(OsStr::new("d1.log")));
        assert!(list.matches(OsStr::new("yz")) && !list.matches(OsStr::new("xz")));
        assert!(list.matches(OsStr::new("*star")) && !list.matches(OsStr::new("astar")));
        assert!(list.matches(OsStr::new("A.TMP")) && !list.matches(OsStr::new("a.tmp")));
        assert!(IgnoreList::new(["[unclosed"]).matches(OsStr::new("[unclosed")));
        assert!(IgnoreList::new(["a*b*c"]).matches(OsStr::new("aXbYbZc")));
        assert!(!IgnoreList::new(Vec::<String>::new()).matches(OsStr::new("x.swp")));
    }
}
