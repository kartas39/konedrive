//! A file's content going up (§4.1, §4.3, §4.8): a `create` or an `update`.
//!
//! The file is read only while downloaded or unmanaged (WR1), under its
//! inode lock (a free-up waits) and a read lease (a writer's open waits the
//! milliseconds of one read), and compared with the snapshot after every
//! read. Up to [`Limits::small_max`] it goes in one request; above, in
//! fragments, the session persisted before the first byte and after each.
//!
//! [`Limits::small_max`]: super::Limits::small_max

use std::fs::File;
use std::io;
use std::sync::Arc;

use konedrive_fs::lease;
use konedrive_fs::placeholder::{self, State};

use super::engine::{Engine, Fail, Outcome};
use super::local::{self, Found, Read, Snap, SYNC_UPLOADING};
use super::steps::{answer_row, blocking, commit_row, copy, follow_cloud, local_name, locate, parent_of, taken, temporary, upload_as_new, wanted_name, Ours, Taken};
use super::{kind, reason, Fault};
use crate::drive::{ChunkOutcome, DriveError, DriveItem, ItemChange, UploadTarget, WriteError};
use crate::quickxor::QuickXor;
use crate::sync::disk::Disk;
use crate::sync::local::examine::OPEN_FOR_WRITING;
use crate::sync::local::{names, QUIET, RECHECK};
use crate::sync::InodeKey;
use crate::tree::outbox::{Base, OutboxKind, OutboxRow};

/// A new file's row whose upload OneDrive holds with other content, and
/// could not be deleted yet: `hash-mismatch:<item id>`.
const BAD_ITEM: &str = "hash-mismatch:";

pub(super) async fn run(e: &Arc<Engine>, disk: &Disk, row: OutboxRow) -> Result<Outcome, Fail> {
    let local = local_name(&row)?;
    let Some(found) = locate(e, disk, &row)?.filter(|f| !f.is_dir) else { return Ok(Outcome::later(reason::NOT_FOUND, RECHECK)) };
    match found.state() {
        Ok(None | Some(State::Hydrated)) => {}
        Ok(Some(_)) => return Ok(Outcome::wait(reason::NOT_LOCAL, RECHECK)),
        Err(err) => return Ok(Outcome::blocked(err.to_string())),
    }
    let file = Arc::new(found.open()?);
    // Before the snapshot: the mark changes no size or time (§9).
    local::set_sync_of(&file, SYNC_UPLOADING);
    if lease::open_for_writing(&file)? {
        return Ok(Outcome::wait(OPEN_FOR_WRITING, RECHECK));
    }
    let snap = Snap::of(&file)?;
    if snap.size > names::MAX_FILE_SIZE {
        return Ok(Outcome::blocked(names::Refused::TooLarge.as_str()));
    }
    // The snapshot and the session belong together: a session opened for
    // other content goes in the same transaction, before any request.
    let session = row.session_url.clone().filter(|_| row.snapshot.as_deref() == Some(snap.text().as_str()));
    if let Some(stale) = e.store().with(|s| s.outbox_take_snapshot(row.seq, &snap.text()))? {
        if let Err(err) = e.cfg.drive.cancel_upload(&stale).await {
            tracing::debug!("an upload session opened for other content was not cancelled: {err}");
        }
    }
    e.upload_progress(row.seq, 0, snap.size);
    let Some(parent) = parent_of(e, disk, &row)? else { return Ok(Outcome::later(reason::PARENT, RECHECK)) };
    let name = wanted_name(&row, &local);
    let job = Job { e, disk, row: &row, found: &found, file: &file, snap, parent: &parent, name: &name, session };
    match row.kind {
        OutboxKind::Create => job.create().await,
        _ => job.update().await,
    }
}

/// What a send came back with: the content's hash when it was computed on
/// the way, and OneDrive's answer.
struct Sent {
    hash: Option<String>,
    answer: Result<DriveItem, WriteError>,
}

struct Job<'a> {
    e: &'a Arc<Engine>,
    disk: &'a Disk,
    row: &'a OutboxRow,
    found: &'a Found,
    file: &'a Arc<File>,
    snap: Snap,
    parent: &'a str,
    name: &'a str,
    /// A session opened for exactly this content (the same snapshot), to
    /// resume.
    session: Option<String>,
}

