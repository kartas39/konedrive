use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::time::{Duration, SystemTime};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::lease;
use konedrive_fs::placeholder::{self, State};
use crate::local::batch::Batch;
use crate::local::entry::{Entry, StateAttr, Type};
use crate::local::names;
use crate::local::{snapshot, RECHECK};
use crate::folder::locks::InodeKey;
use konedrive_tree::outbox::{OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::{Row, Table};

use super::{Content, ExamineError, Expect, gone, OPEN_FOR_WRITING, Run, UNKNOWN_STATE};

impl Run<'_, '_> {
    /// Item `id` found as entry `i`: where it is, and its content.
    pub(super) fn found(&mut self, id: &str, i: usize, batch: &Batch) -> Result<(), ExamineError> {
        let e = self.entries[i].clone();
        let Some(base) = self.base_row(id)? else { return Ok(()) };
        if !self.located(id)?.is_some_and(|l| l.placed) {
            // Not placed by the base, and placed right here by the new tree:
            // a reconcile is placing it now, and its swap follows — never a
            // move of the user's (issue #104).
            let placing = self.store({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?.is_some_and(|l| l.placed && l.rel == e.rel);
            if placing {
                self.recheck(&e);
                return Ok(());
            }
        }
        let recorded = self.local_handle(id)?;
        if e.handle.is_some() && e.handle != recorded {
            self.ops.push(OutboxOp::SetHandle { item_id: id.to_owned(), handle: e.handle.clone() });
        }
        if e.ty == Type::File && e.nlink > 1 && matches!(e.state, StateAttr::Known(State::OnlineOnly | State::Hydrating | State::Dehydrating)) {
            self.out.mark_files.push(e.rel.clone());
        }
        if e.ty == Type::Dir {
            if let Expect::At(was) = self.expected(id)? {
                if was != e.rel {
                    self.ops.push(OutboxOp::Rebase { from: was, to: e.rel.clone() });
                }
            }
        }
        let content = if e.ty == Type::File { self.content(id, &base, &e, batch)? } else { Content::Same };
        let mut d = self.detection(OutboxKind::Move, id, &base, &e, e.ctag.as_deref());
        match content {
            Content::Changed => d.kind = OutboxKind::Update,
            Content::Waiting => {
                d.kind = OutboxKind::Update;
                d.state = OutboxState::Waiting;
                d.reason = Some(OPEN_FOR_WRITING.into());
                d.next_try = Some(self.ex.now + RECHECK.as_secs() as i64);
            }
            Content::Same => d.same_content = true,
            Content::Unknown => {}
        }
        // Renamed to a name OneDrive refuses: blocked until renamed again.
        if e.name != OsStr::new(&base.name) {
            if let Some(refused) = names::refused(&e.name) {
                d.state = OutboxState::Blocked;
                d.reason = Some(refused.as_str().into());
                d.next_try = None;
            }
        }
        let at_base = d.target_parent.as_deref() == base.parent_id.as_deref() && d.target_name.as_deref() == Some(base.name.as_str());
        // In place, unchanged or unknown, with no row — or only rows of an
        // object of it that is leaving (issue #104): nothing to record.
        if d.kind == OutboxKind::Move && at_base && self.rows.of_item(id).all(|r| self.under_leaving(&r.rel)) {
            return Ok(());
        }
        self.detections.push(d);
        Ok(())
    }

    /// Item `id` found as `e` at or below an object that is leaving (a name
    /// too long, the Personal Vault...): an object that stays on disk only
    /// until what waits inside it is uploaded (issue #104). It is never
    /// moved in OneDrive to where it is here, nor recorded as the item's
    /// object again; only its content, changed here, goes up into the item
    /// where OneDrive has it.
    pub(super) fn found_leaving(&mut self, id: &str, base: &Row, e: &Entry, batch: &Batch) -> Result<(), ExamineError> {
        if e.ty != Type::File {
            return Ok(());
        }
        if matches!(e.state, StateAttr::Absent | StateAttr::Corrupt) {
            // Whether it holds anything cannot be told: listed, and its
            // folder stays (issue #104).
            self.skip(&e.rel, UNKNOWN_STATE);
            return Ok(());
        }
        // A change OneDrive answered `404` for while it still lists the item
        // stays blocked until the listing settles it, or the file changes
        // again (issue #104): looking at it again is no reason to retry.
        let now = snapshot(e.size, e.mtime.0, e.mtime.1);
        if self.rows.of_item(id).any(|row| {
            row.state == OutboxState::Blocked && row.reason.as_deref() == Some(konedrive_tree::outbox::LEAVING_NOT_FOUND) && row.snapshot.as_deref() == Some(now.as_str())
        }) {
            return Ok(());
        }
        let mut d = self.detection(OutboxKind::Update, id, base, e, e.ctag.as_deref());
        (d.target_parent, d.target_name) = (base.parent_id.clone(), Some(base.name.clone()));
        match self.content(id, base, e, batch)? {
            Content::Changed => {}
            Content::Waiting => {
                d.state = OutboxState::Waiting;
                d.reason = Some(OPEN_FOR_WRITING.into());
                d.next_try = Some(self.ex.now + RECHECK.as_secs() as i64);
            }
            Content::Same | Content::Unknown => return Ok(()),
        }
        self.detections.push(d);
        Ok(())
    }

    /// The content check (§3.4) of item `id`'s file `e` against `base`.
    fn content(&mut self, id: &str, base: &Row, e: &Entry, batch: &Batch) -> Result<Content, ExamineError> {
        match e.state {
            StateAttr::Known(State::Hydrating | State::Dehydrating) => {
                self.recheck(e);
                Ok(Content::Unknown)
            }
            StateAttr::Known(State::OnlineOnly) => {
                if e.size != base.size {
                    self.restore(e, base)?;
                }
                Ok(Content::Same)
            }
            StateAttr::Absent | StateAttr::Corrupt => {
                tracing::warn!("{} carries an item id and no state konedrive can read; it is left alone", e.rel.display());
                Ok(Content::Unknown)
            }
            StateAttr::Known(State::Hydrated) => self.hydrated(id, base, e, batch),
        }
    }

    fn hydrated(&mut self, id: &str, base: &Row, e: &Entry, batch: &Batch) -> Result<Content, ExamineError> {
        let now = snapshot(e.size, e.mtime.0, e.mtime.1);
        if self.rows.of_item(id).any(|row| row.state == OutboxState::Running && row.snapshot.as_deref() == Some(now.as_str())) {
            // Being uploaded as it is now.
            return Ok(Content::Unknown);
        }
        let size_changed = e.stamp.is_some_and(|s| s.size != e.size);
        let time_changed = e.stamp.is_none_or(|s| (s.mtime_sec, s.mtime_nsec) != e.mtime);
        let written = e.handle.as_ref().is_some_and(|h| batch.written_handles.contains(h)) || batch.written_rels.contains(&e.rel);
        if !size_changed && !time_changed && !written {
            return Ok(Content::Same);
        }
        let Some(file) = self.open_same(e)? else {
            self.recheck(e);
            return Ok(Content::Unknown);
        };
        if !matches!(placeholder::read_state(&file), Ok(Some(State::Hydrated))) {
            self.recheck(e);
            return Ok(Content::Unknown);
        }
        match lease::open_for_writing(&file) {
            Ok(true) => {
                self.recheck(e);
                return Ok(Content::Waiting);
            }
            Ok(false) => {}
            Err(err) => tracing::warn!("cannot tell whether {} is open for writing ({err}); examined as it is", e.rel.display()),
        }
        if size_changed {
            return Ok(Content::Changed);
        }
        // Same size: only the hash tells an edit from a touch. Against the
        // base's hash only when the file is the base's version.
        let same_version = e.ctag.as_deref().is_none_or(|c| Some(c) == base.ctag.as_deref());
        let (true, Some(expected)) = (same_version, base.quickxor.as_deref()) else {
            return Ok(Content::Changed);
        };
        let before = size_and_time(&file)?;
        if hash(&file)? != expected {
            return Ok(Content::Changed);
        }
        if size_and_time(&file)? != before {
            // Written while it was being read: the hash says nothing.
            self.recheck(e);
            return Ok(Content::Unknown);
        }
        // Only the time changed: a `touch` uploads nothing.
        if let Err(err) = placeholder::write_stamp(&file) {
            tracing::warn!("cannot refresh the stamp of {}: {err}", e.rel.display());
        }
        Ok(Content::Same)
    }

    /// `e`, opened read-only, if it is still the same object.
    fn open_same(&self, e: &Entry) -> Result<Option<File>, ExamineError> {
        let dir = match self.ex.disk.dir(e.dir_rel()) {
            Ok(dir) => dir,
            Err(err) if gone(&err) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let file = match self.ex.disk.open_file(&dir, &e.name) {
            Ok(file) => file,
            Err(err) if gone(&err) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let stat = nix::sys::stat::fstat(&file).map_err(io::Error::from)?;
        let same = match (FileHandle::of(&file).ok(), &e.handle) {
            (Some(a), Some(b)) => &a == b,
            _ => stat.st_dev as u64 == e.dev && stat.st_ino as u64 == e.ino,
        };
        Ok(same.then_some(file))
    }

    /// A placeholder whose size a `truncate(2)` changed gets the cloud's
    /// size and time back. Through the daemon's own descriptor (its opens
    /// are never intercepted, so nothing is filled), never a name a symlink
    /// could redirect, and under the per-inode lock a fill holds for its
    /// whole run, with the state read again under it.
    fn restore(&mut self, e: &Entry, base: &Row) -> Result<(), ExamineError> {
        let Some(file) = self.open_same(e)? else {
            self.recheck(e);
            return Ok(());
        };
        let Some(_guard) = self.ex.locks.try_lock(InodeKey::of(&file)?) else {
            self.recheck(e);
            return Ok(());
        };
        let writable = placeholder::reopen_writable(&file)?;
        drop(file);
        if !matches!(placeholder::read_state(&writable), Ok(Some(State::OnlineOnly))) {
            self.recheck(e);
            return Ok(());
        }
        writable.set_len(base.size)?;
        placeholder::set_mtime(&writable, SystemTime::UNIX_EPOCH + Duration::from_secs(base.mtime.max(0) as u64))?;
        tracing::info!("{} was cut to {} bytes while not downloaded; it has the cloud's size again", e.rel.display(), e.size);
        self.out.restored.push(e.rel.clone());
        Ok(())
    }

    /// Whether a writer holds the new file `e`: `waiting` then, `ready`
    /// otherwise.
    pub(super) fn probe_writer(&mut self, e: &Entry) -> Result<(OutboxState, Option<String>, Option<i64>), ExamineError> {
        let busy = match self.open_same(e)? {
            Some(file) => lease::open_for_writing(&file).unwrap_or_else(|err| {
                tracing::warn!("cannot tell whether {} is open for writing ({err})", e.rel.display());
                false
            }),
            None => false,
        };
        if busy {
            self.recheck(e);
            return Ok((OutboxState::Waiting, Some(OPEN_FOR_WRITING.into()), Some(self.ex.now + RECHECK.as_secs() as i64)));
        }
        Ok((OutboxState::Ready, None, None))
    }
}

fn size_and_time(file: &File) -> io::Result<(i64, i64, i64)> {
    let stat = nix::sys::stat::fstat(file).map_err(io::Error::from)?;
    Ok((stat.st_size as i64, stat.st_mtime as i64, stat.st_mtime_nsec as i64))
}

/// The file's quickXorHash, base64, read once from the start.
fn hash(file: &File) -> io::Result<String> {
    let mut hasher = konedrive_graph::quickxor::QuickXor::new();
    let mut reader = file;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(hasher.finish_base64())
}
