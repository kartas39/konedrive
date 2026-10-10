//! What the steps remember of one item ([`ItemMemory`]): each fact as a value, and whether
//! the item's row still says what a fact was learnt for ([`FactStanding`]). Nothing here
//! reads or writes the store: every decision is a function of a fact and a row.
//!
//! A fact is of the item as it was when the fact was learnt. The row is the truth of what
//! the item is; a fact is answered only while the row still says it:
//!
//! - an upload session and a refusal for lack of space, while the bytes the fact holds are
//!   the file's ([`is_content_of`]): its local content, or the content it had before a
//!   writer that still has it open;
//! - a refusal of a version, while the item is at the place that was refused, with those
//!   bytes;
//! - the confirmation of a removal and a refusal of a removal, while the row is that
//!   removal ([`UnsentRemoval`]): an item OneDrive has, which the folder does not show, with
//!   a place the two sides agreed on, and the stamp the row was given when the removal came
//!   to be unsent. An item shown again and removed again is another removal, with another
//!   stamp;
//! - a taken name, while the item's local place is the place that was sent.
//!
//! The temporary name, the failed tries and the item an upload left with other content hold
//! no version of the item: they are answered as long as they are kept. A temporary name
//! says under which name OneDrive has the item, which nothing done in the folder changes.
//!
//! What becomes of the facts when something other than the row's versions changes is
//! decided here too, each a function of the memory and of typed cases: a request OneDrive
//! confirmed ([`ConfirmedRequest`]), a binding that moved, a directory that gave its
//! binding up, a content told by its hash from now on.

use std::ffi::OsString;

use konedrive_reason::Reason;

use crate::model::{ContentVersion, Item, ItemPlace, ItemVersion, LocalItemId, OneDriveItemId};

/// The address OneDrive gave an upload session: what its fragments are sent to and what it
/// is cancelled by. Whoever has it can write to the session, so it is never printed.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct UploadSessionAddress(String);

impl UploadSessionAddress {
    pub fn new(address: impl Into<String>) -> Self {
        Self(address.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for UploadSessionAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UploadSessionAddress(…)")
    }
}

/// An upload session that was opened and has not ended: what a step goes on with after a
/// stop or a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadSession {
    pub address: UploadSessionAddress,
    /// When OneDrive ends the session by itself, Unix seconds; `None` when it did not say.
    pub expires_at: Option<i64>,
    /// The first byte OneDrive does not have yet: how far the upload got.
    pub next_offset: u64,
    /// The content version being sent: the session is the item's while these bytes are the
    /// file's.
    pub content: ContentVersion,
    /// For the session of a new file: the place in OneDrive its empty file holds until the
    /// upload ends, the directory named by its local item id. `None` for a session that
    /// sends into an item OneDrive has.
    pub empty_file_place: Option<ItemPlace>,
}

/// What became of an upload session a step gave the store or went on with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadSessionStanding {
    /// The session is the item's: its step goes on with it.
    Continued,
    /// Nothing goes on with the session: the item's row went, the bytes it sends are not
    /// the file's any more, or the item has another session. Its address is kept to be
    /// cancelled, and the step sends nothing more to it.
    Abandoned,
}

/// What tells whether an item of OneDrive is still what an upload left there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadTag {
    /// The cTag the upload's answer gave: the item is still that upload while its content
    /// tag is this one, whatever became of its eTag.
    CTag(String),
    /// The eTag of the answer, which carried no cTag.
    ETag(String),
    /// The answer carried neither: nothing says the item is not that upload.
    NotGiven,
}

/// The item an upload of a new file left in OneDrive with other content than was sent: to be
/// removed before the file is sent again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MismatchedItem {
    pub item_id: OneDriveItemId,
    pub tag: UploadTag,
}

impl MismatchedItem {
    /// What the upload's answer says of the item it made: its cTag, or its eTag when it has
    /// no cTag.
    pub fn of_answer(item_id: OneDriveItemId, ctag: Option<&str>, etag: Option<&str>) -> Self {
        let tag = match (ctag, etag) {
            (Some(ctag), _) => UploadTag::CTag(ctag.to_owned()),
            (None, Some(etag)) => UploadTag::ETag(etag.to_owned()),
            (None, None) => UploadTag::NotGiven,
        };
        Self { item_id, tag }
    }

