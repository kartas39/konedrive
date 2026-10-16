//! What the steps remember of one item ([`ItemMemory`]): each fact as a value, and whether
//! the item's row still says what a fact was learnt for ([`FactStanding`]). Nothing here
//! reads or writes the store: every decision is a function of a fact and a row.
//!
//! **A fact of a local change that is not sent** — the upload session, the failed tries of
//! a step towards OneDrive for that change, the refusal for lack of space, the refusal for
//! good, the taken name — is learnt under the stamp the item's row has while its local version is not
//! the synced one ([`Item::unsent_since`]). The stamp is given when the two first differ,
//! stays while they differ, and is taken when they agree; the next difference gets another.
//! All such facts of one item are of one stamp, which the memory holds
//! ([`ItemMemory::unsent_since`]), and they are answered only while the row has that stamp:
//! a row with nothing unsent answers none of them, and what was learnt of one unsent change
//! is never answered for the next. One function decides it for all of them
//! ([`ItemMemory::split_by_standing`]).
//!
//! The stamp does not change when the item changes again while it is unsent, so four facts
//! are asked one thing more beside it:
//!
//! - an upload session and a refusal for lack of space, that the bytes the fact holds are
//!   the file's ([`is_content_of`]): its local content, or the content it had before a
//!   writer that still has it open;
//! - a refusal of a shown version, that the item is at the place that was refused, with
//!   those bytes;
//! - a taken name, that the item's local place is the place that was sent.
//!
//! The refusal of a removal is asked that the row is a removal, which the stamp implies.
//! The failed tries are asked nothing more.
//!
//! **A fact of what OneDrive has** holds no version of the item and is not of an unsent
//! change: the temporary name says under which name OneDrive has the item that has the
//! binding, and goes with the binding; the item an upload left is an item of OneDrive, told
//! by its own id.
//!
//! **Every fact names what voids it**, and the voider is something that happens for every
//! way the fact can stop being true. What a fact says of OneDrive is void when OneDrive
//! has answered again about that item: by the step's own next answer, or by a listing that
//! names the item. Each type here says its voiders in one line, with where the store does
//! it. A fact that can name no such voider is not kept in the store: the failed tries of a
//! step towards the folder or of the rows alone, which nothing of a row tells from the
//! tries of the next piece of work, are kept by the plan's task for its run, and the store
//! keeps the tries of what is sent to OneDrive alone ([`FailedTries`]).
//!
//! What becomes of the facts when something other than the row's versions changes is
//! decided here too, each a function of the memory and of typed cases: a request OneDrive
//! confirmed (`ConfirmedRequest`), an operation on the item's binding, a directory that
//! gave its binding up, a content told by its hash from now on.

use std::ffi::OsString;

use konedrive_reason::Reason;

use crate::model::{ContentVersion, Item, ItemPlace, ItemVersion, LocalItemId, OneDriveItemReference};

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
/// stop or a crash. It says that OneDrive has a session at this address, which got so far,
/// for this content of the item.
///
/// *Voided by:* the step's own next answer from the session — a fragment taken
/// (`advance_upload_session`), the session ended or found gone
/// (`forget_ended_upload_session`, and the commit of the upload, which is told the
/// address), given up (`abandon_upload_session`); the row not being the unsent change or
/// the bytes the session sends ([`ItemMemory::split_by_standing`]: taken away to the
/// cancellations by the next write for the item); an operation on the item's binding, or
/// on that of the directory its empty file is in ([`ItemMemory::drop_facts_of_binding`],
/// [`ItemMemory::drop_answers_of_directory`]). No listing names a session. A session whose
/// item has no step that would ask — it is left here, or waits for the user — ends at the
/// time OneDrive said (`expires_at`): the step of another item that finds the name held
/// by it is answered that time and acts on it (`EmptyFileSession::Continued`).
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
#[must_use = "a step sends nothing more to a session that is abandoned"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadSessionStanding {
    /// The session is the item's: its step goes on with it.
    Continued,
    /// Nothing goes on with the session: the item's row went, the row is not the unsent
    /// change the session was opened for, the bytes it sends are not the file's any more,
    /// or the item has another session. Its address is kept to be
    /// cancelled, and the step sends nothing more to it.
    Abandoned,
    /// The address is the session another item's memory keeps: the caller named the wrong
    /// item. Nothing was written and nothing is left to cancel: that session is its
    /// item's, and this step sends nothing to it.
    OfAnotherItem,
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
///
/// *Voided by:* the answer to the request that removes it, or that reads it first —
/// removed or gone (`forget_removed_mismatched_item`, which takes the OneDrive version of
/// the row a listing made of it in the same transaction), changed by another or adopted
/// (`forget_mismatched_item`); the folder having the
/// item (the schema's triggers `mismatched_item_of_folder*`, whoever writes the row); a
/// listing that names it with another tag than the upload's answer gave, or as gone
/// (`forget_mismatched_item_listed_changed` and `forget_mismatched_item`, in the page's
/// transaction). It leaves the file's memory, and is left to the daemon to remove
/// (`mismatched_item_removals`), as soon as the file cannot take care of it any more: its
/// row goes, it comes to be left here, or it gets a binding (the schema's triggers
/// `step_memory_item_gone` and `mismatched_item_file_unable`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MismatchedItem {
    /// The item, with its drive: what its removal is asked of.
    pub item: OneDriveItemReference,
    pub tag: UploadTag,
}

