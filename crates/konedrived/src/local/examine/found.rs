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
use crate::folder::locks::InodeKey;
use konedrive_tree::outbox::{Inode, OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::Row;

use super::detect::Readiness;
use super::{Content, denied, ExamineError, Expect, gone, Run};

impl Run<'_, '_> {
    /// Item `id` found as entry `i`, its object ([`resolve`](Self::resolve)):
    /// where it is, and its content.
    pub(super) fn found(&mut self, id: &str, i: usize, batch: &Batch) -> Result<(), ExamineError> {
        let e = self.entries[i].clone();
        let Some(base) = self.base_row(id)? else { return Ok(()) };
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
        let mut d = self.of_item(OutboxKind::Move, id, &base, &e, e.ctag.as_deref());
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
            && (d.target_parent.as_deref() == base.parent_id.as_deref() || (d.target_parent.is_none() && self.located(id)?.is_some_and(|at| at.placed && at.rel == e.rel)));
        // In place, unchanged or unknown, with no row: nothing to record.
        if d.kind == OutboxKind::Move && at_base && self.rows.of_item(id).next().is_none() {
            return Ok(());
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
        if self.rows.of_item(id).any(|row| row.state == OutboxState::Running && row.snapshot_is(e.snapshot())) {
            // Being uploaded as it is now.
            return Ok(Content::Unknown);
        }
        let size_changed = e.stamp.is_some_and(|s| s.size != e.size);
        let time_changed = e.stamp.is_none_or(|s| (s.mtime_sec, s.mtime_nsec) != e.mtime);
        let written = e.handle.as_ref().is_some_and(|h| batch.written_handles.contains(h)) || batch.written_rels.contains(&e.rel);
        if !size_changed && !time_changed && !written {
            return Ok(Content::Same);
        }
        let file = match self.open_same(e)? {
            Opened::Same(file) => file,
            Opened::Gone => {
                self.recheck(e);
                return Ok(Content::Unknown);
            }
            Opened::Passed => return Ok(Content::Unknown),
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
        if let Err(err) = placeholder::write_stamp(&file) {
            tracing::warn!("cannot refresh the stamp of {}: {err}", e.rel.display());
        }
        Ok(Content::Same)
    }

    /// The object `e` was listed as, opened read-only: the way a listed
    /// entry is opened (the removal of an empty copy alone makes its own
    /// checks, `remove_empty`). The name is opened and the descriptor compared with
    /// what was listed, so everything done through it afterwards — a read, a
    /// strip, a restore — is done to the object the run decided about. A
    /// name that holds another object by now is [`Opened::Gone`], like one
    /// that holds nothing. By the policy of [`entry_io`](Self::entry_io).
    pub(super) fn open_same(&mut self, e: &Entry) -> Result<Opened, ExamineError> {
        let disk = self.ex.disk;
        let open = |dir| if e.ty == Type::Dir { disk.open_subdir(&dir, &e.name) } else { disk.open_file(&dir, &e.name) };
        let opened = disk.dir(e.dir_rel()).and_then(open).and_then(|file| {
            let stat = nix::sys::stat::fstat(&file).map_err(io::Error::from)?;
            Ok((file, stat))
        });
        let (file, stat) = match opened {
            Ok(opened) => opened,
            Err(err) if gone(&err) => return Ok(Opened::Gone),
            Err(err) if denied(&err) => {
                self.pass_over(e, &err);
                return Ok(Opened::Passed);
            }
            Err(err) => return Err(err.into()),
        };
        let there = Inode { dev: stat.st_dev, ino: stat.st_ino, handle: FileHandle::of(&file).ok() };
        Ok(if there.same_object(&e.inode()) { Opened::Same(file) } else { Opened::Gone })
    }

    /// A placeholder whose size a `truncate(2)` changed gets the cloud's
    /// size and time back. Through the daemon's own descriptor (its opens
    /// are never intercepted, so nothing is filled), never a name a symlink
    /// could redirect, and under the per-inode lock a fill holds for its
    /// whole run, with the state read again under it.
    fn restore(&mut self, e: &Entry, base: &Row) -> Result<(), ExamineError> {
        let file = match self.open_same(e)? {
            Opened::Same(file) => file,
            Opened::Gone => {
                self.recheck(e);
                return Ok(());
            }
            Opened::Passed => return Ok(()),
        };
        let Some(key) = self.entry_io(e, InodeKey::of(&file))? else { return Ok(()) };
        let Some(_guard) = self.ex.locks.try_lock(key) else {
            self.recheck(e);
            return Ok(());
        };
        let Some(writable) = self.entry_io(e, placeholder::reopen_writable(&file))? else { return Ok(()) };
        drop(file);
        if !matches!(placeholder::read_state(&writable), Ok(Some(State::OnlineOnly))) {
            self.recheck(e);
            return Ok(());
        }
        let restored = writable
            .set_len(base.size)
            .and_then(|()| placeholder::set_mtime(&writable, SystemTime::UNIX_EPOCH + Duration::from_secs(base.mtime.max(0) as u64)));
        if self.entry_io(e, restored)?.is_none() {
            return Ok(());
        }
        tracing::info!("{} was cut to {} bytes while not downloaded; it has the cloud's size again", e.rel.display(), e.size);
        self.out.restored.push(e.rel.clone());
        Ok(())
    }

    /// Whether a writer holds the new file `e`. `None` for a file that gets
    /// no row in this run: one this daemon is refused to open is passed
    /// over; one that went since it was listed, or whose name holds another
    /// object by now, is not there to upload, and its name is looked at
    /// again.
    pub(super) fn probe_writer(&mut self, e: &Entry) -> Result<Option<Readiness>, ExamineError> {
        let file = match self.open_same(e)? {
            Opened::Same(file) => file,
            Opened::Gone => {
                self.recheck(e);
                return Ok(None);
            }
            Opened::Passed => return Ok(None),
        };
        let busy = lease::open_for_writing(&file).unwrap_or_else(|err| {
            tracing::warn!("cannot tell whether {} is open for writing ({err})", e.rel.display());
            false
        });
        if busy {
            self.recheck(e);
            return Ok(Some(self.waiting()));
        }
        Ok(Some(Readiness::Ready))
    }
}

/// What [`Run::open_same`] found.
pub(super) enum Opened {
    Same(File),
    /// Gone, or another object by now.
    Gone,
    /// Refused to this daemon: passed over.
    Passed,
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
