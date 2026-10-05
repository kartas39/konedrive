//! A file's content going up (§4.1, §4.3, §4.8): a `create` or an `update`.
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
//! [`Limits::chunk`]: super::Limits::chunk

use konedrive_tree::ActivityKind;
use std::fs::File;
use std::io;
use std::sync::Arc;

use konedrive_fs::lease;
use konedrive_fs::placeholder::{self, State};

use super::engine::{now, Engine, Fail, Outcome};
use super::local::{self, Found, Opened, Read, Snap};
use super::steps::{answer_row, blocking, blocking_under, cancel_session, commit_row, copy, follow_cloud, holds, local_name, locate, name_taken, never_uploaded, tree, upload_as_new, wanted_name, Guard, Named, Ours};
use super::Fault;
use konedrive_graph::drive::item::parse_graph_time;
use konedrive_graph::drive::{ChunkOutcome, DriveError, DriveItem, ItemChange, UploadTarget, WriteError};
use konedrive_graph::quickxor::QuickXor;
use crate::folder::disk::Disk;
use crate::local::{names, QUIET, RECHECK};
use crate::folder::locks::InodeKey;
use konedrive_tree::outbox::{BadItem, Base, OutboxKind, OutboxRow, Reason, SessionUrl};
use konedrive_tree::Table;

/// How far OneDrive's clock may be behind this machine's when a placeholder's
/// creation time is compared with the recorded opening.
pub(super) const CLOCK_SLACK: i64 = 5 * 60;

pub(super) async fn run(e: &Arc<Engine>, disk: &Arc<Disk>, row: OutboxRow) -> Result<Outcome, Fail> {
    let local = local_name(&row)?;
    let Some(found) = locate(e, disk, &row).await?.filter(|f| !f.is_dir) else {
        return removed(e, disk, &row).await;
    };
    // Taken while it waits for space only to see whether its file is gone
    // (`space::allows`): it is not, so it waits on.
    if let Some(why) = e.space_holds(&row) {
        return Ok(Outcome::Space(why));
    }
    let object = found.clone();
    let (file, snap) = match blocking(move || local::open_for_upload(&object)).await? {
        Opened::Content(file, snap) => (Arc::new(file), snap),
        Opened::NotLocal => return Ok(Outcome::wait(Reason::NotLocal, RECHECK)),
        Opened::BadState(err) => return Ok(Outcome::blocked(Reason::BadState(Some(err)))),
        Opened::Writing => return Ok(Outcome::wait(Reason::OpenForWriting, RECHECK)),
    };
    if snap.size > names::MAX_FILE_SIZE {
        return Ok(Outcome::blocked(names::Refused::TooLarge.reason()));
    }
    // The snapshot and the session belong together: a session opened for
    // other content goes in the same transaction, before any request.
    let session = row.session_url.clone().filter(|_| row.snapshot_is(snap.snapshot()));
    let (seq, taken) = (row.seq, snap.snapshot());
    if let Some(stale) = e.store().call(move |s| s.outbox_take_snapshot(seq, taken)).await? {
        cancel_session(e, &stale).await?;
    }
    e.upload_progress(row.seq, 0, snap.size);
    let Some(parent) = super::steps::parent_recorded(e, disk, &row).await? else { return Ok(Outcome::later(Reason::Parent, RECHECK)) };
    let name = wanted_name(&row, &local);
    let job = Job { e, disk, row: &row, found: &found, file: &file, snap, parent: &parent, name: &name, session };
    match row.kind {
        OutboxKind::Create => job.create().await,
        _ => job.update().await,
    }
}