impl MismatchedItem {
    /// What the upload's answer says of the item it made: its cTag, or its eTag when it has
    /// no cTag.
    pub fn of_answer(item: OneDriveItemReference, ctag: Option<&str>, etag: Option<&str>) -> Self {
        let tag = match (ctag, etag) {
            (Some(ctag), _) => UploadTag::CTag(ctag.to_owned()),
            (None, Some(etag)) => UploadTag::ETag(etag.to_owned()),
            (None, None) => UploadTag::NotGiven,
        };
        Self { item, tag }
    }

    /// Whether a listing that names the item with these tags says that it is not what the
    /// upload left any more: the tag the answer gave is given again, and is another one. A
    /// listing that does not give that tag says nothing.
    pub fn is_listed_changed(&self, ctag: Option<&str>, etag: Option<&str>) -> bool {
        match &self.tag {
            UploadTag::CTag(kept) => ctag.is_some_and(|ctag| ctag != kept),
            UploadTag::ETag(kept) => etag.is_some_and(|etag| etag != kept),
            UploadTag::NotGiven => false,
        }
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
/// item from the first request of the exchange until OneDrive confirmed it at another
/// place.
///
/// *Voided by:* the commit of a request OneDrive confirmed the item at another place by —
/// a move ([`ItemMemory::forget_confirmed_request`]), the making of a new item
/// (`forget_temporary_name_unless_at`) — or removed
/// (`forget_temporary_names_of_items_onedrive_lacks`); a listing that names the item at
/// another place (`forget_temporary_name_unless_at`, in the page's transaction) or as
/// gone, by itself or with a directory above it (the sweep at the end of every page). It
/// goes with the binding to the row that has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporaryName {
    /// The temporary name, in the directory of `toward`.
    pub name: OsString,
    /// The local place the item was on its way to in OneDrive. Whether it is still the
    /// item's local place is its step's question.
    pub toward: ItemPlace,
}

impl TemporaryName {
    /// Whether `place`, a place in OneDrive, is the temporary name itself: in the
    /// directory of `toward`, under the temporary name.
    pub fn is_at(&self, place: &ItemPlace) -> bool {
        self.toward.parent_local_item_id == place.parent_local_item_id && self.name == place.name
    }
}

/// The tries of an item's step towards OneDrive that failed: how many, when the next is
/// due, and why the last failed. They are tries of the unsent change the memory's stamp
/// tells ([`ItemMemory::unsent_since`]): every step towards OneDrive is planned from a row
/// that has something unsent.
///
/// *Voided by:* the row's stamp being another or none (the change was sent, taken back, or
/// is another one: [`ItemMemory::split_by_standing`]); the commit of a request OneDrive
/// confirmed while the item stays unsent ([`ItemMemory::forget_confirmed_request`]); an
/// operation on the item's binding ([`ItemMemory::drop_facts_of_binding`]); and the wait
/// alone by its time, or by the user's refresh (`clear_next_tries`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTries {
    pub count: u32,
    /// Unix seconds; `None` when the next try is due at once.
    pub next_try_at: Option<i64>,
    pub last_failure: Reason,
}

