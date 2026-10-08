//! What changed in OneDrive, brought into the store and the folder under the mount: a whole
//! listing the first time, the changes since the stored link after that.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use konedrive_graph::drive::{DeltaFrom, DeltaNext, DriveClient};
use konedrive_tree::{Kind, NewTree, Row, Table, TreeStore};
use konedrived::folder::classify::classify;

/// One item a cycle changed: where the store had it and where it has it now.
pub struct Step {
    pub id: String,
    pub old: Option<(Row, PathBuf)>,
    pub new: Option<(Row, PathBuf)>,
}

impl Step {
    /// The item is a file whose content is another now.
    pub fn content_changed(&self) -> bool {
        matches!((&self.old, &self.new), (Some((o, _)), Some((n, _)))
            if n.kind == Kind::File && (o.ctag != n.ctag || o.quickxor != n.quickxor || o.size != n.size))
    }
}

fn other(e: impl ToString) -> io::Error {
    io::Error::other(e.to_string())
}

fn placed(store: &TreeStore, table: Table, id: &str) -> Option<(Row, PathBuf)> {
    let row = store.get(table, id).ok().flatten()?;
    let at = store.locate(table, id).ok().flatten()?;
    at.placed.then_some((row, at.rel))
}

/// Removes the downloaded copy at `path` if it is the copy of `id` and no other file.
fn remove_copy(path: &Path, id: &str) {
    let ours = File::open(path).ok().and_then(|f| konedrive_fs::placeholder::read_item_id(&f).ok().flatten());
    if ours.as_deref() == Some(id) {
        let _ = fs::remove_file(path);
    }
}

/// Makes the folder under the mount follow one change: a downloaded copy whose content is
/// stale or whose item is gone is removed, and one whose item moved moves with it. A folder
/// that is gone is left where it is; the mount no longer shows it.
fn follow(under: &Path, step: &Step) {
    match (&step.old, &step.new) {
        (Some((old, was)), None) if old.kind == Kind::File => remove_copy(&under.join(was), &step.id),
        (Some((old, was)), Some((_, now))) => {
            if step.content_changed() {
                remove_copy(&under.join(was), &step.id);
            } else if was != now && fs::symlink_metadata(under.join(was)).is_ok() {
                let to = under.join(now);
                if fs::symlink_metadata(&to).is_err() {
                    let _ = to.parent().map(fs::create_dir_all);
                    let _ = fs::rename(under.join(was), to);
                }
            } else if old.kind == Kind::File && was != now {
                // Nothing downloaded: only the store had it.
            }
        }
        _ => {}
    }
}

/// One cycle: asks OneDrive, stages what it says, makes the folder under the mount follow,
/// and commits. Returns what changed.
pub async fn sync_once(drive: &DriveClient, store: &Mutex<TreeStore>, under: &Path) -> io::Result<Vec<Step>> {
    let link = store.lock().unwrap().delta_link().map_err(other)?;
    let mut from = link.clone().map_or(DeltaFrom::Start, DeltaFrom::Link);
    let mut changes = Vec::new();
    let done = loop {
        let page = drive.delta(&from).await.map_err(other)?;
        changes.extend(page.items.iter().map(classify));
        match page.next {
            DeltaNext::Page(next) => from = DeltaFrom::Link(next),
            DeltaNext::Done(done) => break done,
        }
    };
    let mut store = store.lock().unwrap();
    if changes.is_empty() {
        store.set_delta_link(&done).map_err(other)?;
        return Ok(Vec::new());
    }
    store.begin_staging(if link.is_some() { NewTree::Delta } else { NewTree::Whole }).map_err(other)?;
    store.stage(&changes).map_err(other)?;
    let steps: Vec<Step> = store
        .changed_ids()
        .map_err(other)?
        .into_iter()
        .map(|id| Step { old: placed(&store, Table::Items, &id), new: placed(&store, Table::Staging, &id), id })
        .collect();
    steps.iter().for_each(|step| follow(under, step));
    store.commit_staging(&done).map_err(other)?;
    Ok(steps)
}
