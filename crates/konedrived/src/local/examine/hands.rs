//! The examination's hands: the one type through which it opens a listed entry and
//! writes to the disk while it is still deciding.
//!
//! The rows, the recorded objects and the skipped list are written at the end, in one
//! transaction. Four things are done on the way, because a decision depends on whether
//! they worked. Each is what the next run would do again, so a batch that fails after
//! one of them converges:
//!
//! - [`strip`](Hands::strip): a copy without its marks is an ordinary file with no row,
//!   found new by the next look at its directory;
//! - [`remove_empty`](Hands::remove_empty): a removed empty copy held nothing;
//! - [`restore`](Hands::restore) and [`stamp`](Hands::stamp): a placeholder's size and
//!   a touched file's stamp are what the next run would write again.
//!
//! Every one of them goes through [`open`](Hands::open): what is read, stripped,
//! restored or removed is the object the run listed, never whatever stands at its name
//! by then.

use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, State};
use konedrive_tree::outbox::Inode;

use super::gone;
use crate::folder::disk::Disk;
use crate::folder::locks::{InodeKey, InodeLocks};
use crate::local::entry::{proc_path, Entry, StateAttr, Type};

/// What [`Hands::open`] found at a listed entry's name.
pub(super) enum Opened {
    Same(File),
    /// Nothing, or another object by now.
    Gone,
}

/// What became of a placeholder a `truncate(2)` had cut.
pub(super) enum Restored {
    Done,
    /// A fill holds it, or it is no placeholder any more: looked at again.
    Busy,
    Gone,
}

pub(super) struct Hands<'e> {
    pub(super) disk: &'e Disk,
    /// The folder's per-inode locks, which a fill holds for its whole run.
    pub(super) locks: &'e InodeLocks,
}

impl Hands<'_> {
    /// The object `e` was listed as, opened read-only. The name is opened
    /// and the descriptor compared with what was listed (the file handle, or
    /// device and inode where there is none), so everything done through it
    /// afterwards is done to the object the run decided about. A name that
    /// holds another object by now is [`Opened::Gone`], like one that holds
    /// nothing. An error is the caller's to judge
    /// ([`Run::entry_io`](super::Run::entry_io)).
    pub(super) fn open(&self, e: &Entry) -> io::Result<Opened> {
        let open = |dir| if e.ty == Type::Dir { self.disk.open_subdir(&dir, &e.name) } else { self.disk.open_file(&dir, &e.name) };
        let opened = self.disk.dir(e.dir_rel()).and_then(open).and_then(|file| {
            let stat = nix::sys::stat::fstat(&file).map_err(io::Error::from)?;
            Ok((file, stat))
        });
        let (file, stat) = match opened {
            Ok(opened) => opened,
            Err(err) if gone(&err) => return Ok(Opened::Gone),
            Err(err) => return Err(err),
        };
        let there = Inode { dev: stat.st_dev, ino: stat.st_ino, handle: FileHandle::of(&file).ok() };
        Ok(if there.same_object(&e.inode()) { Opened::Same(file) } else { Opened::Gone })
    }

    /// Takes konedrive's marks off the object `e` was listed as, the item
    /// id first ([`placeholder::strip`]): `Opened::Gone` when its name holds
    /// another object by now — the item's own file, renamed over a copy
    /// while the run was under way, is left as it is.
    ///
    /// Immediate: whether the copy gets a row, and anything inside it,
    /// depends on it. A crash after it leaves an ordinary file with no row,
    /// which the next look at its directory finds new.
    pub(super) fn strip(&self, e: &Entry) -> io::Result<Opened> {
        let opened = self.open(e)?;
        if let Opened::Same(object) = &opened {
            placeholder::strip(object)?;
        }
        Ok(opened)
    }

    /// Removes a copy marked as not downloaded that holds no data — a
    /// regular file with one name and no data region (`SEEK_DATA` finds
    /// none): a placeholder of nothing, restored from a snapshot or copied
    /// beside its original — and says whether it did. One that holds any
    /// data, has another name, is being filled, cannot be opened or checked,
    /// or whose name holds another object by now is kept (`false`): no file
    /// with data is deleted on the strength of an attribute. Checked through
    /// the descriptor [`open`](Self::open) proved to be the listed object,
    /// under the lock a fill holds, and the name against that descriptor
    /// right before the unlink. Only for an entry that is certainly not its
    /// item's file.
    ///
    /// Immediate: a copy that stays must be listed. What went held nothing.
    /// The unlink itself is by name: a rename over that name between the
    /// last check and `unlinkat` removes another file, the window every
    /// removal by the daemon has (`docs/limitations/F53.md`).
    pub(super) fn remove_empty(&self, e: &Entry) -> bool {
        if e.state != StateAttr::Known(State::OnlineOnly) {
            return false;
        }
        let Ok(Opened::Same(file)) = self.open(e) else { return false };
        let Some(_guard) = InodeKey::of(&file).ok().and_then(|key| self.locks.try_lock(key)) else { return false };
        let Ok(dir) = self.disk.dir(e.dir_rel()) else { return false };
        let empty = |file: &File| {
            file.metadata().is_ok_and(|m| m.file_type().is_file() && m.nlink() == 1 && (m.dev(), m.ino()) == (e.dev, e.ino))
                && matches!(placeholder::read_state(file), Ok(Some(State::OnlineOnly)))
                // One hole from its start to its end. Never the block
                // count: the marks alone take a block on ext4. Where holes
                // are not reported the whole file reads as data: kept.
                && nix::unistd::lseek(file.as_fd(), 0, nix::unistd::Whence::SeekData) == Err(nix::errno::Errno::ENXIO)
        };
        let named = |dir: &File| std::fs::symlink_metadata(proc_path(dir).join(&e.name)).is_ok_and(|m| (m.dev(), m.ino()) == (e.dev, e.ino));
        if !empty(&file) || !named(&dir) || !empty(&file) {
            return false;
        }
        match self.disk.remove(&dir, &e.name, false) {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!("cannot remove the empty copy {}: {err}", e.rel.display());
                false
            }
        }
    }

    /// Gives a placeholder whose size a `truncate(2)` changed the cloud's
    /// `size` and `mtime` back. Through the daemon's own descriptor (its
    /// opens are never intercepted, so nothing is filled), never a name a
    /// symlink could redirect, and under the per-inode lock a fill holds for
    /// its whole run, with the state read again under it.
    ///
    /// Immediate: the next run would write the same.
    pub(super) fn restore(&self, e: &Entry, size: u64, mtime: i64) -> io::Result<Restored> {
        let Opened::Same(file) = self.open(e)? else { return Ok(Restored::Gone) };
        let Some(_guard) = self.locks.try_lock(InodeKey::of(&file)?) else { return Ok(Restored::Busy) };
        let writable = placeholder::reopen_writable(&file)?;
        drop(file);
        if !matches!(placeholder::read_state(&writable), Ok(Some(State::OnlineOnly))) {
            return Ok(Restored::Busy);
        }
        writable.set_len(size)?;
        placeholder::set_mtime(&writable, SystemTime::UNIX_EPOCH + Duration::from_secs(mtime.max(0) as u64))?;
        Ok(Restored::Done)
    }

    /// Renews the stamp of a downloaded file whose time alone changed (a
    /// `touch`), through the descriptor its content was just read by.
    ///
    /// Immediate: the next run would read the file again and write the same.
    pub(super) fn stamp(&self, e: &Entry, file: &File) {
        if let Err(err) = placeholder::write_stamp(file) {
            tracing::warn!("cannot refresh the stamp of {}: {err}", e.rel.display());
        }
    }
}