/// What became of a failed try given to the store.
#[must_use = "a try that was not counted is of a row that is another piece of work by now: its step does not wait"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedTryStanding {
    /// Counted: so many tries of this unsent change failed, with this one.
    Counted(u32),
    /// The row has another stamp or none: what the step tried to send is not what is
    /// unsent any more. Nothing was written, and the next plan decides.
    ChangeGone,
}

/// A file OneDrive refused as too big for the space it has.
///
/// *Voided by:* a quota read that says the file fits (`forget_fitting_space_refusals`:
/// OneDrive answered again about its space; one that says it does not fit is the free
/// space the refusal holds from then on); the row not being the unsent change or the bytes
/// that were refused ([`ItemMemory::split_by_standing`]); an operation on the item's
/// binding ([`ItemMemory::drop_facts_of_binding`]).
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
    /// The file fits when its size is no more than the free space: a quota read lets a
    /// waiting file go (`docs/design/writes.md` §4.7).
    pub fn decide_fit(&self, free_space: u64) -> SpaceFit {
        if self.content.size() <= free_space {
            SpaceFit::Fits
        } else {
            SpaceFit::StillTooBig
        }
    }
}

/// Whether `item` is a removal made here: OneDrive has the item, the folder does not show
/// it, and the two sides agreed on a place. Such a row is unsent, and has a stamp.
fn is_removal(item: &Item) -> bool {
    item.onedrive_item.is_some() && item.local.is_none() && item.synced_place.is_some()
}

/// What OneDrive refused of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusedVersion {
    /// The local version the folder showed: the item at that place, with that content.
    Shown(ItemVersion),
    /// The removal of the item: the one whose stamp the refusal is given with.
    Removal,
}

/// An item OneDrive refuses for good, until it changes.
///
/// *Voided by:* the row not being what was refused — the unsent change, the version at
/// that place, the removal ([`ItemMemory::split_by_standing`]); an operation on the
/// item's binding, or the directory it was sent into giving its binding up
/// ([`ItemMemory::drop_facts_of_binding`], [`ItemMemory::drop_answers_of_directory`]);
/// and, for what was not allowed, a new sign-in or a refresh (`forget_refusals_for`).
/// Nothing asks OneDrive again while the item is as it was refused, and no listing says that OneDrive
/// would take it now: that is "until the item changes" (`docs/design/sync.md` §7.5), and the one fact here
/// that an answer of OneDrive does not void.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// Why, as OneDrive answered: not allowed, or refused with its message.
    pub reason: Reason,
    /// What was refused.
    pub version: RefusedVersion,
}

/// A name OneDrive answered is taken, though the client's own rule of names did not foresee
/// it. It says two things: that this item was sent under this place — of the unsent
/// change —, and that OneDrive has another item under that name — of what OneDrive has.
///
/// *Voided by:* the row not being that unsent change at that place
/// ([`ItemMemory::split_by_standing`]); an operation on the item's binding or on the
/// directory's; the commit of a move OneDrive confirmed the item at that place by
/// ([`ItemMemory::forget_confirmed_request`]); and OneDrive answering again about the
/// name, whoever writes it into the rows — a row that says the holder has the name is
/// moved, renamed or removed, or another item is put under a name a row already has
/// (the schema's triggers `taken_name_holder_left` and `taken_name_other_item_came`, in
/// the transaction of the page or of the commit), or a listing ends and no row says that
/// OneDrive has an item under the name (`forget_taken_names_onedrive_lacks`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenName {
    /// The place the item was sent to.
    pub place: ItemPlace,
    /// The name of the item OneDrive has there, as OneDrive spells it: the two names are one
    /// name to OneDrive.
    pub holder_name: OsString,
}

/// What became of an item an upload left that was given to the store.
#[must_use = "an item that is not left to remove is one the folder has: its removal is not asked"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchedItemStanding {
    /// Kept with the file: its removal is asked before the file is sent again.
    LeftToRemove,
    /// The folder has the item — a row is bound to it, and the folder shows it or the two
    /// sides agreed on a place of it: nothing was kept, and it is not to be removed.
    OfFolder,
}

