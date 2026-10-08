//! A file or a directory a program renames or moves inside the folder.
//!
//! An item is moved in OneDrive by the outbox, and stands at its new place at once. What
//! has no item yet is sent from its new place, or no longer sent when the new place stays
//! on this computer. A file without an item renamed over an item's file is that item's new
//! content: programs save that way.

use std::fs;

use fuser::Errno;
use konedrive_tree::outbox::{OutboxKind, OutboxOp, OutboxState};

use super::copies::mark_copy;
use super::changes::{based, forgotten, leaves, object, rebase, waits, Place};
use super::local::{Dir, Entry};
use super::nodes::Key;
use super::shared::{errno, Shared};
use crate::folder::disk::daemon_owned;

impl Shared {
    /// Renames `name` of the directory `parent` to `new_name` of `new_parent`, over what
    /// stands there unless `keep` says to refuse that.
    pub(super) fn rename(&self, parent: u64, name: &str, new_parent: u64, new_name: &str, keep: bool) -> Result<(), Errno> {
        self.may_change()?;
        let (from_dir, to_dir) = (self.dir(parent).ok_or(Errno::ENOENT)?, self.dir(new_parent).ok_or(Errno::ENOENT)?);
        if daemon_owned(new_name.as_ref()) {
            return Err(Errno::EPERM);
        }
        let src = self.entry(&from_dir, name).ok_or(Errno::ENOENT)?;
        let dst = self.entry(&to_dir, new_name);
        let (from, to) = (from_dir.rel.join(name), to_dir.rel.join(new_name));
        if from == to {
            return Ok(());
        }
        if let Some(dst) = &dst {
            let holds = |dst: &Entry| !self.entries(&Dir { id: dst.id().map(str::to_owned), rel: to.clone() }).is_empty();
            match (src.is_dir(), dst.is_dir()) {
                _ if keep => return Err(Errno::EEXIST),
                (false, true) => return Err(Errno::EISDIR),
                (true, false) => return Err(Errno::ENOTDIR),
                (true, true) if holds(dst) => return Err(Errno::ENOTEMPTY),
                _ => {}
            }
        }
        // An item cannot be moved to where nothing is sent from: a program that copies it
        // there and removes it here does what that means.
        let (stays, local_dir) = (self.stays_local(&to_dir, new_name), self.stays_local(&to_dir, ""));
        if src.id().is_some() && local_dir {
            return Err(Errno::EXDEV);
        }
        let src_ino = self.ino_of(&src, parent);
        let open = self.writers.open(src_ino);
        // The folder under the mount first: whatever the item or object has there moves, and
        // what stood at the new place goes.
        self.backing(&to_dir.rel).map_err(|e| errno(&e))?;
        let (was, now) = (self.under(&from), self.under(&to));
        let displaced = fs::symlink_metadata(&now).ok();
        let moved = fs::symlink_metadata(&was).is_ok();
        if let Some(meta) = displaced.as_ref().filter(|m| m.is_dir() || !moved) {
            let _ = if meta.is_dir() { fs::remove_dir(&now) } else { fs::remove_file(&now) };
        }
        if moved {
            fs::rename(&was, &now).map_err(|e| errno(&e))?;
        }
        let taken_over = match (&src, &dst) {
            (Entry::Local(local), Some(Entry::Item(over))) if !local.meta.is_dir() && !over.name.is_empty() => Some(over.id.clone()),
            _ => None,
        };
        // What has no item and is sent from the new place: itself, and all it holds.
        let mut news = Vec::new();
        if let (Entry::Local(local), false, None) = (&src, stays, &taken_over) {
            if local.meta.is_dir() {
                self.below(&to_dir, new_name, &mut news);
            } else {
                let mut new = Place { dir: &to_dir, name: new_name }.new(&local.meta, open);
                new.rel = to.clone();
                news.push(new);
            }
        }
        let src_item = src.id().map(str::to_owned);
        let src_object = match &src {
            Entry::Local(local) => Some(local.meta.clone()),
            Entry::Item(_) => fs::symlink_metadata(&now).ok().filter(|_| moved),
        };
        let (src_dir, dst_item) = (src.is_dir(), dst.as_ref().and_then(|d| d.id().map(str::to_owned)));
        let dst_object = match &dst {
            Some(Entry::Local(local)) => Some(object(&local.meta)),
            _ => None,
        };
        let (at, came_from, place_dir, place_name, over) = (to.clone(), from.clone(), to_dir.clone(), new_name.to_owned(), taken_over.clone());
        let recorded = self.record(move |s| {
            let place = Place { dir: &place_dir, name: &place_name };
            let (mut detections, mut ops) = (Vec::new(), Vec::new());
            // What stood at the new place.
            match (&over, &dst_item, &dst_object) {
                (Some(id), _, _) => {
                    if let (Some(base), Some(meta)) = (based(s, id)?, &src_object) {
                        let pending = s.outbox_for_inode(&object(meta))?;
                        ops.extend(pending.iter().filter(|row| row.state != OutboxState::Running).map(|row| OutboxOp::Remove(row.seq)));
                        let mut update = place.of_item(OutboxKind::Update, &base, Some(meta));
                        if open {
                            waits(&mut update);
                        }
                        detections.push(update);
                    }
                }
                (None, Some(id), _) => detections.extend(based(s, id)?.iter().map(|base| leaves(Some(base), None, &at))),
                (None, None, Some(inode)) => detections.extend(forgotten(s, inode, &at)?),
                _ => {}
            }
            // What moved.
            match (&src_item, &src_object) {
                _ if over.is_some() => {}
                (Some(id), object) => {
                    if let Some(base) = based(s, id)? {
                        let mut moved = place.of_item(OutboxKind::Move, &base, object.as_ref());
                        moved.same_content = !s.outbox_for_item(id)?.iter().any(|row| row.kind == OutboxKind::Update);
                        detections.push(moved);
                    }
                }
                (None, Some(meta)) if news.is_empty() => {
                    // It stays on this computer from now on, with what is below it.
                    detections.extend(forgotten(s, &object(meta), &at)?);
                    let below = s.outbox_at_or_under(&came_from)?;
                    ops.extend(below.iter().filter(|row| row.item_id.is_none() && row.state != OutboxState::Running).map(|row| OutboxOp::Remove(row.seq)));
                }
                (None, Some(meta)) => {
                    let being_sent = s.outbox_for_inode(&object(meta))?.iter().any(|row| row.state == OutboxState::Running);
                    if being_sent && !src_dir {
                        // Going up as it is: only where it stands changed.
                        news[0].kind = OutboxKind::Move;
                    }
                    detections.append(&mut news);
                }
                (None, None) => {}
            }
            if src_dir {
                ops.push(rebase(&came_from, &at));
            }
            Ok((detections, ops))
        });
        if let Err(e) = recorded {
            tracing::warn!("the move of {} to {} could not be recorded: {e}", from.display(), to.display());
            if moved {
                let _ = fs::rename(&now, &was);
            }
            return Err(Errno::EIO);
        }
        if let Some(id) = &taken_over {
            // The file is the item's content from here on: read as its downloaded copy.
            let _ = mark_copy(&now, id);
        }
        let mut nodes = crate::panic::lock(&self.nodes);
        nodes.moved(src_ino, new_parent, new_name);
        if let (Some(id), Entry::Local(local)) = (&taken_over, &src) {
            // The file's node is the item's from here on: whoever looks the name up gets
            // the node the new content is read and written through.
            nodes.rekey(&Key::Local(std::os::unix::fs::MetadataExt::ino(&local.meta)), Key::Item(id.clone()));
        }
        Ok(())
    }
}
