//! How a file's content gets to OneDrive (`docs/design/writes.md` §6.1, §6.3): the read, and the one
//! upload session.
//!
//! The file is read only while downloaded or unmanaged (WR1), under its
//! inode lock (a free-up waits) and a read lease (a writer's open waits the
//! milliseconds of one read), and compared with the snapshot after every
//! read. Every non-empty file goes through an upload session persisted
//! before its first byte, in fragments of [`Limits::chunk`], the
//! session persisted after each: a file up to that size is the session's one
//! fragment, and goes the same way ([`Job::send_session`]). A session is
//! resumed while the file is still the snapshot it was opened for, and
//! cancelled when given up.
//!
//! [`Limits::chunk`]: crate::upload::Limits::chunk

use std::sync::Arc;

use konedrive_fs::lease;
use konedrive_graph::drive::{ChunkOutcome, DriveError, DriveItem, UploadTarget, WriteError};
use konedrive_graph::quickxor::QuickXor;
use konedrive_tree::outbox::{OutboxRow, Reason, SessionUrl};

use super::{removed, Job, Sent};
use crate::folder::disk::Disk;
use crate::folder::locks::InodeKey;
use crate::local::{QUIET, RECHECK};
use crate::upload::engine::{now, Engine, Fail, Outcome};
use crate::upload::local::{self, Read, Snap};
use crate::upload::steps::{blocking, blocking_under, cancel_session, locate};
use crate::upload::Fault;

/// Why an upload stops before its next fragment.
enum Stop {
    /// The row waits with this outcome: the session and its offset stay in
    /// the row, and the next run resumes them — or opens a new session,
    /// logged, when this one expired meanwhile.
    Wait(Outcome),
    /// The file is under none of the row's names: the upload ends
    /// ([`removed`]).
    Removed,
}

/// Whether an upload stops before its next fragment — the first one too,
/// once its session is open and persisted — and why: the one place such a
/// stop is decided.
///
/// - **Paused** (`docs/design/writes.md` §11): waiting, reason
///   [`Reason::Paused`], due again as soon as the pause ends. No failure.
/// - **The daemon stopping**: ready, resumed at the next start.
/// - **OneDrive full** (a refusal of another row, `space`): ready in its
///   place, reason [`Reason::WaitingForSpace`], taken again once a quota read shows
///   space.
/// - **The write gate** closed: waiting until it opens.
/// - **The file removed**: under none of the row's names, as
///   [`locate`] looks for it — the same test as a run's start. A move whose
///   row is recorded is found under its new name, and the upload goes on.
async fn stop_between_fragments(e: &Engine, disk: &Arc<Disk>, row: &OutboxRow) -> Result<Option<Stop>, Fail> {
    if e.stopped() {
        return Ok(Some(Stop::Wait(Outcome::wait(Reason::Paused, std::time::Duration::ZERO))));
    }
    // The daemon is stopping: the session is persisted, and the
    // next start resumes it.
    if e.closing() {
        return Ok(Some(Stop::Wait(Outcome::again())));
    }
    if e.space_full() {
        return Ok(Some(Stop::Wait(Outcome::Space(Reason::WaitingForSpace))));
    }
    if let Err(why) = e.may_write().await {
        return Ok(Some(Stop::Wait(Outcome::wait(Reason::NotAllowed(Some(why)), std::time::Duration::ZERO))));
    }
    if locate(e, disk, row).await?.filter(|f| !f.is_dir).is_none() {
        return Ok(Some(Stop::Removed));
    }
    Ok(None)
}

/// Where an upload session stands between two of its requests
/// ([`Job::send_session`]). Every step ends in the next one, or in
/// [`Step::Over`].
enum Step {
    /// The row holds a session opened for this very content: it is asked
    /// where it stands.
    Resume(SessionUrl),
    /// No session: one is opened, and persisted before its first byte.
    Open,
    /// The session takes the fragment at `next`. `opened`: its opening was
    /// the request before this one, so the guard it carried was checked
    /// just now.
    Fragment { url: SessionUrl, next: u64, opened: bool },
    /// The session answered `404`: it completed without its answer reaching
    /// us, or it expired.
    Ended,
    /// Nothing more is sent.
    Over(Sent),
}

