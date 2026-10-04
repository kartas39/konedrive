use std::ffi::OsStr;
use std::fs::File;
use std::path::Path;
use std::time::{Duration, SystemTime};

use konedrive_fs::placeholder::{self, read_state, State};

use crate::status::activity::Kind as EventKind;
use konedrive_tree::Row;
use super::{ApplyError, Materializer, Replacement, Run};

impl Materializer {
    /// A file already in place, and what its content needs:
    /// a placeholder takes the new size, time and cTag in place; a downloaded
    /// file of another version is queued for replacement (§7.3), or moved out
    /// of the way first when it holds local work (§9.3: rescued, or kept
    /// beside it in a read-write folder); a file being filled or freed up
    /// right now is left for the next cycle.
    ///
    /// A placeholder's content is told by its cTag and size alone, never by
    /// its time: every write of a fill moves the
    /// time to now, and a fill that stopped part-way — a restart, a failure —
    /// leaves the time wrong over a checkpoint worth keeping. Taken for a
    /// new version, the whole partial download was punched away at the first
    /// Full reconcile after a restart. Only the time is put back then.
    pub(super) fn check_file(&self, dir: &File, name: &OsStr, row: &Row, rel: &Path, run: &mut Run) -> Result<(), ApplyError> {
        use std::os::unix::fs::MetadataExt;
        let file = self.disk.open_file(dir, name)?;
        let local_ctag = placeholder::read_ctag(&file).ok().flatten();
        match read_state(&file) {
            Ok(Some(State::OnlineOnly)) => {
                let meta = file.metadata()?;
                let same_content = local_ctag.as_deref() == row.ctag.as_deref() && meta.len() == row.size;
                if !same_content {
                    if self.update_placeholder(file, row, run)? {
                        run.note(EventKind::Updated, rel, None);
                    } else {
                        self.content_waits(&row.id, run);
                    }
                } else if meta.mtime() != row.mtime {
                    self.put_time_back(file, row, run)?;
                }
                Ok(())
            }
            Ok(Some(State::Hydrated)) => {
                if local_ctag.is_some() && local_ctag.as_deref() == row.ctag.as_deref() {
                    return Ok(());
                }
                if self.changed_since_the_cycle_began(row, run)? {
                    return Ok(());
                }
                // Local work in it (edit × edit, §6), or a version OneDrive
                // may have lost: not replaced, but moved out of the way.
                if self.local_work(&file) || self.keeps_every_download() {
                    drop(file);
                    self.out_of_the_way(dir, name, rel, run)?;
                    self.create(dir, row, rel, run)?;
                    run.note(EventKind::Updated, rel, None);
                    return Ok(());
                }
                if let Some(ctag) = &row.ctag {
                    run.out.pending.replacements.push(Replacement { id: row.id.clone(), rel: rel.to_path_buf(), ctag: ctag.clone(), size: row.size });
                    self.content_waits(&row.id, run);
                }
                Ok(())
            }
            Ok(Some(State::Hydrating | State::Dehydrating)) => {
                run.out.counts.deferred += 1;
                self.content_waits(&row.id, run);
                Ok(())
            }
            // Ours by its id, in no state anyone can vouch for.
            Ok(None) | Err(_) => {
                let work = self.local_work(&file);
                drop(file);
                if work {
                    self.out_of_the_way(dir, name, rel, run)?;
                } else {
                    self.disk.remove(dir, name, false)?;
                }
                self.create(dir, row, rel, run)?;
                run.note(EventKind::Updated, rel, None);
                Ok(())
            }
        }
    }

    /// A placeholder takes its new size, time and cTag, in place (same inode).
    /// Under the per-inode lock, which a fill holds for its whole run: if one is
    /// running, this waits for the next cycle rather than for the download.
    /// Whether it was updated now.
    fn update_placeholder(&self, file: File, row: &Row, run: &mut Run) -> Result<bool, ApplyError> {
        let key = crate::folder::locks::InodeKey::of(&file)?;
        let Some(_guard) = self.locks.try_lock(key) else {
            run.out.counts.deferred += 1;
            return Ok(false);
        };
        let writable = placeholder::reopen_writable(&file)?;
        drop(file);
        // Looked at again under the lock: a fill may have finished meanwhile.
        if !matches!(read_state(&writable), Ok(Some(State::OnlineOnly))) {
            run.out.counts.deferred += 1;
            return Ok(false);
        }
        // A checkpoint of another version goes, with its bytes; one of this
        // very version stays (A-I1). An online-only file carries no ignore
        // mark (the helper marks only what reads hydrated), so the punch
        // needs no ClearIgnore (question, answered).
        let stale = placeholder::read_progress(&writable)?.is_some_and(|p| Some(p.ctag.as_str()) != row.ctag.as_deref());
        if stale {
            placeholder::remove_progress(&writable)?;
            placeholder::punch_all(&writable)?;
        }
        writable.set_len(row.size)?;
        if let Some(ctag) = &row.ctag {
            placeholder::write_ctag(&writable, ctag)?;
        }
        placeholder::set_mtime(&writable, cloud_time(row))?;
        if row.size == 0 {
            // Nothing left to fetch: an empty file is a downloaded one.
            placeholder::write_stamp(&writable)?;
            placeholder::write_state(&writable, State::Hydrated)?;
        }
        writable.sync_all()?;
        run.out.counts.updated += 1;
        Ok(true)
    }

    /// A placeholder of the tree's very version whose time is not the
    /// cloud's — a fill wrote into it and stopped — gets the cloud's time
    /// back, and keeps its checkpoint and the bytes it counts (A-I1). KIO
    /// checks a thumbnail against the file's time, so a wrong one also had
    /// Dolphin open the file to make its own thumbnail: a download. Under
    /// the per-inode lock, as [`Self::update_placeholder`]: a fill running
    /// now leaves it for the next cycle. The owner sets a time through a
    /// read-only descriptor, lock or not.
    fn put_time_back(&self, file: File, row: &Row, run: &mut Run) -> Result<(), ApplyError> {
        let key = crate::folder::locks::InodeKey::of(&file)?;
        let Some(_guard) = self.locks.try_lock(key) else {
            run.out.counts.deferred += 1;
            return Ok(());
        };
        if !matches!(read_state(&file), Ok(Some(State::OnlineOnly))) {
            run.out.counts.deferred += 1;
            return Ok(());
        }
        placeholder::set_mtime(&file, cloud_time(row))?;
        Ok(())
    }
}

/// The time the cloud gives an item, as a placeholder carries it.
pub(super) fn cloud_time(row: &Row) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(row.mtime.max(0) as u64)
}
