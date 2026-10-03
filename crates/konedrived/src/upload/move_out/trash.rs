use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// the Trash
// ---------------------------------------------------------------------------

/// An entry of a Trash: the top-level object in `files/` and its `.trashinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    pub top: PathBuf,
    pub info: PathBuf,
    /// `.Trash/<uid>`: the shared `.Trash`, which must be a sticky directory.
    pub shared: Option<PathBuf>,
}

/// The Trash entry `path` is in, by its place (the freedesktop.org specification): the user's
/// own (`home_trash`), or `.Trash-<uid>` or `.Trash/<uid>` directly at the top of a mount
/// (`is_mount_point`). A `.Trash-<uid>` anywhere else is an ordinary directory.
pub fn trash_of(path: &Path, home_trash: Option<&Path>, uid: u32, is_mount_point: &dyn Fn(&Path) -> bool) -> Option<TrashEntry> {
    let parts: Vec<Component<'_>> = path.components().collect();
    let uid = uid.to_string();
    for i in 1..parts.len().saturating_sub(1) {
        if parts[i].as_os_str() != "files" {
            continue;
        }
        let trash: PathBuf = parts[..i].iter().collect();
        let name = trash.file_name().map(OsStr::to_os_string).unwrap_or_default();
        let parent = trash.parent();
        let shared = parent.filter(|p| p.file_name() == Some(OsStr::new(".Trash")));
        let is_trash = home_trash.is_some_and(|home| home == trash)
            || (name == OsString::from(format!(".Trash-{uid}")) && parent.is_some_and(is_mount_point))
            || (name == OsString::from(&uid) && shared.and_then(Path::parent).is_some_and(is_mount_point));
        if !is_trash {
            continue;
        }
        let top = parts[i + 1].as_os_str();
        let mut info = top.to_os_string();
        info.push(".trashinfo");
        let shared = shared.filter(|_| home_trash.is_none_or(|home| home != trash)).map(Path::to_path_buf);
        return Some(TrashEntry { top: trash.join("files").join(top), info: trash.join("info").join(info), shared });
    }
    None
}

/// Whether `entry` is a Trash's as a desktop makes one: its `.trashinfo` is there (a conforming
/// desktop writes it before it moves the object), and a shared `.Trash` is a sticky directory,
/// not a symlink.
pub(super) fn real_trash(entry: &TrashEntry) -> bool {
    let info = std::fs::symlink_metadata(&entry.info).is_ok_and(|m| m.is_file());
    let shared = entry.shared.as_ref().is_none_or(|dir| {
        std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o1000 != 0)
    });
    info && shared
}

/// Whether `path` is where a filesystem is mounted (`/proc/self/mountinfo`).
pub(super) fn is_mount_point(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else { return false };
    text.lines().filter_map(|line| line.split(' ').nth(4)).any(|at| Path::new(&unescape(at)) == path)
}

/// A mountinfo field: `\040` and the like are octal escapes.
fn unescape(field: &str) -> OsString {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&field[i + 1..i + 4], 8) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    <OsString as std::os::unix::ffi::OsStringExt>::from_vec(out)
}

/// The user's own Trash: `$XDG_DATA_HOME/Trash`, or `~/.local/share/Trash`. Read from the
/// environment only; nothing is opened.
pub fn home_trash() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))?;
    Some(data.join("Trash"))
}
