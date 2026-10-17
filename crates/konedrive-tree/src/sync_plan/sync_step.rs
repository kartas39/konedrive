//! One thing the sync plan does: a request to OneDrive, a change of the folder, or a change of
//! the rows alone. Every step carries the versions of the row it was planned from
//! (`ItemVersions`): it runs outside the lock, and its commit, under the lock, changes
//! nothing unless those are still the row's.
//!
//! A case holds what the step needs to run without deciding again, and nothing it could read
//! off the row at its commit: the kind of the item, whether its content is on disk, its
//! pin. What it holds of another row (the binding of the directory the item is sent into) is
//! what the plan read there.

use std::ffi::OsString;

use crate::model::{ContentPresence, ContentVersion, Item, ItemPlace, ItemVersion, LocalItemId, OneDriveItemReference};

/// The binding and the three versions of one item, with the eTag of the OneDrive one,
/// whether its content is on disk, and the stamp of what is unsent of it, as its row had
/// them when a step was planned from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemVersions {
    pub local_item_id: LocalItemId,
    pub onedrive_item: Option<OneDriveItemReference>,
    pub local: Option<ItemVersion>,
    pub onedrive: Option<ItemVersion>,
    /// What a request to OneDrive about the item is guarded with.
    pub onedrive_etag: Option<String>,
    pub synced_place: Option<ItemPlace>,
    pub synced_content: Option<ContentVersion>,
    /// Whether the whole content of a file was on disk: a step planned from a copy on disk
    /// is not the step for a file whose copy went.
    pub content_presence: ContentPresence,
    /// The stamp the row had while its local version was not the synced one
    /// ([`Item::unsent_since`]), none when nothing was unsent: which unsent change the step
    /// was planned for. What the step learns of that change it gives the store with this
    /// stamp, and the store keeps it only for a row that still has it.
    pub unsent_since: Option<u64>,
}

impl ItemVersions {
    /// Those of `item`, as its row is now.
    pub fn of(item: &Item) -> Self {
        Self {
            local_item_id: item.local_item_id,
            onedrive_item: item.onedrive_item.clone(),
            local: item.local.clone(),
            onedrive: item.onedrive.clone(),
            onedrive_etag: item.onedrive_etag.clone(),
            synced_place: item.synced_place.clone(),
            synced_content: item.synced_content.clone(),
            content_presence: item.content_presence,
            unsent_since: item.unsent_since,
        }
    }

    /// Whether `item`, a row read now, is the same item with the same binding, the same
    /// three versions and the same eTag, its content on disk or not as it was, and the same
    /// unsent change or none: what a step asks at its commit.
    pub fn are_those_of(&self, item: &Item) -> bool {
        *self == Self::of(item)
    }

    /// Whether `item`, a row read now, has the same binding, the same OneDrive version and
    /// eTag and the same place and content agreed on, whatever the folder shows of it now
    /// and whichever item of the folder it is: what a step that made, moved or filled the
    /// item in OneDrive asks at its commit, of the row that has the binding then. A program
    /// is free to change the item here while the request runs, and to save another file
    /// over it, which takes the binding.
    pub fn are_onedrive_and_synced_those_of(&self, item: &Item) -> bool {
        (&self.onedrive_item, &self.onedrive, &self.onedrive_etag, &self.synced_place, &self.synced_content) == (&item.onedrive_item, &item.onedrive, &item.onedrive_etag, &item.synced_place, &item.synced_content)
    }
}

/// One thing to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStep {
    /// Towards OneDrive: one request to Graph.
    OneDrive(OneDriveStep),
    /// Towards the folder: a change of the backing directory and of the local version.
    Folder(FolderStep),
    /// A change of the rows alone: no disk and no request.
    Row(RowStep),
}

impl SyncStep {
    /// The item the step is of; of two items that are merged, the one the folder shows.
    pub fn local_item_id(&self) -> LocalItemId {
        match self {
            SyncStep::OneDrive(step) => step.item().local_item_id,
            SyncStep::Folder(step) => step.item().local_item_id,
            SyncStep::Row(step) => step.item().local_item_id,
        }
    }
}

/// Which step towards the folder a step is, without the versions, the name and the place
/// it carries: what tells one step of an item from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FolderStepKind {
    ShowItem,
    MoveItem,
    TakeOneDriveContent,
    RemoveItem,
    DownloadContent,
    RenameToConflictCopy,
    TakeMoveBack,
    MoveToTemporaryName,
}

/// Which step of the rows alone a step is, without the versions it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowStepKind {
    GiveUpBinding,
    MergeItems,
    AgreeOnRemoval,
    ForgetSyncedVersion,
    HashLocalContent,
    AgreeOnPlace,
    AgreeOnContent,
}