impl Job<'_> {
    /// A new file's upload that OneDrive holds with other content:
    /// that item goes before the file is sent again — or, holding this
    /// content after all, is this file's.
    async fn clear_bad_item(&self) -> Result<Option<Outcome>, Fail> {
        let Some(bad) = self.row.reason.as_deref().and_then(|r| r.strip_prefix(BAD_ITEM)) else { return Ok(None) };
        let item = match self.e.cfg.drive.item(bad).await {
            Ok(item) => item,
            Err(DriveError::NotFound) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if item.quick_xor_hash() == Some(self.hash(None).await?.as_str()) {
            return self.commit(item).await.map(Some);
        }
        let guard = item.e_tag.clone().or(item.c_tag.clone()).unwrap_or_default();
        match self.e.cfg.drive.delete_item(bad, &guard).await {
            Ok(()) | Err(WriteError::NotFound) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    async fn create(&self) -> Result<Outcome, Fail> {
        if let Some(outcome) = self.clear_bad_item().await? {
            return Ok(outcome);
        }
        let sent = self.send(UploadTarget::New { parent_id: self.parent, name: self.name }, None).await?;
        match sent.answer {
            Ok(item) => self.finish(item, sent.hash).await,
            Err(WriteError::NameExists) => {
                let hash = self.hash(sent.hash).await?;
                match taken(self.e, self.row, self.parent, self.name, Ours::File(&hash)).await? {
                    Taken::Free => Ok(Outcome::again()),
                    Taken::Temporary(swap) => temporary(self.e, self.row, self.parent, &swap),
                    // The same content is there: its own earlier request, or
                    // create/create with equal files (§6). Nothing is sent.
                    Taken::Adopt(item) => self.commit(*item).await,
                    Taken::Copy => copy(self.e, self.disk, self.row, self.found, self.parent, None).await,
                }
            }
            Err(WriteError::NotFound) => {
                self.e.cfg.host.cycle_wanted();
                Ok(Outcome::backoff(reason::PARENT))
            }
            Err(other) => Err(other.into()),
        }
    }

    async fn update(&self) -> Result<Outcome, Fail> {
        let row = self.row;
        let Some(id) = row.item_id.as_deref() else { return Ok(Outcome::blocked("no-item")) };
        let base = row.base.clone().unwrap_or_default();
        // F55 (4): a row queued against another version than the base's
        // carries that version's cTag, and no eTag.
        let Some(mut guard) = base.etag.clone().or_else(|| base.ctag.clone()) else { return Ok(Outcome::blocked("no-guard")) };
        let new_name = (Some(self.name) != base.name.as_deref()).then_some(self.name);
        let new_parent = (Some(self.parent) != base.parent.as_deref()).then_some(self.parent);
        if new_name.is_some() || new_parent.is_some() {
            // Moved as well: the move first, then the content (§3.5).
            let change = ItemChange { name: new_name, parent_id: new_parent, modified: None };
            match self.e.cfg.drive.update_item(id, &guard, &change).await {
                Ok(item) => guard = item.e_tag.unwrap_or(guard),
                Err(WriteError::NameExists) => match taken(self.e, row, self.parent, self.name, Ours::Item(id)).await? {
                    Taken::Free => return Ok(Outcome::again()),
                    Taken::Temporary(swap) => return temporary(self.e, row, self.parent, &swap),
                    Taken::Adopt(item) => guard = item.e_tag.unwrap_or(guard),
                    Taken::Copy => return copy(self.e, self.disk, row, self.found, self.parent, None).await,
                },
                Err(WriteError::Changed) => match self.landed(id, &base).await? {
                    // The move went through before (a replay: the temporary
                    // name of a swap, I1): the content follows it.
                    Some(fresh) => guard = fresh,
                    None => return self.changed(None).await,
                },
                Err(WriteError::NotFound) => return upload_as_new(self.e, row, self.found, self.parent, id).await,
                Err(other) => return Err(other.into()),
            }
        }
        let sent = self.send(UploadTarget::Existing { id, if_match: &guard }, Some((id, &guard))).await?;
        match sent.answer {
            Ok(item) => self.finish(item, sent.hash).await,
            Err(WriteError::Changed) => self.changed(sent.hash).await,
            // Deleted in OneDrive while changed here: local wins (§6).
            Err(WriteError::NotFound) => upload_as_new(self.e, row, self.found, self.parent, id).await,
            Err(other) => Err(other.into()),
        }
    }

    /// A `412` on the move before the content: has OneDrive the item where
    /// this row takes it already, its content still the version the change
    /// was made against? Then the move landed before (§5), and the content
    /// goes against the fresh eTag.
    async fn landed(&self, id: &str, base: &Base) -> Result<Option<String>, Fail> {
        let remote = match self.e.cfg.drive.item(id).await {
            Ok(remote) => remote,
            Err(DriveError::NotFound) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let there = remote.parent_reference.as_ref().and_then(|p| p.id.as_deref()) == Some(self.parent) && remote.name.as_deref() == Some(self.name);
        Ok(if there && remote.c_tag.is_some() && remote.c_tag == base.ctag { remote.e_tag } else { None })
    }

    /// `412` for a change (§3.6, §6): read the item again. Its content is
    /// this file's → adopted (its own earlier request, §5). Its content is
    /// the version this change was made against → again with the fresh eTag;
    /// a rename made there first stands. Otherwise both changed: a copy.
    async fn changed(&self, hash: Option<String>) -> Result<Outcome, Fail> {
        let row = self.row;
        let id = row.item_id.as_deref().unwrap_or_default();
        let remote = match self.e.cfg.drive.item(id).await {
            Ok(remote) => remote,
            Err(DriveError::NotFound) => return upload_as_new(self.e, row, self.found, self.parent, id).await,
            Err(err) => return Err(err.into()),
        };
        let hash = self.hash(hash).await?;
        let base = row.base.clone().unwrap_or_default();
        let remote_parent = remote.parent_reference.as_ref().and_then(|p| p.id.clone());
        let remote_name = remote.name.clone().unwrap_or_default();
        let same_content = remote.quick_xor_hash() == Some(hash.as_str());
        if same_content && remote_parent.as_deref() == Some(self.parent) && remote_name == self.name {
            return self.commit(remote).await;
        }
        if same_content || (remote.c_tag.is_some() && remote.c_tag == base.ctag) {
            let moved_there = remote_parent != base.parent || Some(remote_name.as_str()) != base.name.as_deref();
            let _tree = self.e.cfg.tree_lock.lock().await;
            let followed = if moved_there { follow_cloud(self.e, self.disk, self.found, &remote)? } else { None };
            let fresh = Base { etag: remote.e_tag.clone(), ctag: base.ctag.clone(), parent: remote_parent.clone(), name: Some(remote_name.clone()) };
            self.e.store().with(|s| {
                s.outbox_amend(row.seq, |next| {
                    if let Some(to_rel) = followed {
                        next.rel = to_rel;
                        next.target_parent = remote_parent;
                        next.target_name = Some(remote_name);
                    }
                    next.base = Some(fresh);
                    next.session_url = None;
                    next.session_expires = None;
                    next.session_next = None;
                })
            })?;
            return Ok(Outcome::again());
        }
        copy(self.e, self.disk, row, self.found, self.parent, Some(id)).await
    }

    async fn send(&self, target: UploadTarget<'_>, last_check: Option<(&str, &str)>) -> Result<Sent, Fail> {
        if self.snap.size <= self.e.cfg.limits.small_max {
            self.send_small(target).await
        } else {
            self.send_large(target, last_check).await
        }
    }

    /// `len` bytes at `offset`, under the inode lock and a read lease.
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, Fail> {
        let _inode = self.e.cfg.locks.lock(InodeKey::of(self.file)?).await;
        let (file, snap) = (Arc::clone(self.file), self.snap);
        match blocking(move || local::read(&file, offset, len, snap)).await? {
            Read::Bytes(bytes) => Ok(bytes),
            Read::Busy => Err(Fail::Now(Outcome::wait(OPEN_FOR_WRITING, RECHECK))),
            Read::NotLocal => Err(Fail::Now(Outcome::wait(reason::NOT_LOCAL, RECHECK))),
            Read::Changed => Err(Fail::Now(Outcome::wait(reason::CHANGED, QUIET))),
        }
    }

    /// Feeds the first `upto` bytes to `hasher`, a fragment at a time.
    async fn hash_prefix(&self, hasher: &mut QuickXor, upto: u64) -> Result<(), Fail> {
        let mut at = 0;
        while at < upto {
            let len = (upto - at).min(self.e.cfg.limits.chunk.max(1));
            hasher.update(&self.read(at, len as usize).await?);
            at += len;
        }
        Ok(())
    }

    /// The content's quickXorHash: known, or read for it.
    async fn hash(&self, known: Option<String>) -> Result<String, Fail> {
        if let Some(hash) = known {
            return Ok(hash);
        }
        let mut hasher = QuickXor::new();
        self.hash_prefix(&mut hasher, self.snap.size).await?;
        Ok(hasher.finish_base64())
    }

    async fn send_small(&self, target: UploadTarget<'_>) -> Result<Sent, Fail> {
        let bytes = self.read(0, self.snap.size as usize).await?;
        let mut hasher = QuickXor::new();
        hasher.update(&bytes);
        let answer = self.e.cfg.drive.upload_small(target, bytes, self.snap.sec).await;
        if answer.is_ok() {
            self.e.upload_progress(self.row.seq, self.snap.size, self.snap.size);
            self.e.fault(Fault::AfterSend)?;
        }
        Ok(Sent { hash: Some(hasher.finish_base64()), answer })
    }

    fn forget_session(&self) -> Result<(), Fail> {
        Ok(self.e.store().with(|s| s.outbox_set_session(self.row.seq, None, None, None))?)
    }

    /// `DELETE <uploadUrl>`: the content changed while it went up (§4.3).
    async fn abandon(&self, url: &str) -> Result<(), Fail> {
        if let Err(err) = self.e.cfg.drive.cancel_upload(url).await {
            tracing::debug!("an abandoned upload session was not cancelled: {err}");
        }
        self.forget_session()
    }

    /// The item the target names, as OneDrive has it now.
    async fn fetch(&self, target: &UploadTarget<'_>) -> Result<Option<DriveItem>, Fail> {
        let fetched = match *target {
            UploadTarget::New { parent_id, name } => self.e.cfg.drive.child(parent_id, name).await,
            UploadTarget::Existing { id, .. } => self.e.cfg.drive.item(id).await,
        };
        match fetched {
            Ok(item) => Ok(Some(item)),
            Err(DriveError::NotFound) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// An ended session (`404`): it completed without the answer reaching
    /// us, or it expired. The item holding this content is adopted (§5);
    /// `None` means start again.
    async fn ended(&self, target: &UploadTarget<'_>) -> Result<Option<Sent>, Fail> {
        let hash = self.hash(None).await?;
        if let Some(item) = self.fetch(target).await? {
            if item.quick_xor_hash() == Some(hash.as_str()) {
                return Ok(Some(Sent { hash: Some(hash), answer: Ok(item) }));
            }
        }
        self.forget_session()?;
        Ok(None)
    }

    /// §4.8: a session in fragments, resumed from where the server stands
    /// when the file is still the snapshot the row holds.
    async fn send_large(&self, target: UploadTarget<'_>, last_check: Option<(&str, &str)>) -> Result<Sent, Fail> {
        let (e, seq, size) = (self.e, self.row.seq, self.snap.size);
        let drive = &e.cfg.drive;
        let mut resumed: Option<(String, u64)> = None;
        // Only a session opened for this very content is resumed.
        if let Some(url) = self.session.clone() {
            match drive.upload_status(&url).await {
                Ok(progress) => resumed = Some((url, progress.next)),
                Err(WriteError::SessionGone) => {
                    if let Some(sent) = self.ended(&target).await? {
                        return Ok(sent);
                    }
                }
                Err(other) => return Err(other.into()),
            }
        }
        let mut restarts = 0;
        loop {
            let (url, mut next) = match resumed.take() {
                Some(session) => session,
                None => {
                    let opened = match drive.create_upload_session(target, size, self.snap.sec).await {
                        Ok(opened) => opened,
                        Err(err) => return Ok(Sent { hash: None, answer: Err(err) }),
                    };
                    // A crash here leaves an orphan session, which expires.
                    e.fault(Fault::SessionNotPersisted)?;
                    e.store().with(|s| s.outbox_set_session(seq, Some(&opened.url), opened.expires, Some(0)))?;
                    (opened.url, 0)
                }
            };
            let mut hasher = QuickXor::new();
            self.hash_prefix(&mut hasher, next).await?;
            let mut fragments = 0;
            loop {
                // Paused (`docs/design/writes.md` §11): the session stays, and is resumed
                // after the pause (the outbox on the bus).
                if super::paused(e.store()).is_some() {
                    return Err(Fail::Now(Outcome::wait("paused", std::time::Duration::ZERO)));
                }
                // The write gate, between fragments too: the session stays.
                if let Err(why) = e.cfg.host.may_write() {
                    return Err(Fail::Now(Outcome::wait(&format!("not allowed now: {why}"), std::time::Duration::ZERO)));
                }
                let len = (size - next).min(e.cfg.limits.chunk);
                if next + len >= size {
                    // Before the last fragment: a writer, the snapshot, and the
                    // item in OneDrive once more (§4.8 step 4).
                    if lease::open_for_writing(self.file)? {
                        return Err(Fail::Now(Outcome::wait(OPEN_FOR_WRITING, RECHECK)));
                    }
                    if Snap::of(self.file)? != self.snap {
                        self.abandon(&url).await?;
                        return Err(Fail::Now(Outcome::wait(reason::CHANGED, QUIET)));
                    }
                    if let Some((id, guard)) = last_check {
                        match drive.item(id).await {
                            Ok(item) if item.e_tag.as_deref() != Some(guard) && item.c_tag.as_deref() != Some(guard) => {
                                self.abandon(&url).await?;
                                return Ok(Sent { hash: None, answer: Err(WriteError::Changed) });
                            }
                            Ok(_) => {}
                            Err(DriveError::NotFound) => {
                                self.abandon(&url).await?;
                                return Ok(Sent { hash: None, answer: Err(WriteError::NotFound) });
                            }
                            Err(err) => return Err(err.into()),
                        }
                    }
                }
                let bytes = match self.read(next, len as usize).await {
                    Ok(bytes) => bytes,
                    Err(Fail::Now(outcome)) => {
                        if matches!(&outcome, Outcome::Again { reason: Some(r), .. } if r == reason::CHANGED) {
                            self.abandon(&url).await?;
                        }
                        return Err(Fail::Now(outcome));
                    }
                    Err(other) => return Err(other),
                };
                hasher.update(&bytes);
                match drive.upload_chunk(&url, next, size, bytes).await {
                    Ok(ChunkOutcome::More(progress)) => {
                        if progress.next != next + len {
                            // The server stands elsewhere: the hash follows it.
                            hasher = QuickXor::new();
                            self.hash_prefix(&mut hasher, progress.next).await?;
                        }
                        next = progress.next;
                        e.store().with(|s| s.outbox_set_session(seq, Some(&url), progress.expires, Some(next)))?;
                        e.upload_progress(seq, next, size);
                        fragments += 1;
                        e.fault(Fault::MidSession(fragments))?;
                    }
                    Ok(ChunkOutcome::Done(item)) => {
                        e.upload_progress(seq, size, size);
                        e.fault(Fault::AfterSend)?;
                        return Ok(Sent { hash: Some(hasher.finish_base64()), answer: Ok(*item) });
                    }
                    Err(WriteError::SessionGone) => {
                        if let Some(sent) = self.ended(&target).await? {
                            return Ok(sent);
                        }
                        restarts += 1;
                        if restarts > 1 {
                            return Err(Fail::Now(Outcome::backoff("the upload session ended twice")));
                        }
                        break;
                    }
                    Err(err @ (WriteError::NameExists | WriteError::Changed | WriteError::NotFound)) => {
                        self.forget_session()?;
                        return Ok(Sent { hash: None, answer: Err(err) });
                    }
                    // The session stays, to be resumed.
                    Err(other) => return Err(other.into()),
                }
            }
        }
    }

    /// The answer's content is compared with what was sent: other content
    /// means the server holds something else (§4.8 step 5). It is never
    /// committed as this file's: it is sent again from zero, against
    /// the version it made — a new file's is deleted first, so that the
    /// name is free again.
    async fn finish(&self, item: DriveItem, hash: Option<String>) -> Result<Outcome, Fail> {
        self.forget_session()?;
        let differs = matches!((hash.as_deref(), item.quick_xor_hash()), (Some(ours), Some(theirs)) if ours != theirs);
        if !differs {
            return self.commit(item).await;
        }
        tracing::warn!("OneDrive holds other content than was sent for {}: it goes again", self.found.rel.display());
        if self.row.kind == OutboxKind::Create {
            // Remembered in the row, so that the next run deletes it first,
            // whatever happens to this delete.
            let outcome = Outcome::backoff(format!("{}{}", BAD_ITEM, item.id));
            let guard = item.e_tag.clone().or(item.c_tag.clone()).unwrap_or_default();
            return match self.e.cfg.drive.delete_item(&item.id, &guard).await {
                Ok(()) | Err(WriteError::NotFound) => Ok(Outcome::backoff(reason::HASH)),
                Err(_) => Ok(outcome),
            };
        }
        let made = Base {
            etag: item.e_tag.clone(),
            ctag: item.c_tag.clone(),
            parent: item.parent_reference.as_ref().and_then(|p| p.id.clone()),
            name: item.name.clone(),
        };
        self.e.store().with(|s| s.outbox_amend(self.row.seq, |next| next.base = Some(made)))?;
        Ok(Outcome::backoff(reason::HASH))
    }

    /// The commit (§3.5): step 1 on the file's own descriptor, under its
    /// inode lock — stamp from the snapshot, cTag, `hydrated`, `fsync`, then
    /// the item id and `fsync` — and step 2 in one store transaction.
    ///
    /// Wherever the file went during the upload — renamed, moved out, deleted
    /// and held open only here, or with another inode at its name (an
    /// editor's backup-and-rewrite save, a move out and back in) — the item is
    /// the object that was sent, and its handle is recorded: the examination
    /// then decides it as if the change came just after the commit. Committed
    /// with no local object, the item could never be proved gone, and a later
    /// delete would be lost (limitations log F54).
    async fn commit(&self, item: DriveItem) -> Result<Outcome, Fail> {
        let answer = answer_row(&item, Some(self.parent))?;
        let _tree = self.e.cfg.tree_lock.lock().await;
        {
            let _inode = self.e.cfg.locks.lock(InodeKey::of(self.file)?).await;
            let (file, snap, ctag) = (Arc::clone(self.file), self.snap, item.c_tag.clone());
            blocking(move || match placeholder::read_state(&file) {
                Ok(None | Some(State::Hydrated)) => local::commit_attributes(&file, snap, ctag.as_deref()),
                // Freed up meanwhile: a placeholder of the version just sent.
                Ok(Some(State::OnlineOnly)) => ctag.map_or(Ok(()), |c| placeholder::write_ctag(&file, &c)),
                other => Err(io::Error::other(format!("the file is being filled or freed ({other:?})"))),
            })
            .await?;
            self.e.fault(Fault::CommitStep1Partial)?;
            let (file, id) = (Arc::clone(self.file), item.id.clone());
            blocking(move || local::commit_id(&file, &id)).await?;
        }
        self.e.fault(Fault::AfterCommitStep1)?;
        let event = self.e.event(kind::UPLOADED, &self.found.rel, crate::sync::activity::human_size(self.snap.size));
        commit_row(self.e, self.row, &answer, self.found.inode.handle.as_ref(), self.parent, event)?;
        Ok(Outcome::Done)
    }
}