/// A `create` or `update` whose file is under none of its names — removed
/// here, or moved where no row looks (for a `create`). The
/// row ends now, with no retry, and the upload session it opened is
/// cancelled. A `create` ends through [`never_uploaded`]: it leaves with the
/// rows behind it that never got an item id, and one `not-uploaded` event.
/// An `update` leaves alone, with no event: the version OneDrive has stays
/// until a removal of the item deletes it — the row behind it, or the one
/// the examination records.
///
/// `row` as the store holds it now: the session it names is the one to
/// cancel.
async fn removed(e: &Engine, disk: &Arc<Disk>, row: &OutboxRow) -> Result<Outcome, Fail> {
    if row.kind == OutboxKind::Create {
        return never_uploaded(e, disk, row).await;
    }
    if let Some(url) = &row.session_url {
        cancel_session(e, url).await?;
    }
    tracing::info!("the new version of {} is not uploaded: the file was removed here", row.rel.display());
    let seq = row.seq;
    e.store().call(move |s| s.outbox_drop(seq, None, None)).await?;
    Ok(Outcome::Done)
}

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

/// What an upload came to, for [`Job::create`] and [`Job::update`] to decide.
enum Sent {
    /// OneDrive holds an item for it — the last fragment's answer, or the
    /// item a session that ended left there with this content (§5) — and
    /// the hash of what was read here for it.
    Landed(Box<DriveItem>, String),
    /// OneDrive refused it for good: the name is taken (`409`), the item
    /// changed (`412`) or is gone (`404`). No session of it is left open.
    Refused(WriteError),
    /// The row's outcome is decided here: it waits, or its file is gone.
    Settled(Outcome),
}

impl Sent {
    /// OneDrive's refusal: one the row's kind decides on, or a failure.
    fn refused(err: WriteError) -> Result<Self, Fail> {
        match err {
            WriteError::NameExists | WriteError::Changed | WriteError::NotFound => Ok(Sent::Refused(err)),
            other => Err(other.into()),
        }
    }
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

struct Job<'a> {
    e: &'a Arc<Engine>,
    disk: &'a Arc<Disk>,
    row: &'a OutboxRow,
    found: &'a Found,
    file: &'a Arc<File>,
    snap: Snap,
    parent: &'a str,
    name: &'a str,
    /// A session opened for exactly this content (the same snapshot), to
    /// resume.
    session: Option<SessionUrl>,
}