impl Step {
    fn settled(outcome: Outcome) -> Self {
        Step::Over(Sent::Settled(outcome))
    }
}

/// What one run of a session keeps between its steps.
#[derive(Default)]
struct Run {
    /// The hash of the file's first `hashed` bytes.
    hasher: QuickXor,
    hashed: u64,
    /// Fragments accepted and persisted.
    fragments: u32,
    /// Sessions found ended.
    ended: u32,
}

/// A read's bytes, or what the row waits for when it gave none.
fn bytes_of(read: Read) -> Result<Vec<u8>, Outcome> {
    match read {
        Read::Bytes(bytes) => Ok(bytes),
        Read::Busy => Err(Outcome::wait(Reason::OpenForWriting, RECHECK)),
        Read::NotLocal => Err(Outcome::wait(Reason::NotLocal, RECHECK)),
        Read::Changed => Err(Outcome::wait(Reason::Changed, QUIET)),
    }
}

impl Job<'_> {
    pub(super) async fn send(&self, target: UploadTarget<'_>) -> Result<Sent, Fail> {
        // OneDrive became full since the row was taken: nothing that adds
        // content starts (`space`).
        if self.e.space_full() {
            return Ok(Sent::Settled(Outcome::Space(Reason::WaitingForSpace)));
        }
        if self.snap.size == 0 {
            self.send_empty(target).await
        } else {
            self.send_session(target).await
        }
    }

    /// What `len` bytes at `offset` are, read under the inode lock and a
    /// read lease, or why they cannot be read now.
    async fn read(&self, offset: u64, len: usize) -> Result<Read, Fail> {
        let inode = self.e.locks().lock(InodeKey::of(self.file)?).await;
        let (file, snap) = (Arc::clone(self.file), self.snap);
        blocking_under(inode.hold(), move || local::read(&file, offset, len, snap)).await
    }

    /// [`local::drop_cache`] for the file that went up whole.
    async fn drop_cache(&self) {
        let file = Arc::clone(self.file);
        let _ = blocking(move || {
            local::drop_cache(&file);
            Ok(())
        })
        .await;
    }

    /// Feeds the bytes from `from` up to `upto` to `hasher`, a fragment at
    /// a time. Bytes that cannot be read now end the row's run: it waits.
    async fn hash_range(&self, hasher: &mut QuickXor, from: u64, upto: u64) -> Result<(), Fail> {
        let mut at = from;
        while at < upto {
            let len = (upto - at).min(self.e.limits().chunk.max(1));
            hasher.update(&bytes_of(self.read(at, len as usize).await?).map_err(Fail::Now)?);
            at += len;
        }
        Ok(())
    }

    /// The content's quickXorHash, read for it.
    pub(super) async fn hash(&self) -> Result<String, Fail> {
        let mut hasher = QuickXor::new();
        self.hash_range(&mut hasher, 0, self.snap.size).await?;
        Ok(hasher.finish_base64())
    }

    /// An empty file: one `PUT`, no session (a session cannot carry it).
    async fn send_empty(&self, target: UploadTarget<'_>) -> Result<Sent, Fail> {
        // Still empty, still the snapshot, and no writer: as for any read.
        if let Err(wait) = bytes_of(self.read(0, 0).await?) {
            return Ok(Sent::Settled(wait));
        }
        match self.e.drive().upload_empty(target, self.snap.sec).await {
            Ok(item) => {
                self.e.upload_progress(self.row.seq, 0, 0);
                self.e.fault(Fault::AfterSend)?;
                Ok(Sent::Landed(Box::new(item), QuickXor::new().finish_base64()))
            }
            Err(err) => Sent::refused(err),
        }
    }

    /// A file with content (§6.3): the fragments of one upload session, a
    /// file of one fragment's size and a larger one alike. The session a run
    /// before persisted for this very content is resumed from where the
    /// server stands; otherwise one is opened, and persisted before its
    /// first byte. The row holds the session after every step,
    /// so a stop or a crash between any two of them is replayed from the
    /// session's own status.
    async fn send_session(&self, target: UploadTarget<'_>) -> Result<Sent, Fail> {
        let mut run = Run::default();
        let mut step = self.session.clone().map_or(Step::Open, Step::Resume);
        loop {
            step = match step {
                Step::Resume(url) => self.resume(url).await?,
                Step::Open => self.open(target).await?,
                Step::Fragment { url, next, opened } => self.fragment(&target, url, next, opened, &mut run).await?,
                Step::Ended => self.ended(&target, &mut run).await?,
                Step::Over(sent) => return Ok(sent),
            };
        }
    }

    /// Where the session a run before persisted stands. Only one opened for
    /// this very content (the same snapshot) comes here.
    async fn resume(&self, url: SessionUrl) -> Result<Step, Fail> {
        match self.e.drive().upload_status(url.as_str()).await {
            Ok(progress) => Ok(Step::Fragment { url, next: progress.next, opened: false }),
            Err(WriteError::SessionGone) => Ok(Step::Ended),
            // The session stays, to be resumed.
            Err(other) => Err(other.into()),
        }
    }

    /// Opens a session for this content and persists it, listed with the
    /// place a new file's session holds, before any byte is sent.
    /// A new file's place is recorded before the request:
    /// a stop before the URL is persisted leaves a placeholder this folder
    /// still knows of ([`Job::own_placeholder`]).
    async fn open(&self, target: UploadTarget<'_>) -> Result<Step, Fail> {
        let place = match target {
            UploadTarget::New { parent_id, name } => Some((parent_id.to_owned(), name.to_owned())),
            UploadTarget::Existing { .. } => None,
        };
        let seq = self.row.seq;
        let mut carried = None;
        if let Some((parent, name)) = place.clone() {
            carried = self.e.store().call(move |s| s.outbox_record_opening(seq, &parent, &name, now())).await?;
        }
        let opened = match self.e.drive().create_upload_session(target, self.snap.sec).await {
            Ok(opened) => opened,
            Err(err) => {
                // Any answer but `Transient` (a timeout, a lost connection, a
                // `5xx` other than `503`, an unreadable answer, a failure
                // before sending) is certain: this request made no
                // placeholder. The record this call made goes; one
                // carried from an earlier attempt whose outcome was not known
                // is kept as it was — what holds the name may be that
                // attempt's placeholder.
                if place.is_some() && !matches!(err, WriteError::Transient(_)) {
                    self.e.store().call(move |s| s.outbox_opening_answered(seq, carried)).await?;
                }
                return Ok(Step::Over(Sent::refused(err)?));
            }
        };
        // A crash here leaves a session nothing knows of but its recorded
        // place: its placeholder holds a new file's name until it expires or
        // is deleted (limitations log F172).
        self.e.fault(Fault::SessionNotPersisted)?;
        let url = SessionUrl::new(opened.url);
        let (kept, expires) = (url.clone(), opened.expires);
        self.e
            .store()
            .call(move |s| s.outbox_open_session(seq, &kept, expires, place.as_ref().map(|(p, n)| (p.as_str(), n.as_str())), now()))
            .await?;
        Ok(Step::Fragment { url, next: 0, opened: true })
    }

    /// One fragment of the session at `url`, from `next`: read, fed to the
    /// hash, sent, and the session's new offset persisted (§6.3 step 3).
    ///
    /// - Before every fragment, the first too, the upload may stop
    ///   ([`stop_between_fragments`]).
    /// - Before the last one (§6.3 step 4): a writer, the snapshot, and —
    ///   for a changed file — the item in OneDrive once more, since the
    ///   session's guard was checked when it was opened, not when it
    ///   completes. Not when the opening was the request just before: its
    ///   answer was that check.
    /// - A fragment OneDrive refuses for now goes again to the same session
    ///   (`DriveClient::upload_chunk`); refused still, the row fails for now
    ///   and keeps its session for its next run.
    /// - A file that is no longer the snapshot gives the session up, and so
    ///   do a refusal for good and a session that expects bytes past the
    ///   file's end.
    async fn fragment(&self, target: &UploadTarget<'_>, url: SessionUrl, next: u64, opened: bool, run: &mut Run) -> Result<Step, Fail> {
        let (e, seq, size) = (self.e, self.row.seq, self.snap.size);
        if let Some(outcome) = self.stopped(next).await? {
            return Ok(Step::settled(outcome));
        }
        if next >= size {
            // Nothing of this file is left to send, and it did not complete.
            self.abandon(&url).await?;
            return Err(WriteError::Failed(format!("the upload session expects bytes from {next} of {size}").into()).into());
        }
        let len = (size - next).min(e.limits().chunk);
        if next + len >= size {
            if lease::open_for_writing(self.file)? {
                return Ok(Step::settled(Outcome::wait(Reason::OpenForWriting, RECHECK)));
            }
            if Snap::of(self.file)? != self.snap {
                self.abandon(&url).await?;
                return Ok(Step::settled(Outcome::wait(Reason::Changed, QUIET)));
            }
            if !opened {
                if let Some(refusal) = self.guard_broken(target).await? {
                    self.abandon(&url).await?;
                    return Ok(Step::Over(Sent::Refused(refusal)));
                }
            }
        }
        // The hash follows the server: it starts over wherever the session
        // does not stand at the end of what was read for it.
        if run.hashed != next {
            run.hasher = QuickXor::new();
            self.hash_range(&mut run.hasher, 0, next).await?;
        }
        let read = self.read(next, len as usize).await?;
        if matches!(read, Read::Changed) {
            self.abandon(&url).await?;
        }
        let bytes = match bytes_of(read) {
            Ok(bytes) => bytes,
            Err(wait) => return Ok(Step::settled(wait)),
        };
        run.hasher.update(&bytes);
        run.hashed = next + len;
        match e.drive().upload_chunk(url.as_str(), next, size, bytes).await {
            // An answer that takes nothing: the session stays, to be resumed.
            Ok(ChunkOutcome::More(progress)) if progress.next <= next => {
                Err(WriteError::Transient(format!("the upload session still expects bytes from {} of {size}", progress.next).into()).into())
            }
            Ok(ChunkOutcome::More(progress)) => {
                let (kept, expires, next) = (url.clone(), progress.expires, progress.next);
                e.store().call(move |s| s.outbox_set_session(seq, Some(&kept), expires, Some(next))).await?;
                e.upload_progress(seq, next, size);
                run.fragments += 1;
                e.fault(Fault::MidSession(run.fragments))?;
                Ok(Step::Fragment { url, next, opened: false })
            }
            Ok(ChunkOutcome::Done(item)) => {
                e.upload_progress(seq, size, size);
                self.drop_cache().await;
                e.fault(Fault::AfterSend)?;
                Ok(Step::Over(Sent::Landed(item, run.hasher.finish_base64())))
            }
            Err(WriteError::SessionGone) => Ok(Step::Ended),
            Err(err @ (WriteError::NameExists | WriteError::Changed | WriteError::NotFound)) => {
                self.abandon(&url).await?;
                Ok(Step::Over(Sent::Refused(err)))
            }
            // The session stays, to be resumed.
            Err(other) => Err(other.into()),
        }
    }

    /// Whether the upload stops before the fragment at `next`
    /// ([`stop_between_fragments`]), and the row's outcome then: it waits
    /// with its session kept, or its file is gone and it ends ([`removed`]).
    async fn stopped(&self, next: u64) -> Result<Option<Outcome>, Fail> {
        let (seq, size) = (self.row.seq, self.snap.size);
        match stop_between_fragments(self.e, self.disk, self.row).await? {
            None => Ok(None),
            Some(Stop::Wait(outcome)) => {
                tracing::info!("the upload of {} stops at {next} of {size} bytes; its session is kept", self.found.rel.display());
                Ok(Some(outcome))
            }
            Some(Stop::Removed) => {
                tracing::info!("the upload of {} stops at {next} of {size} bytes: the file was removed", self.found.rel.display());
                // The row as it is now: with the session just used.
                match self.e.store().call(move |s| s.outbox_row(seq)).await? {
                    Some(now) => removed(self.e, self.disk, &now).await.map(Some),
                    None => Ok(Some(Outcome::Done)),
                }
            }
        }
    }

    /// The item a changed file goes into, read again before the session's
    /// last fragment (§6.3 step 4): the refusal its opening would get now —
    /// another version than the guard names, or no item.
    async fn guard_broken(&self, target: &UploadTarget<'_>) -> Result<Option<WriteError>, Fail> {
        let UploadTarget::Existing { id, if_match } = *target else { return Ok(None) };
        match self.e.drive().item(id).await {
            Ok(item) if item.e_tag.as_deref() != Some(if_match) && item.c_tag.as_deref() != Some(if_match) => Ok(Some(WriteError::Changed)),
            Ok(_) => Ok(None),
            Err(DriveError::NotFound) => Ok(Some(WriteError::NotFound)),
            Err(err) => Err(err.into()),
        }
    }

    /// The session ended (`404`): it completed without its answer reaching
    /// us, or it expired — while it waited (a pause, a restart, the
    /// network), or under the upload. The item holding this content is
    /// adopted (§10); otherwise the bytes sent before are lost, and the
    /// upload starts over with a new session — once in a run: a second
    /// session that ends leaves the row in backoff.
    async fn ended(&self, target: &UploadTarget<'_>, run: &mut Run) -> Result<Step, Fail> {
        let hash = self.hash().await?;
        if let Some(item) = self.fetch(target).await? {
            if item.quick_xor_hash() == Some(hash.as_str()) {
                return Ok(Step::Over(Sent::Landed(Box::new(item), hash)));
            }
        }
        self.forget_session().await?;
        run.ended += 1;
        if run.ended > 1 {
            return Ok(Step::settled(Outcome::backoff(Reason::SessionEnded)));
        }
        tracing::info!("the upload session of {} ended: the upload starts over", self.found.rel.display());
        Ok(Step::Open)
    }

    /// The row's session completed, or is gone: nothing to cancel.
    pub(super) async fn forget_session(&self) -> Result<(), Fail> {
        let seq = self.row.seq;
        Ok(self.e.store().call(move |s| s.outbox_session_ended(seq)).await?)
    }

    /// `DELETE <uploadUrl>`: the session is given up (§6.1) —
    /// the content changed while it went up, or OneDrive refused it. The row
    /// points at it no more; a cancel that fails leaves it listed, and a
    /// later run cancels it.
    async fn abandon(&self, url: &SessionUrl) -> Result<(), Fail> {
        cancel_session(self.e, url).await?;
        let seq = self.row.seq;
        Ok(self.e.store().call(move |s| s.outbox_set_session(seq, None, None, None)).await?)
    }

    /// The item the target names, as OneDrive has it now.
    async fn fetch(&self, target: &UploadTarget<'_>) -> Result<Option<DriveItem>, Fail> {
        let fetched = match *target {
            UploadTarget::New { parent_id, name } => self.e.drive().child(parent_id, name).await,
            UploadTarget::Existing { id, .. } => self.e.drive().item(id).await,
        };
        match fetched {
            Ok(item) => Ok(Some(item)),
            Err(DriveError::NotFound) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}
