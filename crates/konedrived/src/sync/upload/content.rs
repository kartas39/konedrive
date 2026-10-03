//! A file's content going up (§4.1, §4.3, §4.8): a `create` or an `update`.
//!
//! The file is read only while downloaded or unmanaged (WR1), under its
//! inode lock (a free-up waits) and a read lease (a writer's open waits the
//! milliseconds of one read), and compared with the snapshot after every
//! read. Every non-empty file goes through an upload session persisted
//! before its first byte (issue #47): up to [`Limits::small_max`] the whole
//! file is its one fragment; above, it goes in fragments, the session
//! persisted after each. A session is resumed while the file is still the
//! snapshot it was opened for, and cancelled when given up.
//!
//! [`Limits::small_max`]: super::Limits::small_max

use std::fs::File;
use std::io;
use std::sync::Arc;

use konedrive_fs::lease;
use konedrive_fs::placeholder::{self, State};

use super::engine::{now, Engine, Fail, Outcome};
use super::local::{self, Found, Read, Snap, SYNC_UPLOADING};
use super::steps::{answer_row, blocking, cancel_session, commit_row, copy, follow_cloud, held, local_name, locate, never_uploaded, parent_of, taken, temporary, upload_as_new, wanted_name, Ours, Taken};
use super::{kind, reason, space, Fault};
use crate::drive::item::parse_graph_time;
use crate::drive::{ChunkOutcome, DriveError, DriveItem, ItemChange, UploadTarget, WriteError};
use crate::quickxor::QuickXor;
use crate::sync::disk::Disk;
use crate::sync::local::examine::OPEN_FOR_WRITING;
use crate::sync::local::{names, QUIET, RECHECK};
use crate::sync::InodeKey;
use crate::tree::outbox::{Base, OutboxKind, OutboxRow};
use crate::tree::Table;

/// How far OneDrive's clock may be behind this machine's when a placeholder's
/// creation time is compared with the recorded opening (issue #84).
pub(super) const CLOCK_SLACK: i64 = 5 * 60;

/// A new file's row whose upload OneDrive holds with other content, and
/// could not be deleted yet: `hash-mismatch:<item id>`.
const BAD_ITEM: &str = "hash-mismatch:";

pub(super) async fn run(e: &Arc<Engine>, disk: &Disk, row: OutboxRow) -> Result<Outcome, Fail> {
    let local = local_name(&row)?;
    let Some(found) = locate(e, disk, &row).await?.filter(|f| !f.is_dir) else {
        return removed(e, disk, &row).await;
    };
    // Taken while it waits for space only to see whether its file is gone
    // (`space`, `Engine::space_allows`): it is not, so it waits on.
    if let Some(why) = e.space_holds(&row) {
        return Ok(Outcome::Space(why));
    }
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
    let (seq, text) = (row.seq, snap.text());
    if let Some(stale) = e.store().call(move |s| s.outbox_take_snapshot(seq, &text)).await? {
        cancel_session(e, &stale).await?;
    }
    e.upload_progress(row.seq, 0, snap.size);
    // A row whose local path is an object that is leaving, or below one
    // (issue #104, decision 1): its content goes up into the item where
    // OneDrive has it — never a rename or a move. A row elsewhere (the
    // user's own move out of it) is carried out as any other.
    let held_out = match (&row.item_id, row.kind) {
        (Some(_), OutboxKind::Update) => {
            let rel = row.rel.clone();
            e.store().call(move |s| Ok(s.leaving()?.iter().any(|(_, at)| rel.starts_with(at)))).await?
        }
        _ => false,
    };
    let at_base = row.base.as_ref().and_then(|b| Some((b.parent.clone()?, b.name.clone()?))).filter(|_| held_out);
    let (parent, name) = match at_base {
        Some(at) => at,
        None => {
            let Some(parent) = parent_of(e, disk, &row).await? else { return Ok(Outcome::later(reason::PARENT, RECHECK)) };
            (parent, wanted_name(&row, &local))
        }
    };
    let content_only = held_out && row.base.as_ref().is_some_and(|b| b.parent.as_deref() == Some(parent.as_str()) && b.name.as_deref() == Some(name.as_str()));
    let job = Job { e, disk, row: &row, found: &found, file: &file, snap, parent: &parent, name: &name, session, content_only };
    match row.kind {
        OutboxKind::Create => job.create().await,
        _ => job.update().await,
    }
}

