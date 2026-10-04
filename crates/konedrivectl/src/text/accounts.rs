use konedrive_dbus::rows::Conflict;

use super::files::rescue_dirs;
use crate::FIRST_LABEL;

/// One line of `account list`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountRow {
    pub id: String,
    pub label: String,
    pub email: String,
    pub state: String,
    pub mode: String,
    /// `Folder.Path`: empty with no folder.
    pub folder: String,
    pub root_state: String,
}

/// `account list` (design §5.2): a table of every account, in the order they were added —
/// id, label, email, sign-in state, mode, and the folder with its `Folder.State`.
pub fn account_list_text(rows: &[AccountRow]) -> String {
    if rows.is_empty() {
        return format!(
            "No accounts yet. `konedrivectl login` adds one called {FIRST_LABEL} and signs it in; \
             `konedrivectl account add <label>` adds one by another name.\n"
        );
    }
    let none = || "\u{2014}".to_owned();
    let mut table = vec![["ID", "LABEL", "EMAIL", "STATE", "MODE", "FOLDER"].map(str::to_owned)];
    for row in rows {
        let email = if row.email.is_empty() { none() } else { row.email.clone() };
        let folder = if row.folder.is_empty() { none() } else { format!("{} ({})", row.folder, row.root_state) };
        table.push([row.id.clone(), row.label.clone(), email, row.state.clone(), row.mode.clone(), folder]);
    }
    let mut widths = [0; 6];
    for line in &table {
        for (width, cell) in widths.iter_mut().zip(line) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for line in &table {
        let cells: Vec<String> = line.iter().zip(widths).map(|(cell, w)| format!("{cell:<w$}")).collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// What `account remove` says it did: what was deleted, and what was kept — the folder, as
/// it is, and the files the conflicts list named (read before the removal), by where they
/// were rescued to. Rescued files are never deleted; where the rescues of conflicts
/// dismissed earlier went cannot be told from here (the data directory, beside a folder on
/// another filesystem, or a migrated account's older ones), so only that they stay is said.
pub fn removed_text(label: &str, folder: &str, conflicts: &[Conflict]) -> String {
    let mut out = format!(
        "Removed the account {label}: it is signed out, and its token, cached name and quota, list of \
         OneDrive items, activity and conflicts list are deleted.\n"
    );
    if folder.is_empty() {
        out.push_str("It had no folder.\n");
    } else {
        out.push_str(&format!(
            "Kept: the folder {folder}, as it is. A file in it that was never downloaded stays as an empty \
             placeholder, which reads as zeros.\n"
        ));
    }
    let rescued = conflicts.iter().filter(|c| !c.is_copy()).count();
    let files = if rescued == 1 { "1 file".to_owned() } else { format!("{rescued} files") };
    match rescue_dirs(folder, conflicts).as_slice() {
        [] => {}
        [one] => out.push_str(&format!("Kept: the {files} the conflicts list named, rescued in {one}\n")),
        several => {
            out.push_str(&format!("Kept: the {files} the conflicts list named, rescued in:\n"));
            for dir in several {
                out.push_str(&format!("  {dir}\n"));
            }
        }
    }
    if !folder.is_empty() || !conflicts.is_empty() {
        out.push_str("Rescued files are never deleted: any from conflicts dismissed earlier stay where they were moved.\n");
    }
    out
}
