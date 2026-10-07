//! A file's content going up (`docs/design/writes.md` §6.1, §6.3): a `create` or an `update`.
//!
//! Here is what is decided: which of the two it is, what a name that is
//! taken or an item that changed in OneDrive means, what an earlier attempt
//! left there, and what the answer makes of the row. How the bytes get
//! there — the read, the upload session, its fragments — is in [`session`],
//! asked through [`Job::send`] and answered with a [`Sent`].

use konedrive_tree::ActivityKind;
use std::fs::File;
use std::io;
use std::sync::Arc;

use konedrive_fs::placeholder::{self, State};

use super::engine::{Engine, Fail, Outcome};
use super::local::{self, Found, Opened, Snap};
use super::steps::{answer_row, blocking, blocking_under, cancel_session, commit_row, copy, follow_cloud, holds, local_name, locate, name_taken, never_uploaded, tree, upload_as_new, wanted_name, Guard, Named, Ours};
use super::Fault;
use konedrive_graph::drive::item::parse_graph_time;
use konedrive_graph::drive::{DriveError, DriveItem, ItemChange, UploadTarget, WriteError};
use crate::folder::disk::Disk;
use crate::local::{names, RECHECK};
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
/// here, or moved where no row looks. The
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

mod session;

/// What an upload came to, for [`Job::create`] and [`Job::update`] to decide.
enum Sent {
    /// OneDrive holds an item for it — the last fragment's answer, or the
    /// item a session that ended left there with this content (§10) — and
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
    ///   the tag just read, whatever happened to it meanwhile.
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
                    // create/create with equal files (§7). Nothing is sent.
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
        // A row queued against another version than the base's
        // carries that version's cTag, and no eTag.
        let Some(mut guard) = Guard::of_base(&base) else { return Ok(Outcome::blocked(Reason::NoGuard)) };
        let new_name = (Some(self.name) != base.name.as_deref()).then_some(self.name);
        let new_parent = (Some(self.parent) != base.parent.as_deref()).then_some(self.parent);
        if new_name.is_some() || new_parent.is_some() {
            // Moved as well: the move first, then the content (§5.2).
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
            // Deleted in OneDrive while changed here: local wins (§7).
            Sent::Refused(WriteError::NotFound) => self.gone_or_new(id).await,
            Sent::Refused(other) => Err(other.into()),
        }
    }

    /// The item is gone from OneDrive. Changed here, it goes up again as new
    /// (§7: local wins), wherever it is.
    async fn gone_or_new(&self, id: &str) -> Result<Outcome, Fail> {
        upload_as_new(self.e, self.row, self.found, self.parent, id).await
    }

    /// A `412` on the move before the content: has OneDrive the item where
    /// this row takes it already, its content still the version the change
    /// was made against? Then the move landed before (§10), and the content
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

    /// `412` for a change (§6.2, §7): read the item again. Its content is
    /// this file's → adopted (its own earlier request, §10). Its content is
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

    /// The answer's content is compared with what was sent: other content
    /// means the server holds something else (§6.3 step 5). It is never
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
            // answer: an empty guard, and what OneDrive makes of it.
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

    /// The commit (§5.4): step 1 on the file's own descriptor, under its
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