/// Whether an item's row still says what a fact of an unsent change was learnt for.
#[must_use = "a fact that is not written was learnt of a row that is another by now"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactStanding {
    /// The row says it: the fact is answered, and a fact given to be written is written.
    StillSaid,
    /// The row is not the unsent change the fact was learnt under, or the version, the
    /// place or the removal the fact holds is not the row's any more: the fact is not
    /// answered, and is not written.
    VersionGone,
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

impl RefusedVersion {
    /// What a refusal is asked beside the stamp: the item is at the place that was refused
    /// with those bytes, or the row is a removal.
    fn is_version_of(&self, item: &Item) -> bool {
        match self {
            RefusedVersion::Shown(version) => {
                item.local.as_ref().is_some_and(|local| local.place == version.place && version.content.as_ref().map_or(local.content.is_none(), |content| is_content_of(item, content)))
            }
            RefusedVersion::Removal => is_removal(item),
        }
    }
}

/// A request to OneDrive that OneDrive confirmed and that may leave the item unsent, as the
/// commit of its step says it. A removal is not among them: its row is a binding alone
/// after it, with no stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmedRequest<'a> {
    /// The content of a file OneDrive has was sent, by the upload session at
    /// `ended_session` when one sent it: that session ended.
    Upload { ended_session: Option<&'a UploadSessionAddress> },
    /// The item was moved to the place `to`, which is the place agreed on from now on.
    Move { to: &'a ItemPlace },
}

/// Everything the steps remember of one item, as one value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemMemory {
    /// The stamp of the unsent change that the facts of such a change were learnt under
    /// ([`Item::unsent_since`]): the upload session, the failed tries, the refusal for lack
    /// of space, the refusal and the taken name. None while
    /// none of them is kept.
    pub unsent_since: Option<u64>,
    pub upload_session: Option<UploadSession>,
    pub failed_tries: Option<FailedTries>,
    pub space_refusal: Option<SpaceRefusal>,
    pub refusal: Option<Refusal>,
    pub taken_name: Option<TakenName>,
    /// Of what OneDrive has, under no stamp.
    pub mismatched_item: Option<MismatchedItem>,
    /// Of what OneDrive has, under no stamp.
    pub temporary_name: Option<TemporaryName>,
}

/// The memory of an item in two parts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemMemoryParts {
    /// The facts the item's row still says.
    pub said: ItemMemory,
    /// The facts the row does not say any more.
    pub gone: ItemMemory,
}

impl ItemMemory {
    /// Nothing is remembered.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether a fact of an unsent change is kept.
    fn holds_fact_of_unsent_change(&self) -> bool {
        self.upload_session.is_some() || self.failed_tries.is_some() || self.space_refusal.is_some() || self.refusal.is_some() || self.taken_name.is_some()
    }

    /// The memory holds the stamp only while it holds a fact that was learnt under it.
    pub(crate) fn keep_stamp_only_with_its_facts(&mut self) {
        if !self.holds_fact_of_unsent_change() {
            self.unsent_since = None;
        }
    }