/// A `create` or `update` whose file is under none of its names — removed
/// here, or moved where no row looks (issue #36; for a `create`, #27). The
/// row ends now, with no retry, and the upload session it opened is
/// cancelled. A `create` ends through [`never_uploaded`]: it leaves with the
/// rows behind it that never got an item id, and one `not-uploaded` event.
/// An `update` leaves alone, with no event: the version OneDrive has stays
/// until a removal of the item deletes it — the row behind it, or the one
/// the examination records.
///
/// `row` as the store holds it now: the session it names is the one to
/// cancel.
async fn removed(e: &Engine, disk: &Disk, row: &OutboxRow) -> Result<Outcome, Fail> {
    if row.kind == OutboxKind::Create {
        return never_uploaded(e, disk, row).await;
    }
    if let Some(url) = &row.session_url {
        cancel_session(e, url).await?;
    }
    tracing::info!("the new version of {} is not uploaded: the file was removed here", row.rel.display());
    let seq = row.seq;
    e.store().call(move |s| s.outbox_drop(seq, None, None, None)).await?;
    Ok(Outcome::Done)
}

/// Why an upload in fragments stops after the fragment just sent.
enum Stop {
    /// The row waits with this outcome: the session and its offset stay in
    /// the row, and the next run resumes them — or opens a new session,
    /// logged, when this one expired meanwhile.
    Wait(Outcome),
    /// The file is under none of the row's names: the upload ends
    /// ([`removed`]).
    Removed,
}

