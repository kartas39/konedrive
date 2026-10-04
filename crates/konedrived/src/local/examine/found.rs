//! An item found as an entry: where it stands, and whether its content changed.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};

use konedrive_fs::lease;
use konedrive_fs::placeholder::{self, State};
use konedrive_tree::outbox::{OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::Row;

use super::facts::Expect;
use super::hands::{Opened, Restored};
use super::listing::EntryIx;
use super::{ExamineError, Run};
use crate::local::batch::Batch;
use crate::local::entry::{Entry, StateAttr, Type};
use crate::local::names;

/// What a content check found.
enum Content {
    Changed,
    /// Checked and the base's.
    Same,
    /// Could not tell (busy, not ours to read, an upload of it running).
    Unknown,
    /// Changed or not, a writer has it open.
    Waiting,
}

/// What an item's file says of its content before anything is opened: its marks, its
/// size and its time against the stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Being downloaded or freed: looked at again.
    InTransit,
    /// Not downloaded: the cloud's content. `cut`: a `truncate(2)` changed
    /// its size, which is put back.
    NotDownloaded { cut: bool },
    /// An item id and no state konedrive can read: left alone.
    Damaged,
    /// Being uploaded as it is now.
    BeingSent,
    /// Size and time are the stamp's, and nothing was written: the base's.
    Same,
    /// The size, the time or a write says it may have changed: to be read.
    ReadIt { size_changed: bool },
}

/// The stamp rule (§3.4) for the file `e` of an item whose base size is `base_size`.
/// `being_sent`: a row of the item is running with the content as listed. `written`:
/// the batch saw a write to it.
pub(super) fn verdict(e: &Entry, base_size: u64, being_sent: bool, written: bool) -> Verdict {
    match e.state {
        StateAttr::Known(State::Hydrating | State::Dehydrating) => Verdict::InTransit,
        StateAttr::Known(State::OnlineOnly) => Verdict::NotDownloaded { cut: e.size != base_size },
        StateAttr::Absent | StateAttr::Corrupt => Verdict::Damaged,
        StateAttr::Known(State::Hydrated) if being_sent => Verdict::BeingSent,
        StateAttr::Known(State::Hydrated) => {
            let size_changed = e.stamp.is_some_and(|s| s.size != e.size);
            let time_changed = e.stamp.is_none_or(|s| (s.mtime_sec, s.mtime_nsec) != e.mtime);
            if !size_changed && !time_changed && !written {
                Verdict::Same
            } else {
                Verdict::ReadIt { size_changed }
            }
        }
    }
}