/// A request to OneDrive. What is sent of the item itself is in its local version
/// (`item.local`); the request is guarded with `item.onedrive_etag` where a case says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneDriveStep {
    /// A directory made here is made in OneDrive, in the directory `parent` under `name`.
    /// `name` is the name here, or a temporary name; what the confirmation writes as
    /// OneDrive's place and as the place agreed on is the place that was sent.
    MakeDirectory { item: ItemVersions, parent: OneDriveItemReference, name: OsString },
    /// A file made here is sent, with its content, into the directory `parent` under `name`;
    /// the name as for a directory.
    SendNewFile { item: ItemVersions, parent: OneDriveItemReference, name: OsString },
    /// The content of a file OneDrive has, changed here, is sent into that item where
    /// OneDrive has it, guarded by the cTag of the content OneDrive has as the row knows it
    /// (`docs/design/writes.md` §4.2). No name and no directory is sent.
    SendContent { item: ItemVersions },
    /// A move or a rename made here is sent, guarded by the eTag: the item goes into the
    /// directory `parent` under `name`. The name is the local one, or a temporary name items
    /// exchange their names through: what the confirmation writes as OneDrive's place and
    /// as the place agreed on is the place that was sent, so that an item sent to a
    /// temporary name is still moved here, and its move is sent again once its name is free.
    SendMove { item: ItemVersions, parent: OneDriveItemReference, name: OsString },
    /// The removal of a file that was removed here is sent, guarded by the eTag.
    SendFileRemoval { item: ItemVersions },
    /// The removal of a directory is sent, with no guard: it removes all OneDrive has in
    /// the directory, and stands where OneDrive renamed or moved that since. `below` is
    /// all the rows say OneDrive has in it and below it, at any depth, each of which was
    /// shown here and removed here, and none a file whose content OneDrive changed since:
    /// a removal is planned on that condition alone, the step asks
    /// it again before its request leaves, and the commit once more
    /// ([`ItemStore::can_directory_removal_be_sent`](crate::ItemStore::can_directory_removal_be_sent)).
    /// Those rows are left with their binding alone by the confirmation, as the
    /// directory's is.
    SendDirectoryRemoval { item: ItemVersions, below: Vec<ItemVersions> },
}

impl OneDriveStep {
    /// The versions of the item the request is about.
    pub fn item(&self) -> &ItemVersions {
        match self {
            OneDriveStep::MakeDirectory { item, .. }
            | OneDriveStep::SendNewFile { item, .. }
            | OneDriveStep::SendContent { item }
            | OneDriveStep::SendMove { item, .. }
            | OneDriveStep::SendFileRemoval { item }
            | OneDriveStep::SendDirectoryRemoval { item, .. } => item,
        }
    }
}

/// A change of the folder: of the backing directory, where the item has something in it,
/// and of the item's local version, with what the two sides agree on from then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderStep {
    /// An item of OneDrive the folder does not show appears at OneDrive's place
    /// (`item.onedrive`), its content not on disk: one that is new in OneDrive, and a file
    /// removed here that comes back because OneDrive changed its content.
    ShowItem { item: ItemVersions },
    /// The item is moved here to OneDrive's place, the copy of a file and what is in a
    /// directory with it.
    MoveItem { item: ItemVersions },
    /// The file shows OneDrive's content from now on: the copy on disk, when it has one, goes.
    TakeOneDriveContent { item: ItemVersions },
    /// The folder shows the item no more: a file's copy on disk goes with it, and a
    /// directory goes when the folder shows nothing in it.
    RemoveItem { item: ItemVersions },
    /// The content of a pinned file that is not on disk is downloaded.
    DownloadContent { item: ItemVersions },
    /// The user's own item is renamed here to `name`, a conflict copy's name, in the
    /// directory the folder shows it in: OneDrive has another item under the name it had,
    /// as OneDrive compares names, or an item of the folder that was unsent earlier is to
    /// have it there. Whether the new name is ignored is asked at the commit. Nothing agreed
    /// on changes: the plan made after it sends the item under its new name.
    RenameToConflictCopy { item: ItemVersions, name: OsString },
    /// A move made here is taken back: the item is at OneDrive's place (`item.onedrive`)
    /// again, because sending the move would put a directory into what OneDrive has in it.
    TakeMoveBack { item: ItemVersions },
    /// The item is moved here to `place`, a temporary name in the directory OneDrive has it
    /// in, or, while the folder cannot bring it there, in the directory the folder shows it
    /// in: items OneDrive moved in a circle cannot each go to its place while another is in
    /// the way. That place is the place agreed on from then, so that nothing of it is sent,
    /// and the plan made after it moves the item to OneDrive's place.
    MoveToTemporaryName { item: ItemVersions, place: ItemPlace },
}

