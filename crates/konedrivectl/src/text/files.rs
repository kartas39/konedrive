use konedrive_dbus::rows::{Conflict, Freed, FreedSpace};
use konedrive_dbus::Refusal;

use super::formats::{human_bytes, local_time};

/// What each `Skipped()` reason means to the person whose file it is. The
/// same sentences, word for word, as `skipReasonText` in `app/skipreasons.cpp`
/// (each wrapped there in `i18n(...)`);
/// `skip_reason_text_matches_every_branch_of_the_windows_skip_reason_text` (in
/// `tests/sync_cli/wording.rs`) compares them with the C++ source, so the two cannot
/// drift apart unnoticed.
pub fn skip_reason_text(reason: &str) -> &'static str {
    match reason {
        "name-too-long" => "The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two).",
        "personal-vault" => "The Personal Vault is locked separately and is not synced.",
        "shared" => "A shared folder added to your OneDrive; shared folders are not synced yet.",
        "onenote" => "A OneNote notebook, which is not a file.",
        "reserved-name" => "The name begins with .konedrive-, which konedrive keeps for itself.",
        _ => "It is neither a file nor a folder konedrive can show.",
    }
}

/// What `Skipped()` says keeps an item on this computer that the folder
/// cannot hold any more (`waits`, as sent); `None` for an item that is not
/// here. The catalogue's sentences (`konedrive-text`), which the window's
/// generated table has too.
pub fn still_here_text(waits: &str) -> Option<String> {
    konedrive_text::waits::text(waits)
}

/// Where an item `Skipped()` lists still is on this computer. The same
/// sentence as the window's.
pub fn where_here_text(here: &str) -> String {
    format!("It is at {here}.")
}

/// `sync conflicts`: each file changed here and in OneDrive, the copy kept of the local
/// version beside it, and when.
pub fn conflicts_text(conflicts: &[Conflict]) -> String {
    if conflicts.is_empty() {
        return "No conflicts.\n".to_owned();
    }
    let mut out = String::new();
    for Conflict { at, original, kept, .. } in conflicts {
        out.push_str(&format!("{original}\n    changed here and in OneDrive: yours is kept beside it as {kept}, {}\n", local_time(*at)));
    }
    out
}

/// `sync free-up-space`: "Freed N files (X)."
pub fn free_up_text(freed: &FreedSpace) -> String {
    let FreedSpace { files, bytes } = *freed;
    if files == 0 {
        return "Nothing was downloaded, so there was nothing to free up.".to_owned();
    }
    format!("Freed {files} {} ({}).", if files == 1 { "file" } else { "files" }, human_bytes(bytes))
}

/// `sync hydrate`, done.
pub const DOWNLOADED: &str = "Downloaded.";

/// `sync dehydrate`, done.
pub const FREED_UP: &str = "Freed up: it takes no space on this computer.";

/// How a suggested command starts when the accounts could not be read to name one.
pub const PLAIN_PREFIX: &str = "konedrivectl";

/// After a path command did what it was asked: the folders could not be read (`error` says
/// why), so the account of a suggested command is not named and whether a folder needs
/// attention was not checked.
pub fn folders_unread_text(error: &str) -> String {
    format!("done, but the sync folders could not be read afterwards, so whether one needs attention was not checked: {error}")
}

/// `sync pin`: that the paths are kept on this device, and how many of their
/// files are downloading now.
pub fn pin_text(queued: u32, prefix: &str) -> String {
    match queued {
        0 => "Kept on this device. Everything in it is here already.".to_owned(),
        1 => format!("Kept on this device. 1 file is downloading (`{prefix} sync transfers`)."),
        n => format!("Kept on this device. {n} files are downloading (`{prefix} sync transfers`)."),
    }
}

/// The path a `NotAllowed` refusal is about: the daemon says exactly
/// `<path> is pinned: unpin it first`.
pub(crate) fn pinned_path(detail: &str) -> Option<&str> {
    detail.strip_suffix(" is pinned: unpin it first")
}

/// The one path a `NotAllowed` or `NotUploaded` refusal of a call on several
/// is about. `None` for any other error.
pub fn refused_path(error: &zbus::Error) -> Option<&str> {
    let zbus::Error::MethodError(name, Some(detail), _) = error else { return None };
    refused_path_of(name.as_str(), detail)
}

/// [`refused_path`], on the error's name and message.
fn refused_path_of<'a>(name: &str, detail: &'a str) -> Option<&'a str> {
    match Refusal::parse(name) {
        Refusal::NotAllowed => pinned_path(detail),
        // "<path> is not uploaded yet, so freeing it up would lose …"
        Refusal::NotUploaded => detail.split_once(" is not uploaded yet").map(|(path, _)| path),
        _ => None,
    }
}

/// `sync unpin`: how many pins came off; the files stay.
pub fn unpin_text(unpinned: u32) -> String {
    match unpinned {
        0 => "Nothing here was kept on this device, so nothing changed.".to_owned(),
        1 => "No longer kept on this device. What is downloaded stays; `konedrivectl sync free` frees it.".to_owned(),
        n => format!(
            "{n} items are no longer kept on this device. What is downloaded stays; `konedrivectl sync free` frees it."
        ),
    }
}

/// The keys of `Files.Menu`'s answer, in the order `sync menu` prints them.
const MENU_KEYS: [&str; 6] = ["paths", "always-keep", "free-up", "free-up-why", "open-online", "open-online-path"];

/// `sync menu`: the answer of `Files.Menu`, one line per key, `key: value`. A list is
/// its entries, each quoted, with a space between; an empty value leaves the line at
/// the key. A key this build does not know is printed after the others.
pub fn menu_text(answer: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>) -> String {
    use zbus::zvariant::Value;
    fn shown(value: &Value<'_>) -> String {
        match value {
            Value::Str(text) => text.to_string(),
            Value::Array(entries) => entries
                .iter()
                .map(|entry| match entry {
                    Value::Str(text) => format!("{:?}", text.as_str()),
                    other => shown(other),
                })
                .collect::<Vec<_>>()
                .join(" "),
            other => format!("{other:?}"),
        }
    }
    let mut unknown: Vec<&str> = answer.keys().map(String::as_str).filter(|key| !MENU_KEYS.contains(key)).collect();
    unknown.sort_unstable();
    let mut out = String::new();
    for key in MENU_KEYS.into_iter().chain(unknown) {
        if let Some(value) = answer.get(key) {
            out.push_str(format!("{key}: {}", shown(value)).trim_end());
            out.push('\n');
        }
    }
    out
}

/// `sync free`: what was freed, and the downloaded files left as they are: being
/// downloaded, open for writing, or changed here and not uploaded yet.
pub fn free_text(freed: &Freed) -> String {
    let Freed { files, bytes, pinned } = *freed;
    if files == 0 && pinned == 0 {
        return "Nothing here was downloaded, so there was nothing to free up.".to_owned();
    }
    let mut out = if files == 0 {
        "Nothing was freed up.".to_owned()
    } else {
        format!("Freed {files} {} ({}).", if files == 1 { "file" } else { "files" }, human_bytes(bytes))
    };
    if pinned > 0 {
        let were = if pinned == 1 { "file was" } else { "files were" };
        out.push_str(&format!(" {pinned} {were} kept on this device: being downloaded, open for writing, or changed here and not uploaded yet."));
    }
    out
}

#[cfg(test)]
mod tests;