impl Run<'_, '_, '_> {
    /// Item `id` found as entry `ix`, its object: where it is, and its content.
    pub(super) fn found(&mut self, id: &str, ix: EntryIx, batch: &Batch) -> Result<(), ExamineError> {
        let e = &self.listing[ix];
        let Some(base) = self.facts.row(id)? else { return Ok(()) };
        let recorded = self.facts.recorded(id)?;
        if e.handle.is_some() && e.handle != recorded {
            self.outcome.ops.push(OutboxOp::SetHandle { item_id: id.to_owned(), handle: e.handle.clone() });
        }
        if e.ty == Type::File && e.nlink > 1 && matches!(e.state, StateAttr::Known(State::OnlineOnly | State::Hydrating | State::Dehydrating)) {
            self.outcome.out.mark_files.push(e.rel.clone());
        }
        if e.ty == Type::Dir {
            if let Expect::At(was) = self.facts.expected(id)? {
                if was != e.rel {
                    self.outcome.ops.push(OutboxOp::Rebase { from: was, to: e.rel.clone() });
                }
            }
        }
        let content = if e.ty == Type::File { self.content(id, &base, e, batch)? } else { Content::Same };
        let mut d = self.of_item(OutboxKind::Move, id, &base, e, e.ctag.as_deref());
        match content {
            Content::Changed => d.kind = OutboxKind::Update,
            Content::Waiting => {
                d.kind = OutboxKind::Update;
                self.waiting().onto(&mut d);
            }
            Content::Same => d.same_content = true,
            Content::Unknown => {}
        }
        // Renamed to a name OneDrive refuses: blocked until renamed again.
        if e.name != OsStr::new(&base.name) {
            if let Some(refused) = names::refused(&e.name) {
                d.state = OutboxState::Blocked;
                d.reason = Some(refused.reason());
                d.next_try = None;
            }
        }
        // Where the base has it: by the folder's id and the name, or, when
        // this batch did not read the folder above (so its id is not known
        // here), by the path the base places it at.
        let at_base = d.target_name.as_deref() == Some(base.name.as_str())
            && (d.target_parent.as_deref() == base.parent_id.as_deref()
                || (d.target_parent.is_none() && self.facts.located(id)?.is_some_and(|at| at.placed && at.rel == e.rel)));
        // In place, unchanged or unknown, with no row: nothing to record.
        if d.kind == OutboxKind::Move && at_base && self.facts.rows.of_item(id).next().is_none() {
            return Ok(());
        }
        self.outcome.detections.push(d);
        Ok(())
    }

    /// The content check (§3.4) of item `id`'s file `e` against `base`: by
    /// its marks and its stamp ([`verdict`]), and only then by reading it.
    fn content(&mut self, id: &str, base: &Row, e: &Entry, batch: &Batch) -> Result<Content, ExamineError> {
        let being_sent = self.facts.rows.of_item(id).any(|row| row.state == OutboxState::Running && row.snapshot_is(e.snapshot()));
        let written = e.handle.as_ref().is_some_and(|h| batch.written_handles.contains(h)) || batch.written_rels.contains(&e.rel);
        match verdict(e, base.size, being_sent, written) {
            Verdict::InTransit => {
                self.recheck(e);
                Ok(Content::Unknown)
            }
            Verdict::NotDownloaded { cut } => {
                if cut {
                    self.restore(e, base)?;
                }
                Ok(Content::Same)
            }
            Verdict::Damaged => {
                tracing::warn!("{} carries an item id and no state konedrive can read; it is left alone", e.rel.display());
                Ok(Content::Unknown)
            }
            Verdict::BeingSent => Ok(Content::Unknown),
            Verdict::Same => Ok(Content::Same),
            Verdict::ReadIt { size_changed } => self.read(base, e, size_changed),
        }
    }

    /// Reads the downloaded file `e`, which may have changed: probes for a
    /// writer first, and tells an edit from a `touch` by the hash.
    fn read(&mut self, base: &Row, e: &Entry, size_changed: bool) -> Result<Content, ExamineError> {
        let opened = self.hands.open(e);
        let file = match self.entry_io(e, opened)? {
            Some(Opened::Same(file)) => file,
            Some(Opened::Gone) => {
                self.recheck(e);
                return Ok(Content::Unknown);
            }
            None => return Ok(Content::Unknown),
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
        let read = match size_and_time(&file).and_then(|before| Ok((hash(&file)?, size_and_time(&file)? != before))) {
            // A read error in an open file is that file's own (a bad block),
            // as a refusal is.
            Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                self.pass_over(e, &err);
                return Ok(Content::Unknown);
            }
            read => read,
        };
        let Some((hash, written_meanwhile)) = self.entry_io(e, read)? else { return Ok(Content::Unknown) };
        if hash != expected {
            return Ok(Content::Changed);
        }
        if written_meanwhile {
            // Written while it was being read: the hash says nothing.
            self.recheck(e);
            return Ok(Content::Unknown);
        }
        // Only the time changed: a `touch` uploads nothing.
        self.hands.stamp(e, &file);
        Ok(Content::Same)
    }

    /// A placeholder whose size a `truncate(2)` changed gets the cloud's
    /// size and time back ([`Hands::restore`](super::hands::Hands::restore)).
    fn restore(&mut self, e: &Entry, base: &Row) -> Result<(), ExamineError> {
        let restored = self.hands.restore(e, base.size, base.mtime);
        match self.entry_io(e, restored)? {
            Some(Restored::Done) => {
                tracing::info!("{} was cut to {} bytes while not downloaded; it has the cloud's size again", e.rel.display(), e.size);
                self.outcome.out.restored.push(e.rel.clone());
            }
            Some(Restored::Busy | Restored::Gone) => self.recheck(e),
            None => {}
        }
        Ok(())
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