    /// The memory in two parts: the facts `item`'s row still says, and those it does not.
    /// The one decision of whether a fact is answered.
    ///
    /// A fact of an unsent change is said while the row has the stamp the memory holds, and
    /// what the fact is asked beside the stamp holds too (the module's words say what, for
    /// each). The temporary name and the item an upload left are of what OneDrive has, and
    /// are said as long as they are kept: what voids them is done where OneDrive answers.
    pub fn split_by_standing(self, item: &Item) -> ItemMemoryParts {
        let mut parts = ItemMemoryParts::default();
        let is_same_unsent_change = self.unsent_since.is_some() && self.unsent_since == item.unsent_since;
        fn part<T>(fact: Option<T>, is_said: impl FnOnce(&T) -> bool, said: &mut Option<T>, gone: &mut Option<T>) {
            match fact {
                Some(fact) if is_said(&fact) => *said = Some(fact),
                fact => *gone = fact,
            }
        }
        let (said, gone) = (&mut parts.said, &mut parts.gone);
        part(self.upload_session, |fact| is_same_unsent_change && is_content_of(item, &fact.content), &mut said.upload_session, &mut gone.upload_session);
        part(self.failed_tries, |_| is_same_unsent_change, &mut said.failed_tries, &mut gone.failed_tries);
        part(self.space_refusal, |fact| is_same_unsent_change && is_content_of(item, &fact.content), &mut said.space_refusal, &mut gone.space_refusal);
        part(self.refusal, |fact| is_same_unsent_change && fact.version.is_version_of(item), &mut said.refusal, &mut gone.refusal);
        part(self.taken_name, |fact| is_same_unsent_change && local_place(item) == Some(&fact.place), &mut said.taken_name, &mut gone.taken_name);
        (said.unsent_since, gone.unsent_since) = (self.unsent_since, self.unsent_since);
        said.keep_stamp_only_with_its_facts();
        gone.keep_stamp_only_with_its_facts();
        said.mismatched_item = self.mismatched_item;
        said.temporary_name = self.temporary_name;
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

    /// OneDrive confirmed `request` of the item, an item OneDrive has. The stamp says
    /// nothing of it where the item is still unsent after it — it was changed here again
    /// while the request ran —, so what the request ends is forgotten here. The failed
    /// tries of an upload and of a move: the request went through. The taken name of the
    /// place a move was confirmed to: OneDrive has the item there. The temporary name,
    /// unless the move was to that very name: OneDrive has the item at the place it
    /// confirmed, or does not have it any more. The upload session that ended is not here:
    /// it is told by its address, and taken by it.
    pub(crate) fn forget_confirmed_request(&mut self, request: ConfirmedRequest<'_>) {
        match request {
            ConfirmedRequest::Upload { .. } => self.failed_tries = None,
            ConfirmedRequest::Move { to } => {
                self.failed_tries = None;
                if self.taken_name.as_ref().is_some_and(|taken_name| &taken_name.place == to) {
                    self.taken_name = None;
                }
                if !self.temporary_name.as_ref().is_some_and(|temporary_name| temporary_name.is_at(to)) {
                    self.temporary_name = None;
                }
            }
        }
        self.keep_stamp_only_with_its_facts();
    }

    /// OneDrive said that it has the item at `place`: a temporary name that is not that
    /// place's is not the name OneDrive has the item under.
    pub(crate) fn forget_temporary_name_unless_at(&mut self, place: &ItemPlace) {
        if !self.temporary_name.as_ref().is_some_and(|temporary_name| temporary_name.is_at(place)) {
            self.temporary_name = None;
        }
    }

    /// An operation was done on the item's binding — it was given one, took one over, was
    /// merged, or gave its own up: every fact of an unsent change is dropped, whatever the
    /// stamp. Each is an answer of OneDrive, or of the user, about the item as it was bound
    /// before. The upload session is returned to be cancelled: continued, it would send
    /// into an item that is not this one's any more, or make a second one. The temporary
    /// name is returned too: it goes with the binding. The item an upload left stays: it is
    /// in OneDrive whatever this item is bound to.
    pub(crate) fn drop_facts_of_binding(&mut self) -> (Option<UploadSession>, Option<TemporaryName>) {
        (self.failed_tries, self.space_refusal, self.refusal, self.taken_name, self.unsent_since) = (None, None, None, None, None);
        (self.upload_session.take(), self.temporary_name.take())
    }

    /// `directory` gave its binding up, and is a directory OneDrive does not have yet: what
    /// OneDrive answered about sending the item into the directory it had is dropped. The
    /// refusal of a version at a place in it, the name taken in it, and the upload session
    /// whose empty file is in it, which is returned to be cancelled: it would end in the
    /// directory of OneDrive that is another row's now. A refusal and a confirmation of a
    /// removal stay: the item is removed from the same directory of OneDrive.
    pub(crate) fn drop_answers_of_directory(&mut self, directory: LocalItemId) -> Option<UploadSession> {
        let is_in_directory = |place: Option<&ItemPlace>| place.is_some_and(|place| place.parent_local_item_id == directory);
        if matches!(&self.refusal, Some(Refusal { version: RefusedVersion::Shown(version), .. }) if is_in_directory(version.place.as_ref())) {
            self.refusal = None;
        }
        if is_in_directory(self.taken_name.as_ref().map(|taken_name| &taken_name.place)) {
            self.taken_name = None;
        }
        let session = if is_in_directory(self.upload_session.as_ref().and_then(|session| session.empty_file_place.as_ref())) { self.upload_session.take() } else { None };
        self.keep_stamp_only_with_its_facts();
        session
    }
}

#[cfg(test)]
mod tests;
