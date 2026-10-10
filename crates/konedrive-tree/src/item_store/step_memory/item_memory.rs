//! What the steps remember of one item ([`ItemMemory`]): each fact as a value, and whether
//! the item's row still says what a fact was learnt for ([`FactStanding`]). Nothing here
//! reads or writes the store: every decision is a function of a fact and a row.
//!
//! A fact that holds a version or a place is of the item as it was then. The row is the
//! truth of what the item is; a fact is answered only while the row still says it:
//!
//! - an upload session and a file too big for the space, while the file's local content is
//!   the content the fact holds;
//! - a refusal, while the item's local version is the version that was refused: its place and
//!   its content, or the folder not showing it for a refused removal;
//! - a temporary name and a taken name, while the item's local place is the place the fact
//!   holds;
//! - the confirmation of a removal, while the row is a removal that is not sent: an item
//!   OneDrive has, which the folder does not show, with a place the two sides agreed on.
//!
//! The failed tries and the item an upload left with other content hold no version of the
//! item: they are answered as long as they are kept.

use std::ffi::OsString;

use konedrive_reason::Reason;

use crate::model::{ContentVersion, Item, ItemPlace, ItemVersion, OneDriveItemId};

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
    /// The content version being sent: the session is the item's while the file's local
    /// content is this one.
    pub content: ContentVersion,
    /// For the session of a new file: the place in OneDrive its empty file holds until the
    /// upload ends, the directory named by its local item id. `None` for a session that
    /// sends into an item OneDrive has.
    pub placeholder: Option<ItemPlace>,
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
pub struct MismatchedUpload {
    pub item_id: OneDriveItemId,
    pub tag: UploadTag,
}

impl MismatchedUpload {
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
    /// not something another client wrote since.
    pub fn is_still_in_onedrive(&self, ctag: Option<&str>, etag: Option<&str>) -> bool {
        match &self.tag {
            UploadTag::CTag(kept) => ctag == Some(kept.as_str()),
            UploadTag::ETag(kept) => etag == Some(kept.as_str()),
            UploadTag::NotGiven => true,
        }
    }
}

/// The temporary name an exchange of names is going through: the name an item is given in
/// OneDrive while the name it takes is still another item's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporaryName {
    /// The temporary name, in the directory of `toward`.
    pub name: OsString,
    /// The local place the item is on its way to in OneDrive.
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
pub struct TooBigForSpace {
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

impl TooBigForSpace {
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

/// An item OneDrive refuses for good, until it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// Why, as OneDrive answered: not allowed, or refused with its message.
    pub reason: Reason,
    /// The local version that was refused; `None` when the folder did not show the item,
    /// and what was refused is its removal.
    pub version: Option<ItemVersion>,
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

/// The kinds of fact a step remembers of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepFact {
    UploadSession,
    MismatchedUpload,
    TemporaryName,
    FailedTries,
    TooBigForSpace,
    RemovalConfirmation,
    Refusal,
    TakenName,
}

/// Whether an item's row still says what a fact was learnt for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactStanding {
    /// The row says it: the fact is answered, and a fact given to be written is written.
    StillSaid,
    /// The version or the place the fact holds is not the row's any more: the fact is not
    /// answered, and is not written.
    VersionGone,
}

fn standing(is_said: bool) -> FactStanding {
    if is_said {
        FactStanding::StillSaid
    } else {
        FactStanding::VersionGone
    }
}

fn local_content(item: &Item) -> Option<&ContentVersion> {
    item.local.as_ref().and_then(|local| local.content.as_ref())
}

fn local_place(item: &Item) -> Option<&ItemPlace> {
    item.local.as_ref().and_then(|local| local.place.as_ref())
}

impl UploadSession {
    /// The session is the item's while the file's local content is the content being sent.
    pub fn decide_standing(&self, item: &Item) -> FactStanding {
        standing(local_content(item) == Some(&self.content))
    }
}

impl TooBigForSpace {
    /// The refusal for lack of space is the item's while the file's local content is the
    /// content that was refused.
    pub fn decide_standing(&self, item: &Item) -> FactStanding {
        standing(local_content(item) == Some(&self.content))
    }
}

impl Refusal {
    /// The refusal is the item's while its local version is the one that was refused.
    pub fn decide_standing(&self, item: &Item) -> FactStanding {
        standing(item.local == self.version)
    }
}

impl TemporaryName {
    /// The temporary name is the item's while its local place is the one it was on its way
    /// to.
    pub fn decide_standing(&self, item: &Item) -> FactStanding {
        standing(local_place(item) == Some(&self.toward))
    }
}

impl TakenName {
    /// The answer is the item's while its local place is the one that was sent.
    pub fn decide_standing(&self, item: &Item) -> FactStanding {
        standing(local_place(item) == Some(&self.place))
    }
}

/// The confirmation of a removal is the item's while its row is a removal that is not sent:
/// OneDrive has the item, the folder does not show it, and the two sides agreed on a place.
pub fn decide_removal_confirmation_standing(item: &Item) -> FactStanding {
    standing(item.onedrive_item.is_some() && item.local.is_none() && item.synced_place.is_some())
}

/// Everything the steps remember of one item, as one value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemMemory {
    pub upload_session: Option<UploadSession>,
    pub mismatched_upload: Option<MismatchedUpload>,
    pub temporary_name: Option<TemporaryName>,
    pub failed_tries: Option<FailedTries>,
    pub too_big_for_space: Option<TooBigForSpace>,
    /// The user confirmed the item's removal when a mass removal was held.
    pub is_removal_confirmed: bool,
    pub refusal: Option<Refusal>,
    pub taken_name: Option<TakenName>,
}

impl ItemMemory {
    /// Nothing is remembered.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The memory in two parts: the facts `item`'s row still says, and those whose version
    /// is gone.
    pub fn split_by_standing(self, item: &Item) -> (ItemMemory, ItemMemory) {
        let (mut said, mut gone) = (ItemMemory::default(), ItemMemory::default());
        fn part<T>(fact: Option<T>, fact_standing: impl FnOnce(&T) -> FactStanding, said: &mut Option<T>, gone: &mut Option<T>) {
            match fact {
                Some(fact) if fact_standing(&fact) == FactStanding::StillSaid => *said = Some(fact),
                fact => *gone = fact,
            }
        }
        part(self.upload_session, |fact| fact.decide_standing(item), &mut said.upload_session, &mut gone.upload_session);
        part(self.too_big_for_space, |fact| fact.decide_standing(item), &mut said.too_big_for_space, &mut gone.too_big_for_space);
        part(self.refusal, |fact| fact.decide_standing(item), &mut said.refusal, &mut gone.refusal);
        part(self.temporary_name, |fact| fact.decide_standing(item), &mut said.temporary_name, &mut gone.temporary_name);
        part(self.taken_name, |fact| fact.decide_standing(item), &mut said.taken_name, &mut gone.taken_name);
        if self.is_removal_confirmed {
            match decide_removal_confirmation_standing(item) {
                FactStanding::StillSaid => said.is_removal_confirmed = true,
                FactStanding::VersionGone => gone.is_removal_confirmed = true,
            }
        }
        said.mismatched_upload = self.mismatched_upload;
        said.failed_tries = self.failed_tries;
        (said, gone)
    }
}

#[cfg(test)]
mod tests;