/// Whether an upload in fragments stops after the fragment just sent, and
/// why: the one place such a stop is decided.
///
/// - **Paused** (`docs/design/writes.md` §11): waiting, reason
///   [`reason::PAUSED`], due again as soon as the pause ends. No failure.
/// - **The daemon stopping** (issue #84): ready, resumed at the next start.
/// - **OneDrive full** (a refusal of another row, `space`): ready in its
///   place, reason [`space::WAITING`], taken again once a quota read shows
///   space.
/// - **The write gate** closed: waiting until it opens.
/// - **The file removed** (issue #36): under none of the row's names, as
///   [`locate`] looks for it — the same test as a run's start. A move whose
///   row is recorded is found under its new name, and the upload goes on.
async fn stop_between_fragments(e: &Engine, disk: &Disk, row: &OutboxRow) -> Result<Option<Stop>, Fail> {
    if e.stopped() {
        return Ok(Some(Stop::Wait(Outcome::wait(reason::PAUSED, std::time::Duration::ZERO))));
    }
    // The daemon is stopping (issue #84): the session is persisted, and the
    // next start resumes it.
    if e.closing() {
        return Ok(Some(Stop::Wait(Outcome::again())));
    }
    if e.space_full() {
        return Ok(Some(Stop::Wait(Outcome::Space(space::WAITING.into()))));
    }
    if let Err(why) = e.cfg.host.may_write() {
        return Ok(Some(Stop::Wait(Outcome::wait(&format!("not allowed now: {why}"), std::time::Duration::ZERO))));
    }
    if locate(e, disk, row).await?.filter(|f| !f.is_dir).is_none() {
        return Ok(Some(Stop::Removed));
    }
    Ok(None)
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
    /// The row's local path is a leaving object's, or below it: its content
    /// only, into the item where OneDrive has it — no rename, no move, no
    /// local object recorded (issue #104).
    content_only: bool,
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
                if let Some(outcome) = self.own_placeholder().await? {
                    return Ok(outcome);
                }
                let hash = self.hash(sent.hash).await?;
                match taken(self.e, self.row, self.parent, self.name, Ours::File(&hash)).await? {
                    Taken::Free => Ok(Outcome::again()),
                    Taken::Temporary(swap) => temporary(self.e, self.row, self.parent, &swap).await,
                    // The same content is there: its own earlier request, or
                    // create/create with equal files (§6). Nothing is sent.
                    Taken::Adopt(item) => self.commit(*item).await,
                    Taken::Held => Ok(held(self.row)),
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

    /// A `409` for a new file (issue #47): is the name held by the empty
    /// placeholder of an upload session this folder opened for it? Such a
    /// session is never taken for someone else's file. This row's own is
    /// resumed by the next run (or cancelled there, if the content changed);
    /// one another row still sends waits for that row; any other — given up,
    /// its cancel not gone through — is cancelled now, and the create goes
    /// again. `None`: no session of ours holds the name, and [`taken`]
    /// decides, as for any `409`.
    ///
    /// No listed session, but an opening recorded there whose URL never came
    /// (a stop between the request and its persisting, issue #84):
    /// [`Job::opened_placeholder`].
    async fn own_placeholder(&self) -> Result<Option<Outcome>, Fail> {
        let (parent, name, seq) = (self.parent.to_owned(), self.name.to_owned(), self.row.seq);
        let held = self.e.store().call(move |s| s.upload_sessions_at(&parent, &name)).await?;
        if held.is_empty() {
            return self.opened_placeholder().await;
        }
        let mut outcome = Outcome::again();
        for (url, by) in held {
            match by {
                Some(by) if by == seq => {}
                Some(_) => outcome = Outcome::backoff(reason::SESSION_OPEN),
                None => {
                    if !cancel_session(self.e, &url).await? {
                        outcome = Outcome::backoff(reason::SESSION_OPEN);
                    }
                }
            }
        }
        tracing::info!("{} is held in OneDrive by an upload session of this folder, not by another file", self.found.rel.display());
        Ok(Some(outcome))
    }

    /// A `409` at a place where this folder recorded an opening whose URL
    /// never came (issue #84) — carried from an earlier attempt whose outcome
    /// was not known, with its row or left by it, since a certain answer to
    /// the attempt that made the record clears it (issue #89): the holder is
    /// an empty file the delta feed never listed, made within one record's
    /// window — its first recording to its latest attempt whose outcome was
    /// not known, each widened by [`CLOCK_SLACK`]: it is that opening's
    /// placeholder. It is deleted, and the create goes again; a delete
    /// OneDrive refuses leaves the row waiting (`upload-session-open`). Never
    /// a copy. Any other holder — with content, listed, outside every
    /// window, or its time unknown — is not taken for ours: `None`, and
    /// [`taken`] decides.
    ///
    /// Once resolved — the placeholder deleted, the name found free, or the
    /// holder not ours (at most one placeholder of ours holds a name) — the
    /// records at the place are cleared, so that a later `409` there never
    /// compares with an old time. Kept only while the holder's time is not
    /// given, a read or a delete fails for now, or the delete is refused.
    async fn opened_placeholder(&self) -> Result<Option<Outcome>, Fail> {
        let (parent, name) = (self.parent.to_owned(), self.name.to_owned());
        let windows = self.e.store().call(move |s| s.upload_opening_windows(&parent, &name)).await?;
        if windows.is_empty() {
            return Ok(None);
        }
        let holder = match self.e.cfg.drive.child(self.parent, self.name).await {
            Ok(holder) => holder,
            Err(DriveError::NotFound) => {
                self.resolved().await?;
                return Ok(Some(Outcome::again()));
            }
            Err(err) => return Err(err.into()),
        };
        let Some(created) = holder.created_date_time.as_deref().and_then(parse_graph_time) else { return Ok(None) };
        let empty = holder.file.is_some() && holder.size == Some(0);
        let id = holder.id.clone();
        let listed = self.e.store().call(move |s| Ok(s.get(Table::Items, &id)?.is_some() || s.get(Table::Staging, &id)?.is_some())).await?;
        let within = windows.iter().any(|(at, last)| at - CLOCK_SLACK <= created && created <= last + CLOCK_SLACK);
        if listed || !empty || !within {
            self.resolved().await?;
            return Ok(None);
        }
        let guard = holder.e_tag.clone().or(holder.c_tag.clone()).unwrap_or_default();
        match self.e.cfg.drive.delete_item(&holder.id, &guard).await {
            Ok(()) | Err(WriteError::NotFound) => {
                self.resolved().await?;
                tracing::info!("{} was held in OneDrive by the placeholder of an upload session this folder opened: deleted", self.found.rel.display());
                Ok(Some(Outcome::again()))
            }
            Err(err @ (WriteError::Throttled { .. } | WriteError::Transient(_) | WriteError::SignedOut)) => Err(err.into()),
            Err(err) => {
                tracing::info!(
                    "{} is held in OneDrive by the placeholder of an upload session this folder opened, not deleted ({err}): it waits for the session to expire",
                    self.found.rel.display()
                );
                Ok(Some(Outcome::backoff(reason::SESSION_OPEN)))
            }
        }
    }

    /// The recorded openings at this place are resolved: cleared.
    async fn resolved(&self) -> Result<(), Fail> {
        let (parent, name) = (self.parent.to_owned(), self.name.to_owned());
        self.e.store().call(move |s| s.upload_openings_clear_at(&parent, &name)).await?;
        Ok(())
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
                    Taken::Temporary(swap) => return temporary(self.e, row, self.parent, &swap).await,
                    Taken::Adopt(item) => guard = item.e_tag.unwrap_or(guard),
                    Taken::Held => return Ok(held(row)),
                    Taken::Copy => return copy(self.e, self.disk, row, self.found, self.parent, None).await,
                },
                Err(WriteError::Changed) => match self.landed(id, &base).await? {
                    // The move went through before (a replay: the temporary
                    // name of a swap, I1): the content follows it.
                    Some(fresh) => guard = fresh,
                    None => return self.changed(None).await,
                },
                Err(WriteError::NotFound) => return self.gone_or_new(id).await,
                Err(other) => return Err(other.into()),
            }
        }
        let sent = self.send(UploadTarget::Existing { id, if_match: &guard }, Some((id, &guard))).await?;
        match sent.answer {
            Ok(item) => self.finish(item, sent.hash).await,
            Err(WriteError::Changed) => self.changed(sent.hash).await,
            // Deleted in OneDrive while changed here: local wins (§6).
            Err(WriteError::NotFound) => self.gone_or_new(id).await,
            Err(other) => Err(other.into()),
        }
    }

    /// The item is gone from OneDrive. Changed here, it goes up again as new
    /// (§6: local wins) — but not from inside a leaving object: removed in
    /// OneDrive means removed (issue #104, decision 2), and the row ends.
    async fn gone_or_new(&self, id: &str) -> Result<Outcome, Fail> {
        if !self.content_only {
            return upload_as_new(self.e, self.row, self.found, self.parent, id).await;
        }
        tracing::info!("{} was removed from OneDrive: its change is not uploaded", self.found.rel.display());
        if let Some(url) = &self.row.session_url {
            cancel_session(self.e, url).await?;
        }
        let seq = self.row.seq;
        self.e.store().call(move |s| s.outbox_drop(seq, None, None, None)).await?;
        Ok(Outcome::Done)
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
            Err(DriveError::NotFound) => return self.gone_or_new(id).await,
            Err(err) => return Err(err.into()),
        };
        let hash = self.hash(hash).await?;
        let base = row.base.clone().unwrap_or_default();
        let remote_parent = remote.parent_reference.as_ref().and_then(|p| p.id.clone());
        let remote_name = remote.name.clone().unwrap_or_default();
        let same_content = remote.quick_xor_hash() == Some(hash.as_str());
        if same_content && (self.content_only || (remote_parent.as_deref() == Some(self.parent) && remote_name == self.name)) {
            return self.commit(remote).await;
        }
        if same_content || (remote.c_tag.is_some() && remote.c_tag == base.ctag) {
            // Not placed here: OneDrive's place is the row's, and nothing on
            // disk follows it (issue #104).
            let moved_there = !self.content_only && (remote_parent != base.parent || Some(remote_name.as_str()) != base.name.as_deref());
            let _tree = self.e.cfg.tree_lock.lock().await;
            let followed = if moved_there { follow_cloud(self.e, self.disk, self.found, &remote).await? } else { None };
            let fresh = Base { etag: remote.e_tag.clone(), ctag: base.ctag.clone(), parent: remote_parent.clone(), name: Some(remote_name.clone()) };
            let seq = row.seq;
            self.e.store().call(move |s| {
                s.outbox_amend(seq, |next| {
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
            }).await?;
            return Ok(Outcome::again());
        }
        copy(self.e, self.disk, row, self.found, self.parent, Some(id)).await
    }

    async fn send(&self, target: UploadTarget<'_>, last_check: Option<(&str, &str)>) -> Result<Sent, Fail> {
        // OneDrive became full since the row was taken: nothing that adds
        // content starts (`space`).
        if self.e.space_full() {
            return Err(Fail::Now(Outcome::Space(space::WAITING.into())));
        }
        if self.snap.size == 0 {
            self.send_empty(target).await
        } else if self.snap.size <= self.e.cfg.limits.small_max {
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

    /// An empty file: one `PUT`, no session (a session cannot carry it).
    async fn send_empty(&self, target: UploadTarget<'_>) -> Result<Sent, Fail> {
        // Still empty, still the snapshot, and no writer: as for any read.
        self.read(0, 0).await?;
        let answer = self.e.cfg.drive.upload_empty(target, self.snap.sec).await;
        if answer.is_ok() {
            self.e.upload_progress(self.row.seq, 0, 0);
            self.e.fault(Fault::AfterSend)?;
        }
        Ok(Sent { hash: Some(QuickXor::new().finish_base64()), answer })
    }

    /// A file up to [`Limits::small_max`](super::Limits::small_max): read
    /// whole, and sent as the one fragment of a session persisted before it
    /// goes (issue #47) — resumed if a run before opened it for this very
    /// content. A fragment OneDrive refuses for now goes again to the same
    /// session (`DriveClient::upload_chunk`); refused still, the row fails
    /// for now and keeps its session for its next run.
    async fn send_small(&self, target: UploadTarget<'_>) -> Result<Sent, Fail> {
        let size = self.snap.size;
        let bytes = self.read(0, size as usize).await?;
        let mut hasher = QuickXor::new();
        hasher.update(&bytes);
        let hash = hasher.finish_base64();
        let drive = &self.e.cfg.drive;
        let mut session = match self.resume(&target, Some(&hash)).await? {
            Ok(session) => session,
            Err(adopted) => return Ok(adopted),
        };
        let mut restarts = 0;
        loop {
            let (url, mut next) = match session.take() {
                Some(session) => session,
                None => match self.open(target).await? {
                    Ok(url) => (url, 0),
                    Err(err) => return Ok(Sent { hash: Some(hash), answer: Err(err) }),
                },
            };
            // The whole file, or what the session still expects of it.
            let mut rounds = 0;
            loop {
                let from = next.min(size) as usize;
                match drive.upload_chunk(&url, from as u64, size, bytes[from..].to_vec()).await {
                    Ok(ChunkOutcome::Done(item)) => {
                        self.e.upload_progress(self.row.seq, size, size);
                        local::drop_cache(self.file);
                        self.e.fault(Fault::AfterSend)?;
                        return Ok(Sent { hash: Some(hash), answer: Ok(*item) });
                    }
                    Ok(ChunkOutcome::More(progress)) => {
                        rounds += 1;
                        if rounds > 2 {
                            // The session stays, to be resumed.
                            return Err(WriteError::Transient(format!("the upload session still expects bytes from {} of {size}", progress.next)).into());
                        }
                        next = progress.next;
                    }
                    Err(WriteError::SessionGone) => {
                        if let Some(adopted) = self.restart(&target, Some(&hash), &mut restarts).await? {
                            return Ok(adopted);
                        }
                        break;
                    }
                    Err(err @ (WriteError::NameExists | WriteError::Changed | WriteError::NotFound)) => {
                        self.abandon(&url).await?;
                        return Ok(Sent { hash: Some(hash), answer: Err(err) });
                    }
                    // The session stays, to be resumed.
                    Err(other) => return Err(other.into()),
                }
            }
        }
    }

    /// The session a run before persisted for this very content (the same
    /// snapshot), and where the server stands in it; `Ok(None)` when there
    /// is none, or it expired. `Err`: it ended with this content in
    /// OneDrive, which is adopted (§5).
    async fn resume(&self, target: &UploadTarget<'_>, hash: Option<&str>) -> Result<Result<Option<(String, u64)>, Sent>, Fail> {
        let Some(url) = self.session.clone() else { return Ok(Ok(None)) };
        match self.e.cfg.drive.upload_status(&url).await {
            Ok(progress) => Ok(Ok(Some((url, progress.next)))),
            Err(WriteError::SessionGone) => {
                if let Some(adopted) = self.ended(target, hash).await? {
                    return Ok(Err(adopted));
                }
                // Expired while it waited (a pause, a restart, the network): the
                // bytes sent before are lost.
                tracing::info!("the upload session of {} has expired: the upload starts over", self.found.rel.display());
                Ok(Ok(None))
            }
            // The session stays, to be resumed.
            Err(other) => Err(other.into()),
        }
    }

    /// Opens a session for this content and persists it, listed with the
    /// place a new file's session holds, before any byte is sent (issue
    /// #47). A new file's place is recorded before the request (issue #84):
    /// a stop before the URL is persisted leaves a placeholder this folder
    /// still knows of ([`Job::own_placeholder`]). `Err` inside: OneDrive's
    /// refusal to open it.
    async fn open(&self, target: UploadTarget<'_>) -> Result<Result<String, WriteError>, Fail> {
        let place = match target {
            UploadTarget::New { parent_id, name } => Some((parent_id.to_owned(), name.to_owned())),
            UploadTarget::Existing { .. } => None,
        };
        let seq = self.row.seq;
        let mut carried = None;
        if let Some((parent, name)) = place.clone() {
            carried = self.e.store().call(move |s| s.outbox_record_opening(seq, &parent, &name, now())).await?;
        }
        let opened = match self.e.cfg.drive.create_upload_session(target, self.snap.size, self.snap.sec).await {
            Ok(opened) => opened,
            Err(err) => {
                // Any answer but `Transient` (a timeout, a lost connection, a
                // `5xx` other than `503`, an unreadable answer, a failure
                // before sending) is certain: this request made no
                // placeholder (issue #89). The record this call made goes; one
                // carried from an earlier attempt whose outcome was not known
                // is kept as it was — what holds the name may be that
                // attempt's placeholder.
                if place.is_some() && !matches!(err, WriteError::Transient(_)) {
                    self.e.store().call(move |s| s.outbox_opening_answered(seq, carried)).await?;
                }
                return Ok(Err(err));
            }
        };
        // A crash here leaves a session nothing knows of but its recorded
        // place: its placeholder holds a new file's name until it expires or
        // is deleted (limitations log F172).
        self.e.fault(Fault::SessionNotPersisted)?;
        let (seq, url, expires) = (self.row.seq, opened.url.clone(), opened.expires);
        self.e
            .store()
            .call(move |s| s.outbox_open_session(seq, &url, expires, place.as_ref().map(|(p, n)| (p.as_str(), n.as_str())), now()))
            .await?;
        Ok(Ok(opened.url))
    }

    /// The session under an upload ended (`404`): the item holding this
    /// content is adopted (§5); otherwise the upload starts over — once per
    /// run.
    async fn restart(&self, target: &UploadTarget<'_>, hash: Option<&str>, restarts: &mut u32) -> Result<Option<Sent>, Fail> {
        if let Some(adopted) = self.ended(target, hash).await? {
            return Ok(Some(adopted));
        }
        tracing::info!("the upload session of {} ended: the upload starts over", self.found.rel.display());
        *restarts += 1;
        if *restarts > 1 {
            return Err(Fail::Now(Outcome::backoff("the upload session ended twice")));
        }
        Ok(None)
    }

    /// The row's session completed, or is gone: nothing to cancel.
    async fn forget_session(&self) -> Result<(), Fail> {
        let seq = self.row.seq;
        Ok(self.e.store().call(move |s| s.outbox_session_ended(seq)).await?)
    }

    /// `DELETE <uploadUrl>`: the session is given up (§4.3, issue #47) —
    /// the content changed while it went up, or OneDrive refused it. The row
    /// points at it no more; a cancel that fails leaves it listed, and a
    /// later run cancels it.
    async fn abandon(&self, url: &str) -> Result<(), Fail> {
        cancel_session(self.e, url).await?;
        let seq = self.row.seq;
        Ok(self.e.store().call(move |s| s.outbox_set_session(seq, None, None, None)).await?)
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
    async fn ended(&self, target: &UploadTarget<'_>, known: Option<&str>) -> Result<Option<Sent>, Fail> {
        let hash = self.hash(known.map(str::to_owned)).await?;
        if let Some(item) = self.fetch(target).await? {
            if item.quick_xor_hash() == Some(hash.as_str()) {
                return Ok(Some(Sent { hash: Some(hash), answer: Ok(item) }));
            }
        }
        self.forget_session().await?;
        Ok(None)
    }

    /// §4.8: a session in fragments, resumed from where the server stands
    /// when the file is still the snapshot the row holds.
    async fn send_large(&self, target: UploadTarget<'_>, last_check: Option<(&str, &str)>) -> Result<Sent, Fail> {
        let (e, seq, size) = (self.e, self.row.seq, self.snap.size);
        let drive = &e.cfg.drive;
        // Only a session opened for this very content is resumed.
        let mut resumed = match self.resume(&target, None).await? {
            Ok(resumed) => resumed,
            Err(adopted) => return Ok(adopted),
        };
        let mut restarts = 0;
        loop {
            let (url, mut next) = match resumed.take() {
                Some(session) => session,
                None => match self.open(target).await? {
                    Ok(url) => (url, 0),
                    Err(err) => return Ok(Sent { hash: None, answer: Err(err) }),
                },
            };
            let mut hasher = QuickXor::new();
            self.hash_prefix(&mut hasher, next).await?;
            let mut fragments = 0;
            loop {
                match stop_between_fragments(e, self.disk, self.row).await? {
                    Some(Stop::Wait(stop)) => {
                        tracing::info!("the upload of {} stops at {next} of {size} bytes; its session is kept", self.found.rel.display());
                        return Err(Fail::Now(stop));
                    }
                    Some(Stop::Removed) => {
                        tracing::info!("the upload of {} stops at {next} of {size} bytes: the file was removed", self.found.rel.display());
                        // The row as it is now: with the session just used.
                        let now = e.store().call(move |s| s.outbox_row(seq)).await?;
                        let Some(now) = now else { return Err(Fail::Now(Outcome::Done)) };
                        return Err(Fail::Now(removed(e, self.disk, &now).await?));
                    }
                    None => {}
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
                        let (kept, expires) = (url.clone(), progress.expires);
                        e.store().call(move |s| s.outbox_set_session(seq, Some(&kept), expires, Some(next))).await?;
                        e.upload_progress(seq, next, size);
                        fragments += 1;
                        e.fault(Fault::MidSession(fragments))?;
                    }
                    Ok(ChunkOutcome::Done(item)) => {
                        e.upload_progress(seq, size, size);
                        local::drop_cache(self.file);
                        e.fault(Fault::AfterSend)?;
                        return Ok(Sent { hash: Some(hasher.finish_base64()), answer: Ok(*item) });
                    }
                    Err(WriteError::SessionGone) => {
                        if let Some(adopted) = self.restart(&target, None, &mut restarts).await? {
                            return Ok(adopted);
                        }
                        break;
                    }
                    Err(err @ (WriteError::NameExists | WriteError::Changed | WriteError::NotFound)) => {
                        self.abandon(&url).await?;
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
        self.forget_session().await?;
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
        let seq = self.row.seq;
        self.e.store().call(move |s| s.outbox_amend(seq, |next| next.base = Some(made))).await?;
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
        self.e.space_used(self.snap.size);
        let event = self.e.event(kind::UPLOADED, &self.found.rel, crate::sync::activity::human_size(self.snap.size));
        // An item not placed here records no local object (issue #104).
        let handle = self.found.inode.handle.as_ref().filter(|_| !self.content_only);
        commit_row(self.e, self.row, &answer, handle, self.parent, event).await?;
        Ok(Outcome::Done)
    }
}