impl FolderStep {
    pub fn kind(&self) -> FolderStepKind {
        match self {
            FolderStep::ShowItem { .. } => FolderStepKind::ShowItem,
            FolderStep::MoveItem { .. } => FolderStepKind::MoveItem,
            FolderStep::TakeOneDriveContent { .. } => FolderStepKind::TakeOneDriveContent,
            FolderStep::RemoveItem { .. } => FolderStepKind::RemoveItem,
            FolderStep::DownloadContent { .. } => FolderStepKind::DownloadContent,
            FolderStep::RenameToConflictCopy { .. } => FolderStepKind::RenameToConflictCopy,
            FolderStep::TakeMoveBack { .. } => FolderStepKind::TakeMoveBack,
            FolderStep::MoveToTemporaryName { .. } => FolderStepKind::MoveToTemporaryName,
        }
    }

    /// The versions of the item the folder changes.
    pub fn item(&self) -> &ItemVersions {
        match self {
            FolderStep::ShowItem { item }
            | FolderStep::MoveItem { item }
            | FolderStep::TakeOneDriveContent { item }
            | FolderStep::RemoveItem { item }
            | FolderStep::DownloadContent { item }
            | FolderStep::RenameToConflictCopy { item, .. }
            | FolderStep::TakeMoveBack { item }
            | FolderStep::MoveToTemporaryName { item, .. } => item,
        }
    }
}

/// A change of the rows alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowStep {
    /// The item keeps its id, its place and its content and is an item OneDrive does not
    /// have; a new row takes the binding and what OneDrive has
    /// ([`ItemStore::give_up_binding`](crate::ItemStore::give_up_binding)). The next plan
    /// sends the item as a new one.
    GiveUpBinding { item: ItemVersions },
    /// A new item here (`local`) and a new item in OneDrive (`onedrive`) are one
    /// ([`ItemStore::merge_items`](crate::ItemStore::merge_items)), and what they were
    /// merged for is agreed on: the place, and for two files the content.
    MergeItems { local: ItemVersions, onedrive: ItemVersions },
    /// Both sides removed the item: that is agreed on. What they agreed on before is
    /// forgotten, and the row is left with its binding alone; it goes later, by the guard
    /// against an outdated listing.
    AgreeOnRemoval { item: ItemVersions },
    /// The folder does not show the item, and what the two sides agreed on is forgotten:
    /// the row is that of an item of OneDrive the folder never showed. For a file removed
    /// here whose content OneDrive changed, where the folder cannot show it: it cannot come
    /// back, and its removal is not sent. And for a directory removed here while OneDrive
    /// has below it what the folder never showed — an item, or the content of a file that
    /// was removed here —: its removal is of the folder alone, and the plan made after it
    /// shows the directory again, where the folder can show it, with what it never showed.
    /// The mount does not let a program remove such a directory (`rmdir` answers
    /// `ENOTEMPTY` while OneDrive has in it what the folder does not show): the plan
    /// meets this case only when the item was listed between that check and the row.
    ForgetSyncedVersion { item: ItemVersions },
    /// The local content is hashed, and the hash written into the local content version:
    /// the plan made after it can ask whether the two sides have the same content.
    HashLocalContent { item: ItemVersions },
    /// The two sides have the item at one place already: that place is the synced one.
    AgreeOnPlace { item: ItemVersions },
    /// The two sides have the same content already: OneDrive's content version, with the
    /// time the content has here, is the local and the synced one.
    AgreeOnContent { item: ItemVersions },
}

impl RowStep {
    pub fn kind(&self) -> RowStepKind {
        match self {
            RowStep::GiveUpBinding { .. } => RowStepKind::GiveUpBinding,
            RowStep::MergeItems { .. } => RowStepKind::MergeItems,
            RowStep::AgreeOnRemoval { .. } => RowStepKind::AgreeOnRemoval,
            RowStep::ForgetSyncedVersion { .. } => RowStepKind::ForgetSyncedVersion,
            RowStep::HashLocalContent { .. } => RowStepKind::HashLocalContent,
            RowStep::AgreeOnPlace { .. } => RowStepKind::AgreeOnPlace,
            RowStep::AgreeOnContent { .. } => RowStepKind::AgreeOnContent,
        }
    }

    /// The versions of the item whose row changes; of two items that are merged, the one
    /// the folder shows.
    pub fn item(&self) -> &ItemVersions {
        match self {
            RowStep::GiveUpBinding { item }
            | RowStep::MergeItems { local: item, .. }
            | RowStep::AgreeOnRemoval { item }
            | RowStep::ForgetSyncedVersion { item }
            | RowStep::HashLocalContent { item }
            | RowStep::AgreeOnPlace { item }
            | RowStep::AgreeOnContent { item } => item,
        }
    }
}

#[cfg(test)]
mod tests;
