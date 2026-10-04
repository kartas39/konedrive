use konedrive_dbus::rows::{Conflict, Freed, FreedSpace};
use konedrive_dbus::Refusal;

use super::formats::{human_bytes, local_time};

/// What each `Skipped()` reason means to the person whose file it is. The
/// same sentences, word for word, as `whyText` in `app/synccontroller.cpp`
/// (each wrapped there in `i18n(...)`);
/// `skip_reason_text_matches_every_branch_of_the_windows_whytext` (in
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

/// `sync conflicts`: each local version kept, where it was and where it is
/// now, and when: moved out of the way, or — in a read-write folder — kept as
/// a copy beside OneDrive's.
pub fn conflicts_text(conflicts: &[Conflict]) -> String {
    if conflicts.is_empty() {
        return "No conflicts.\n".to_owned();
    }
    let mut out = String::new();
    for conflict in conflicts {
        let Conflict { at, original, kept: rescued, .. } = conflict;
        if conflict.is_copy() {
            out.push_str(&format!(
                "{original}\n    changed here and in OneDrive: yours is kept beside it as {rescued}, {}\n",
                local_time(*at)
            ));
        } else {
            out.push_str(&format!("{original}\n    moved to {rescued} on {}\n", local_time(*at)));
        }
    }
    out
}

/// The directories the files `conflicts` lists were rescued to: for each, the rescued path
/// with the original's place in `folder` taken off its end (`rescued/<id>/<time>`, or a
/// directory beside a folder on another filesystem), or the file's own directory when that
/// cannot be told. Each once, in the order first met.
pub fn rescue_dirs(folder: &str, conflicts: &[Conflict]) -> Vec<String> {
    use std::path::Path;
    let mut dirs: Vec<String> = Vec::new();
    // A copy is in the folder, beside its original, and stays with it.
    for Conflict { original, kept, .. } in conflicts.iter().filter(|c| !c.is_copy()) {
        let rescued = Path::new(kept);
        let within = if folder.is_empty() { None } else { Path::new(original).strip_prefix(folder).ok() };
        let dir = match within {
            Some(within) if within.components().next().is_some() && rescued.ends_with(within) => {
                let mut dir = rescued.to_path_buf();
                for _ in within.components() {
                    dir.pop();
                }
                dir
            }
            _ => rescued.parent().map(Path::to_path_buf).unwrap_or_default(),
        };
        let dir = dir.display().to_string();
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// `sync free-up-space`: "Freed N files (X). M files were in use and kept."
pub fn free_up_text(freed: &FreedSpace) -> String {
    let FreedSpace { files, bytes, busy } = *freed;
    if files == 0 && busy == 0 {
        return "Nothing was downloaded, so there was nothing to free up.".to_owned();
    }
    let mut out = format!("Freed {files} {} ({}).", if files == 1 { "file" } else { "files" }, human_bytes(bytes));
    if busy > 0 {
        let were = if busy == 1 { "file was" } else { "files were" };
        out.push_str(&format!(" {busy} {were} in use and kept."));
    }
    out
}

/// `sync hydrate`, done.
pub const DOWNLOADED: &str = "Downloaded.";

/// `sync dehydrate`, done.
pub const FREED_UP: &str = "Freed up.";

/// `sync pin`: that the paths are kept on this device, and how many of their
/// files are downloading now.
pub fn pin_text(queued: u32, prefix: &str) -> String {
    match queued {
        0 => "Kept on this device. Everything in it is here already.".to_owned(),
        1 => format!("Kept on this device. 1 file is downloading (`{prefix} sync transfers`)."),
        n => format!("Kept on this device. {n} files are downloading (`{prefix} sync transfers`)."),
    }
}

/// The path a `NotAllowed` refusal is about, and what pins it: the daemon
/// says exactly "<path> is pinned by <folder>: unpin it first", and the
/// folder is the path or one above it — which tells the two apart even when
/// a name holds " is pinned by " itself.
pub(crate) fn pinned_parts(detail: &str) -> Option<(&str, &str)> {
    const BY: &str = " is pinned by ";
    let both = detail.strip_suffix(": unpin it first")?;
    both.match_indices(BY)
        .map(|(at, _)| (&both[..at], &both[at + BY.len()..]))
        .find(|(path, by)| std::path::Path::new(path).starts_with(by))
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
        Refusal::NotAllowed => pinned_parts(detail).map(|(path, _)| path),
        // "<path> is not uploaded yet, so freeing it up would lose …"
        Refusal::NotUploaded => detail.split_once(" is not uploaded yet").map(|(path, _)| path),
        _ => None,
    }
}

/// `sync unpin`: how many pins came off; the files stay.
pub fn unpin_text(unpinned: u32) -> String {
    match unpinned {
        0 => "Nothing here had a pin of its own, so nothing changed.".to_owned(),
        1 => "No longer kept on this device. What is downloaded stays; `konedrivectl sync free` frees it.".to_owned(),
        n => format!(
            "{n} items are no longer kept on this device. What is downloaded stays; `konedrivectl sync free` frees it."
        ),
    }
}

/// `sync free`: what was freed, what was kept because it was in use or
/// changed here (FreeUp's `busy` counts both), and the downloaded files a pin
/// of their own — or of a folder below the one freed — kept.
pub fn free_text(freed: &Freed) -> String {
    let Freed { files, bytes, busy, pinned } = *freed;
    if files == 0 && busy == 0 && pinned == 0 {
        return "Nothing here was downloaded, so there was nothing to free up.".to_owned();
    }
    let mut out = if files == 0 {
        "Nothing was freed up.".to_owned()
    } else {
        format!("Freed {files} {} ({}).", if files == 1 { "file" } else { "files" }, human_bytes(bytes))
    };
    if busy > 0 {
        let were = if busy == 1 { "file was" } else { "files were" };
        out.push_str(&format!(" {busy} {were} in use or changed here, and kept."));
    }
    if pinned > 0 {
        let were = if pinned == 1 { "file was" } else { "files were" };
        out.push_str(&format!(" {pinned} {were} kept: a file or folder below is kept on this device."));
    }
    out
}

#[cfg(test)]
mod tests;