impl Job<'_> {
    /// A new file's upload that OneDrive holds with other content (the bad
    /// item, remembered beside the row with the content tag the upload's
    /// answer gave: [`Job::finish`]): that item goes before the file is sent
    /// again — or, holding this content after all, is this file's.
    ///
    /// - The item is read again. It is still the bad upload while its cTag
    ///   is the one remembered — the eTag moves by itself in OneDrive, with
    ///   no change of content, and says nothing here (when the answer had no
    ///   cTag, its eTag is what is compared). Then it is deleted, with the
    ///   eTag just read as the guard, which covers the time between the read
    ///   and the delete.
    /// - Another cTag, or a `412` on that delete, means someone changed it
    ///   in OneDrive since (another device, the web): theirs now, not this
    ///   row's. It is left where it is and forgotten, and the name's holder
    ///   is decided as any other's, by the `409` the create then gets
    ///   ([`name_taken`]).
    /// - With no tag remembered (a row an older version wrote, an answer
    ///   that carried none) it is deleted as that version deleted it: with
    ///   the tag just read, whatever happened to it meanwhile (F200).
    /// - It is adopted only while nothing here knows it (no outbox row of
    ///   the item, no local object: the test of `steps::shared::taken`): one the delta
    ///   feed listed and a cycle placed here is that local file's.
    /// - It is forgotten exactly when it is deleted, found gone, or left
    ///   as someone else's; adopted, it goes with the row. Anything else —
    ///   a read that fails, a delete OneDrive does not carry out — leaves it
    ///   remembered for the next run, whatever the row's reason becomes.
    ///
    /// A `create` does this before anything it sends, so a row that goes
    /// on to a copy (`steps::copy`) has no bad item left; a row made a
    /// `create` again by `steps::upload_as_new` was an `update` or a
    /// `move`, which never has one.
    async fn clear_bad_item(&self) -> Result<Option<Outcome>, Fail> {
        let seq = self.row.seq;
        let Some(bad) = self.e.store().call(move |s| s.outbox_bad_item(seq)).await? else { return Ok(None) };
        let item = match self.e.drive().item(&bad.id).await {
            Ok(item) => item,
            Err(DriveError::NotFound) => return self.forget_bad_item().await.map(|()| None),
            Err(err) => return Err(err.into()),
        };
        if item.quick_xor_hash() == Some(self.hash().await?.as_str()) {
            let id = bad.id.clone();
            let known_here = self.e.store().call(move |s| Ok(!s.outbox_for_item(&id)?.is_empty() || s.local_handle(&id)?.is_some())).await?;
            if !known_here {
                return self.commit(item).await.map(Some);
            }
        }
        let left = || tracing::info!("what OneDrive holds for {} with other content was changed there since: it is left", self.found.rel.display());
        if !bad.still(item.c_tag.as_deref(), item.e_tag.as_deref()) {
            left();
            return self.forget_bad_item().await.map(|()| None);
        }
        match self.e.drive().delete_item(&bad.id, Guard::of_item(&item).as_str()).await {
            Ok(()) | Err(WriteError::NotFound) => {}
            Err(WriteError::Changed) => left(),
            Err(err) => return Err(err.into()),
        }
        self.forget_bad_item().await.map(|()| None)
    }

    /// The row's bad item is gone from OneDrive, or is not this row's any
    /// more: nothing to delete.
    async fn forget_bad_item(&self) -> Result<(), Fail> {
        let seq = self.row.seq;
        Ok(self.e.store().call(move |s| s.outbox_set_bad_item(seq, None)).await?)
    }

    async fn create(&self) -> Result<Outcome, Fail> {
        if let Some(outcome) = self.clear_bad_item().await? {
            return Ok(outcome);
        }
        match self.send(UploadTarget::New { parent_id: self.parent, name: self.name }).await? {
            Sent::Landed(item, hash) => self.finish(*item, hash).await,
            Sent::Settled(outcome) => Ok(outcome),
            Sent::Refused(WriteError::NameExists) => {
                if let Some(outcome) = self.own_placeholder().await? {
                    return Ok(outcome);
                }
                let hash = self.hash().await?;
                match name_taken(self.e, self.disk, self.row, Some(self.found), self.parent, self.name, Ours::File(&hash)).await? {
                    // The same content is there: its own earlier request, or
                    // create/create with equal files (§6). Nothing is sent.
                    Named::Adopt(item) => self.commit(*item).await,
                    Named::Settled(outcome) => Ok(outcome),
                }
            }
            Sent::Refused(WriteError::NotFound) => {
                self.e.host().cycle_wanted();
                Ok(Outcome::backoff(Reason::Parent))
            }
            Sent::Refused(other) => Err(other.into()),
        }
    }

    /// A `409` for a new file: is the name held by the empty
    /// placeholder of an upload session this folder opened for it? Such a
    /// session is never taken for someone else's file. This row's own is
    /// resumed by the next run (or cancelled there, if the content changed);
    /// one another row still sends waits for that row; any other — given up,
    /// its cancel not gone through — is cancelled now, and the create goes
    /// again. `None`: no session of ours holds the name, and [`name_taken`]
    /// decides, as for any `409`.
    ///
    /// No listed session, but an opening recorded there whose URL never came
    /// (a stop between the request and its persisting):
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
                Some(_) => outcome = Outcome::backoff(Reason::SessionOpen),
                None => {
                    if !cancel_session(self.e, &url).await? {
                        outcome = Outcome::backoff(Reason::SessionOpen);
                    }
                }
            }
        }
        tracing::info!("{} is held in OneDrive by an upload session of this folder, not by another file", self.found.rel.display());
        Ok(Some(outcome))
    }

    /// A `409` at a place where this folder recorded an opening whose URL
    /// never came — carried from an earlier attempt whose outcome
    /// was not known, with its row or left by it, since a certain answer to
    /// the attempt that made the record clears it: the holder is
    /// an empty file the delta feed never listed, made within one record's
    /// window — its first recording to its latest attempt whose outcome was
    /// not known, each widened by [`CLOCK_SLACK`]: it is that opening's
    /// placeholder. It is deleted, and the create goes again; a delete
    /// OneDrive refuses leaves the row waiting (`upload-session-open`). Never
    /// a copy. Any other holder — with content, listed, outside every
    /// window, or its time unknown — is not taken for ours: `None`, and
    /// [`name_taken`] decides.
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
        let holder = match self.e.drive().child(self.parent, self.name).await {
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
        match self.e.drive().delete_item(&holder.id, Guard::of_item(&holder).as_str()).await {
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
                Ok(Some(Outcome::backoff(Reason::SessionOpen)))
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
        let Some(id) = row.item_id.as_deref() else { return Ok(Outcome::blocked(Reason::NoItem)) };
        let base = row.base.clone().unwrap_or_default();
        // F55 (4): a row queued against another version than the base's
        // carries that version's cTag, and no eTag.
        let Some(mut guard) = Guard::of_base(&base) else { return Ok(Outcome::blocked(Reason::NoGuard)) };
        let new_name = (Some(self.name) != base.name.as_deref()).then_some(self.name);
        let new_parent = (Some(self.parent) != base.parent.as_deref()).then_some(self.parent);
        if new_name.is_some() || new_parent.is_some() {
            // Moved as well: the move first, then the content (§3.5).
            let change = ItemChange { name: new_name, parent_id: new_parent, modified: None };
            match self.e.drive().update_item(id, guard.as_str(), &change).await {
                Ok(item) => guard = guard.renewed(item.e_tag),
                Err(WriteError::NameExists) => match name_taken(self.e, self.disk, row, Some(self.found), self.parent, self.name, Ours::Item(id)).await? {
                    Named::Adopt(item) => guard = guard.renewed(item.e_tag),
                    Named::Settled(outcome) => return Ok(outcome),
                },
                Err(WriteError::Changed) => match self.landed(id, &base).await? {
                    // The move went through before (a replay: the temporary
                    // name of a swap, I1): the content follows it.
                    Some(fresh) => guard = guard.renewed(Some(fresh)),
                    None => return self.changed().await,
                },
                Err(WriteError::NotFound) => return self.gone_or_new(id).await,
                Err(other) => return Err(other.into()),
            }
        }
        match self.send(UploadTarget::Existing { id, if_match: guard.as_str() }).await? {
            Sent::Landed(item, hash) => self.finish(*item, hash).await,
            Sent::Settled(outcome) => Ok(outcome),
            Sent::Refused(WriteError::Changed) => self.changed().await,
            // Deleted in OneDrive while changed here: local wins (§6).
            Sent::Refused(WriteError::NotFound) => self.gone_or_new(id).await,
            Sent::Refused(other) => Err(other.into()),
        }
    }

    /// The item is gone from OneDrive. Changed here, it goes up again as new
    /// (§6: local wins), wherever it is.
    async fn gone_or_new(&self, id: &str) -> Result<Outcome, Fail> {
        upload_as_new(self.e, self.row, self.found, self.parent, id).await
    }

    /// A `412` on the move before the content: has OneDrive the item where
    /// this row takes it already, its content still the version the change
    /// was made against? Then the move landed before (§5), and the content
    /// goes against the fresh eTag.
    async fn landed(&self, id: &str, base: &Base) -> Result<Option<String>, Fail> {
        let remote = match self.e.drive().item(id).await {
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
    async fn changed(&self) -> Result<Outcome, Fail> {
        let row = self.row;
        let id = row.item_id.as_deref().unwrap_or_default();
        let remote = match self.e.drive().item(id).await {
            Ok(remote) => remote,
            Err(DriveError::NotFound) => return self.gone_or_new(id).await,
            Err(err) => return Err(err.into()),
        };
        let hash = self.hash().await?;
        let base = row.base.clone().unwrap_or_default();
        let remote_parent = remote.parent_reference.as_ref().and_then(|p| p.id.clone());
        let remote_name = remote.name.clone().unwrap_or_default();
        let same_content = remote.quick_xor_hash() == Some(hash.as_str());
        if same_content && remote_parent.as_deref() == Some(self.parent) && remote_name == self.name {
            return self.commit(remote).await;
        }
        if same_content || (remote.c_tag.is_some() && remote.c_tag == base.ctag) {
            let moved_there = remote_parent != base.parent || Some(remote_name.as_str()) != base.name.as_deref();
            // OneDrive has the item where the folder cannot hold it (a name
            // too long, the Personal Vault...), and nobody moved it here:
            // its content goes into the item where it is, with no name and
            // no folder sent — the row was made to send content, and a name
            // sent now would undo what was done in OneDrive. The commit
            // keeps the item where the disk has it, and the reconcile takes
            // it off once nothing in it waits.
            if moved_there && self.unmoved_here(id).await? && !holds(self.e, &remote).await? {
                tracing::info!("{} is in OneDrive where this folder cannot hold it: its content goes into it there", self.found.rel.display());
                if same_content {
                    return self.commit(remote).await;
                }
                let guard = Guard::of_item(&remote);
                return match self.send(UploadTarget::Existing { id, if_match: guard.as_str() }).await? {
                    Sent::Landed(item, hash) => self.finish(*item, hash).await,
                    Sent::Settled(outcome) => Ok(outcome),
                    // Changed there once more meanwhile: looked at again.
                    Sent::Refused(WriteError::Changed) => Ok(Outcome::again()),
                    Sent::Refused(WriteError::NotFound) => self.gone_or_new(id).await,
                    Sent::Refused(other) => Err(other.into()),
                };
            }
            let tree = tree(self.e).await;
            let followed = if moved_there { follow_cloud(self.e, self.disk, &tree, self.found, &remote).await? } else { None };
            let held = followed.is_some() || holds(self.e, &remote).await?;
            let fresh = super::steps::base_after_a_change(&base, self.parent, self.name, &remote, held);
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

    /// Whether the row sends the item nowhere: its object stands where the
    /// base has the item, under the name the base has. Then a difference
    /// between the row's place and OneDrive's was made in OneDrive.
    async fn unmoved_here(&self, id: &str) -> Result<bool, Fail> {
        let id = id.to_owned();
        let base = self.e.store().call(move |s| s.get(Table::Items, &id)).await?;
        Ok(base.is_some_and(|base| base.parent_id.as_deref() == Some(self.parent) && base.name == self.name))
    }

    async fn send(&self, target: UploadTarget<'_>) -> Result<Sent, Fail> {
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
    async fn hash(&self) -> Result<String, Fail> {
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

    /// A file with content (§4.8): the fragments of one upload session, a
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
    /// hash, sent, and the session's new offset persisted (§4.8 step 3).
    ///
    /// - Before every fragment, the first too, the upload may stop
    ///   ([`stop_between_fragments`]).
    /// - Before the last one (§4.8 step 4): a writer, the snapshot, and —
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
    /// last fragment (§4.8 step 4): the refusal its opening would get now —
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
    /// adopted (§5); otherwise the bytes sent before are lost, and the
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
    async fn forget_session(&self) -> Result<(), Fail> {
        let seq = self.row.seq;
        Ok(self.e.store().call(move |s| s.outbox_session_ended(seq)).await?)
    }

    /// `DELETE <uploadUrl>`: the session is given up (§4.3) —
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

    /// The answer's content is compared with what was sent: other content
    /// means the server holds something else (§4.8 step 5). It is never
    /// committed as this file's: it is sent again from zero, against
    /// the version it made — a new file's is deleted first, so that the
    /// name is free again.
    async fn finish(&self, item: DriveItem, hash: String) -> Result<Outcome, Fail> {
        self.forget_session().await?;
        let differs = item.quick_xor_hash().is_some_and(|theirs| theirs != hash);
        if !differs {
            return self.commit(item).await;
        }
        tracing::warn!("OneDrive holds other content than was sent for {}: it goes again", self.found.rel.display());
        if self.row.kind == OutboxKind::Create {
            // Remembered beside the row, with the content tag this answer
            // gave, before its delete is asked for: the next run deletes it
            // first whatever happens to this delete, and to the row
            // meanwhile (`clear_bad_item`). The delete is asked even when
            // the store fails: what cannot be remembered must not stay there.
            let bad = BadItem::answered(&item.id, item.c_tag.as_deref(), item.e_tag.as_deref());
            let seq = self.row.seq;
            let remembered = self.e.store().call(move |s| s.outbox_set_bad_item(seq, Some(&bad))).await;
            if let Err(err) = &remembered {
                tracing::warn!("what OneDrive holds for {} with other content could not be recorded ({err})", self.found.rel.display());
            }
            // The answer's own tag: nothing came between. Neither tag in the
            // answer: an empty guard, and what OneDrive makes of it (F200, F235).
            match self.e.drive().delete_item(&item.id, Guard::of_item(&item).as_str()).await {
                Ok(()) | Err(WriteError::NotFound) => {
                    if remembered.is_ok() {
                        self.forget_bad_item().await?;
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        "what OneDrive holds for {} with other content could not be deleted ({err}): it is deleted before the file goes again",
                        self.found.rel.display()
                    );
                    // Neither deleted nor remembered: the store's failure is the row's.
                    remembered?;
                }
            }
            return Ok(Outcome::backoff(Reason::Hash));
        }
        let made = Base {
            etag: item.e_tag.clone(),
            ctag: item.c_tag.clone(),
            parent: item.parent_reference.as_ref().and_then(|p| p.id.clone()),
            name: item.name.clone(),
        };
        let seq = self.row.seq;
        self.e.store().call(move |s| s.outbox_amend(seq, |next| next.base = Some(made))).await?;
        Ok(Outcome::backoff(Reason::Hash))
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
        let tree = tree(self.e).await;
        {
            let inode = self.e.locks().lock(InodeKey::of(self.file)?).await;
            let (file, snap, ctag) = (Arc::clone(self.file), self.snap, item.c_tag.clone());
            blocking_under((Arc::clone(&tree), inode.hold()), move || match placeholder::read_state(&file) {
                Ok(None | Some(State::Hydrated)) => local::commit_attributes(&file, snap, ctag.as_deref()),
                // Freed up meanwhile: a placeholder of the version just sent.
                Ok(Some(State::OnlineOnly)) => ctag.map_or(Ok(()), |c| placeholder::write_ctag(&file, &c)),
                other => Err(io::Error::other(format!("the file is being filled or freed ({other:?})"))),
            })
            .await?;
            self.e.fault(Fault::CommitStep1Partial)?;
            let (file, id) = (Arc::clone(self.file), item.id.clone());
            blocking_under((Arc::clone(&tree), inode.hold()), move || local::commit_id(&file, &id)).await?;
        }
        self.e.fault(Fault::AfterCommitStep1)?;
        self.e.space_used(self.snap.size);
        let event = self.e.event(ActivityKind::Uploaded, &self.found.rel, crate::status::activity::human_size(self.snap.size));
        commit_row(self.e, self.row, &answer, self.found.inode.handle.as_ref(), self.parent, event).await?;
        Ok(Outcome::Done)
    }
}