    /// Whether an item OneDrive has now with these tags is still what the upload left, and
    /// not something another client wrote since. With no tag kept, nothing says it is not.
    pub fn is_unchanged_since_upload(&self, ctag: Option<&str>, etag: Option<&str>) -> bool {
        match &self.tag {
            UploadTag::CTag(kept) => ctag == Some(kept.as_str()),
            UploadTag::ETag(kept) => etag == Some(kept.as_str()),
            UploadTag::NotGiven => true,
        }
    }
}

/// The temporary name an exchange of names is going through: the name an item is given in
/// OneDrive while the name it takes is still another item's. It says where OneDrive has the
/// item from the first request of the exchange until its step forgets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporaryName {
    /// The temporary name, in the directory of `toward`.
    pub name: OsString,
    /// The local place the item was on its way to in OneDrive. Whether it is still the
    /// item's local place is its step's question.
    pub toward: ItemPlace,
}

/// The tries of an item's step that failed: how many, when the next is due, and why the
/// last failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTries {
    pub count: u32,
    /// Unix seconds; `None` when the next try is due at once.
    pub next_try_at: Option<i64>,
    pub last_failure: Reason,
}

/// A file OneDrive refused as too big for the space it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceRefusal {
    /// The content that was refused; its size is what the file needs.
    pub content: ContentVersion,
    /// The free space of OneDrive, in bytes, as the quota read at the refusal, or the last
    /// one since, said it.
    pub free_space: u64,
}

/// Whether a file refused for lack of space would go into the free space there is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceFit {
    Fits,
    StillTooBig,
}

impl SpaceRefusal {
    /// The file fits when its size is no more than the free space, as today's upload worker
    /// lets a waiting file go after a quota read.
    pub fn decide_fit(&self, free_space: u64) -> SpaceFit {
        if self.content.size() <= free_space {
            SpaceFit::Fits
        } else {
            SpaceFit::StillTooBig
        }
    }
}

/// A removal made here that is not sent, told from every other removal of the same item: by
/// the stamp its row was given when the removal came to be unsent
/// ([`Item::unsent_since`]). The stamp is of one counter and is taken when the folder shows
/// the item again, so that a second removal of the item has another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnsentRemoval {
    pub item: LocalItemId,
    pub unsent_since: u64,
}

/// Whether `item` is a removal that is not sent, and the one stamped `unsent_since`:
/// OneDrive has the item, the folder does not show it, the two sides agreed on a place, and
/// the row has that stamp.
fn is_unsent_removal(item: &Item, unsent_since: u64) -> bool {
    item.onedrive_item.is_some() && item.local.is_none() && item.synced_place.is_some() && item.unsent_since == Some(unsent_since)
}

impl UnsentRemoval {
    /// The removal `item`'s row is now; `None` for a row that is no removal that is not
    /// sent.
    pub fn of(item: &Item) -> Option<Self> {
        let unsent_since = item.unsent_since?;
        is_unsent_removal(item, unsent_since).then_some(Self { item: item.local_item_id, unsent_since })
    }
}

/// The user's confirmation of a removal a mass removal held: of the removal with this
/// stamp, and of no later removal of the same item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemovalConfirmation {
    pub unsent_since: u64,
}

/// What OneDrive refused of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusedVersion {
    /// The local version the folder showed: the item at that place, with that content.
    Shown(ItemVersion),
    /// The removal of the item, the one its row had this stamp for
    /// ([`UnsentRemoval::unsent_since`]).
    Removal { unsent_since: u64 },
}

/// An item OneDrive refuses for good, until it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// Why, as OneDrive answered: not allowed, or refused with its message.
    pub reason: Reason,
    /// What was refused.
    pub version: RefusedVersion,
}

/// A name OneDrive answered is taken, though the client's own rule of names did not foresee
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenName {
    /// The place the item was sent to.
    pub place: ItemPlace,
    /// The name of the item OneDrive has there, as OneDrive spells it: the two names are one
    /// name to OneDrive.
    pub holder_name: OsString,
}

/// The kinds of fact a step forgets by name once it is done with them. An upload session is
/// not among them: one that ended and one that is given up are forgotten by two operations,
/// since only the second leaves a cancel to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepFact {
    MismatchedItem,
    TemporaryName,
    FailedTries,
    SpaceRefusal,
    RemovalConfirmation,
    Refusal,
    TakenName,
}

/// Whether an item's row still says what a fact was learnt for: asked of a fact that holds
/// a version, a place or a removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactStanding {
    /// The row says it: the fact is answered, and a fact given to be written is written.
    StillSaid,
    /// The version, the place or the removal the fact holds is not the row's any more: the
    /// fact is not answered, and is not written.
    VersionGone,
}

fn tell_standing(is_said: bool) -> FactStanding {
    if is_said {
        FactStanding::StillSaid
    } else {
        FactStanding::VersionGone
    }
}

fn local_place(item: &Item) -> Option<&ItemPlace> {
    item.local.as_ref().and_then(|local| local.place.as_ref())
}

/// Whether the bytes `content` tells are those of the file the folder shows as `item`: its
/// local content, or the content it had before a writer that still has it open. Nothing is
/// planned of a file held open for writing, and the close says whether the bytes changed:
/// closed unchanged, the file has that content again; closed changed, another one.
pub fn is_content_of(item: &Item, content: &ContentVersion) -> bool {
    let Some(local) = &item.local else { return false };
    local.content.as_ref() == Some(content) || item.content_before_write.as_ref() == Some(content)
}

impl UploadSession {
    /// The session is the item's while the bytes being sent are the file's.
    pub fn decide_standing(&self, item: &Item) -> FactStanding {
        tell_standing(is_content_of(item, &self.content))
    }
}

impl SpaceRefusal {
    /// The refusal for lack of space is the item's while the bytes that were refused are
    /// the file's.
    pub fn decide_standing(&self, item: &Item) -> FactStanding {
        tell_standing(is_content_of(item, &self.content))
    }
}

impl RefusedVersion {
    /// Whether `item`'s row still says what was refused: the item at that place with those
    /// bytes, or the removal with that stamp.
    pub fn decide_standing(&self, item: &Item) -> FactStanding {
        tell_standing(match self {
            RefusedVersion::Shown(version) => {
                item.local.as_ref().is_some_and(|local| local.place == version.place && version.content.as_ref().map_or(local.content.is_none(), |content| is_content_of(item, content)))
            }
            RefusedVersion::Removal { unsent_since } => is_unsent_removal(item, *unsent_since),
        })
    }
}

impl Refusal {
    /// The refusal is the item's while its row says what was refused.
    pub fn decide_standing(&self, item: &Item) -> FactStanding {
        self.version.decide_standing(item)
    }
}

impl RemovalConfirmation {
    /// The confirmation is the item's while its row is the removal that was confirmed.
    pub fn decide_standing(&self, item: &Item) -> FactStanding {
        tell_standing(is_unsent_removal(item, self.unsent_since))
    }
}

impl TakenName {
    /// The answer is the item's while its local place is the one that was sent.
    pub fn decide_standing(&self, item: &Item) -> FactStanding {
        tell_standing(local_place(item) == Some(&self.place))
    }
}

/// A request to OneDrive that OneDrive confirmed, as the commit of its step says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmedRequest<'a> {
    /// A new item was made or a file's content was sent: `sent` is the content the step was
    /// planned from, none for a directory.
    Upload { sent: Option<&'a ContentVersion> },
    /// The item was moved to the place `to`, which is the place agreed on from now on.
    Move { to: &'a ItemPlace },
    /// The item was removed.
    Removal,
}

/// Everything the steps remember of one item, as one value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemMemory {
    pub upload_session: Option<UploadSession>,
    pub mismatched_item: Option<MismatchedItem>,
    pub temporary_name: Option<TemporaryName>,
    pub failed_tries: Option<FailedTries>,
    pub space_refusal: Option<SpaceRefusal>,
    /// The user confirmed the item's removal when a mass removal was held.
    pub removal_confirmation: Option<RemovalConfirmation>,
    pub refusal: Option<Refusal>,
    pub taken_name: Option<TakenName>,
}

/// The memory of an item in two parts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemMemoryParts {
    /// The facts the item's row still says.
    pub said: ItemMemory,
    /// The facts whose version is not the row's any more.
    pub gone: ItemMemory,
}

impl ItemMemory {
    /// Nothing is remembered.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The memory in two parts: the facts `item`'s row still says, and those whose version
    /// is gone.
    pub fn split_by_standing(self, item: &Item) -> ItemMemoryParts {
        let mut parts = ItemMemoryParts::default();
        fn part<T>(fact: Option<T>, fact_standing: impl FnOnce(&T) -> FactStanding, said: &mut Option<T>, gone: &mut Option<T>) {
            match fact {
                Some(fact) if fact_standing(&fact) == FactStanding::StillSaid => *said = Some(fact),
                fact => *gone = fact,
            }
        }
        let (said, gone) = (&mut parts.said, &mut parts.gone);
        part(self.upload_session, |fact| fact.decide_standing(item), &mut said.upload_session, &mut gone.upload_session);
        part(self.space_refusal, |fact| fact.decide_standing(item), &mut said.space_refusal, &mut gone.space_refusal);
        part(self.refusal, |fact| fact.decide_standing(item), &mut said.refusal, &mut gone.refusal);
        part(self.removal_confirmation, |fact| fact.decide_standing(item), &mut said.removal_confirmation, &mut gone.removal_confirmation);
        part(self.taken_name, |fact| fact.decide_standing(item), &mut said.taken_name, &mut gone.taken_name);
        said.mismatched_item = self.mismatched_item;
        said.temporary_name = self.temporary_name;
        said.failed_tries = self.failed_tries;
        parts
    }

    /// The bytes `from` told are told by `to` from now on, the same bytes: a step computed
    /// their hash. Every fact that holds `from` holds `to`, so that the row goes on saying
    /// it.
    pub(crate) fn carry_content(&mut self, from: &ContentVersion, to: &ContentVersion) {
        let carry = |content: &mut ContentVersion| {
            if content == from {
                *content = to.clone();
            }
        };
        if let Some(session) = self.upload_session.as_mut() {
            carry(&mut session.content);
        }
        if let Some(space_refusal) = self.space_refusal.as_mut() {
            carry(&mut space_refusal.content);
        }
        if let Some(Refusal { version: RefusedVersion::Shown(ItemVersion { content: Some(content), .. }), .. }) = self.refusal.as_mut() {
            carry(content);
        }
    }

    /// OneDrive confirmed `request` of the item: what was kept for that request is
    /// forgotten. The failed tries, whatever the request: it went through. The upload
    /// session that sent the content of an upload: it ended, and is not to be cancelled.
    /// The temporary name, when the move reached the place it led to.
    pub(crate) fn forget_confirmed_request(&mut self, request: ConfirmedRequest<'_>) {
        self.failed_tries = None;
        match request {
            ConfirmedRequest::Upload { sent } => {
                if self.upload_session.as_ref().is_some_and(|session| Some(&session.content) == sent) {
                    self.upload_session = None;
                }
            }
            ConfirmedRequest::Move { to } => {
                if self.temporary_name.as_ref().is_some_and(|temporary_name| &temporary_name.toward == to) {
                    self.temporary_name = None;
                }
            }
            ConfirmedRequest::Removal => {}
        }
    }

    /// The item's binding is another than the one its requests were made for — it took one,
    /// or gave its own up: what OneDrive answered those requests is dropped. The upload
    /// session, which is returned to be cancelled: continued, it would send into an item
    /// that is not this one's any more, or make a second one. The temporary name, the
    /// refusal, the failed tries and the confirmation of a removal. What is of the item's
    /// content and of the place it is at stays: the item an upload left, the refusal for
    /// lack of space, the taken name.
    pub(crate) fn drop_requests_of_binding(&mut self) -> Option<UploadSession> {
        (self.temporary_name, self.refusal, self.failed_tries, self.removal_confirmation) = (None, None, None, None);
        self.upload_session.take()
    }

    /// `directory` gave its binding up, and is a directory OneDrive does not have yet: what
    /// OneDrive answered about sending the item into the directory it had is dropped. The
    /// refusal of a version at a place in it, the name taken in it, and the upload session
    /// whose empty file is in it, which is returned to be cancelled: it would end in the
    /// directory of OneDrive that is another row's now. A refusal of a removal stays: the
    /// item is removed from the same directory of OneDrive.
    pub(crate) fn drop_answers_of_directory(&mut self, directory: LocalItemId) -> Option<UploadSession> {
        let is_in_directory = |place: Option<&ItemPlace>| place.is_some_and(|place| place.parent_local_item_id == directory);
        if matches!(&self.refusal, Some(Refusal { version: RefusedVersion::Shown(version), .. }) if is_in_directory(version.place.as_ref())) {
            self.refusal = None;
        }
        if is_in_directory(self.taken_name.as_ref().map(|taken_name| &taken_name.place)) {
            self.taken_name = None;
        }
        if is_in_directory(self.upload_session.as_ref().and_then(|session| session.empty_file_place.as_ref())) {
            return self.upload_session.take();
        }
        None
    }
}

#[cfg(test)]
mod tests;
