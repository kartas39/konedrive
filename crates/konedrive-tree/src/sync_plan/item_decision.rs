//! What the two sides' changes mean for one item by itself: the three tables of the sync
//! plan (whether the item exists, its place, its content), cell by cell. A decision of typed
//! cases: it is given the item's row and what the plan knows about it as values
//! ([`ItemCase`]) and answers one step, one wait, or that nothing is done. It reads no other
//! row and decides nothing between items.
//!
//! One step per item: where an item needs more than one (a move and new content), the
//! place goes first, and the plan made after it decides the rest.

use konedrive_reason::Reason;

use super::sending_obstacle::{find_sending_obstacle, Sending, SendingConditions, SendingObstacle};
use super::side_change::{compare_local_content, ContentChange, PlaceChange, SideChange};
use super::sync_step::{FolderStep, ItemVersions, OneDriveStep, RowStep, SyncStep};
use super::{ExclusionReason, WaitReason};
use crate::model::{ContentPresence, FolderVisibility, Item, ItemKind, LocalItemId, OneDriveItemReference};

/// What the plan answers for one item by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemDecision {
    /// The two sides agree, or the row is no item: a row with a binding alone.
    NothingToDo,
    Step(SyncStep),
    Wait(WaitReason),
    /// A rule leaves the item out: nothing is done for it, and it waits for nothing.
    Exclusion(ExclusionReason),
    /// The row, or what the plan was given about it, says what no operation leaves.
    Fault,
}

/// The directory the folder shows an item in, as OneDrive knows that directory: where a
/// step towards OneDrive sends the item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalDirectoryBinding {
    Bound(OneDriveItemReference),
    /// The directory has no binding, or its row was not given: which directory.
    NotBound(LocalItemId),
    /// The folder does not show the item.
    NoLocalPlace,
}

/// Whether the folder can take the place OneDrive has an item at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OneDrivePlaceHere {
    /// The folder shows the directory, and nothing but the item itself has the name there.
    Free,
    /// The folder does not show the directory, or its row was not given: which directory.
    DirectoryNotShown(LocalItemId),
    /// The folder shows another item under that name there: which.
    NameTaken(LocalItemId),
    /// The name is not the name of a directory entry.
    NameUnusable,
    /// OneDrive has no place for the item.
    NoOneDrivePlace,
}

/// One item as the plan decides on it: its row, and what the plan was given or found among
/// the other rows about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemCase<'a> {
    pub(crate) item: &'a Item,
    /// Whether the folder can show the item as OneDrive has it.
    pub(crate) folder_visibility: FolderVisibility,
    /// A listing is being written: what the rows say OneDrive removed, or the folder
    /// cannot show, a later page may take back.
    pub(crate) is_listing_under_way: bool,
    pub(crate) has_running_step: bool,
    pub(crate) is_open_for_writing: bool,
    pub(crate) is_pinned: bool,
    pub(crate) local_directory: LocalDirectoryBinding,
    pub(crate) onedrive_place_here: OneDrivePlaceHere,
    /// One of the items the folder shows in the item, a directory.
    pub(crate) first_shown_in_it: Option<LocalItemId>,
    pub(crate) sending: SendingConditions,
}

impl ItemCase<'_> {
    fn versions(&self) -> ItemVersions {
        ItemVersions::of(self.item)
    }
}

fn onedrive_step(step: OneDriveStep) -> ItemDecision {
    ItemDecision::Step(SyncStep::OneDrive(step))
}

fn folder_step(step: FolderStep) -> ItemDecision {
    ItemDecision::Step(SyncStep::Folder(step))
}

fn row_step(step: RowStep) -> ItemDecision {
    ItemDecision::Step(SyncStep::Row(step))
}

/// What the plan answers for the item of `case`. The first that holds:
///
/// - the root of the folder is never planned, and a row the rules of the rows refuse is a
///   fault;
/// - a row with no version on either side and nothing agreed on is no item;
/// - an item with a step running waits for it;
/// - an item a program holds open for writing is not planned at all;
/// - otherwise the tables decide ([`decide_existence`]).
pub(crate) fn decide_item(case: &ItemCase<'_>) -> ItemDecision {
    let item = case.item;
    if item.local_item_id == LocalItemId::ROOT {
        return ItemDecision::NothingToDo;
    }
    if item.onedrive_item.is_none() && (item.onedrive.is_some() || item.synced_place.is_some() || item.synced_content.is_some()) {
        return ItemDecision::Fault;
    }
    let (here, in_onedrive) = (SideChange::of_local(item), SideChange::of_onedrive(item));
    if (here, in_onedrive) == (SideChange::NeverWas, SideChange::NeverWas) {
        return ItemDecision::NothingToDo;
    }
    if case.has_running_step {
        return ItemDecision::Wait(WaitReason::RunningStep);
    }
    if case.is_open_for_writing {
        return ItemDecision::Wait(WaitReason::User(Reason::OpenForWriting));
    }
    decide_existence(case, here, in_onedrive)
}

/// The table "whether the item exists": what was done here against what was done in
/// OneDrive.
pub(crate) fn decide_existence(case: &ItemCase<'_>, here: SideChange, in_onedrive: SideChange) -> ItemDecision {
    use SideChange::{IsThere, NeverWas, Removed};
    match (here, in_onedrive) {
        (IsThere { place: here_place, content: here_content }, IsThere { place: onedrive_place, content: onedrive_content }) => {
            decide_item_on_both_sides(case, (here_place, here_content), (onedrive_place, onedrive_content))
        }
        (IsThere { place, content }, Removed) => decide_removed_in_onedrive(case, place, content),
        (IsThere { .. }, NeverWas) if case.item.onedrive_item.is_some() => decide_bound_item_onedrive_never_listed(case),
        (IsThere { .. }, NeverWas) => decide_new_here(case),
        (Removed, IsThere { content, .. }) => decide_removed_here(case, content),
        (Removed, Removed) if case.is_listing_under_way => ItemDecision::Wait(WaitReason::Listing),
        (Removed, Removed) => row_step(RowStep::DropRow { item: case.versions() }),
        (NeverWas, IsThere { .. }) => show_onedrive_item(case),
        (NeverWas, NeverWas) => ItemDecision::NothingToDo,
        // Both sides are read against one synced version: where one side removed the item
        // something was agreed on, and the other cannot have never had it.
        (Removed, NeverWas) | (NeverWas, Removed) => ItemDecision::Fault,
    }
}

/// The folder shows the item no more; a directory waits for what the folder shows in it.
fn remove_item_here(case: &ItemCase<'_>) -> ItemDecision {
    match (case.item.kind, case.first_shown_in_it) {
        (ItemKind::Directory, Some(shown_in_it)) => ItemDecision::Wait(WaitReason::AnotherItem(shown_in_it)),
        _ => folder_step(FolderStep::RemoveItem { item: case.versions() }),
    }
}

/// Is there here, removed in OneDrive. Nothing is done while a listing is under way.
///
/// - A file neither moved nor changed here goes, its clean copy with it.
/// - A file whose content was changed here is sent again as a new item.
/// - A file moved here is sent again at its new place when its content is on disk, and goes
///   when it is not.
/// - A directory goes, moved here or not, once the folder shows nothing in it.
fn decide_removed_in_onedrive(case: &ItemCase<'_>, place: PlaceChange, content: ContentChange) -> ItemDecision {
    if case.is_listing_under_way {
        return ItemDecision::Wait(WaitReason::Listing);
    }
    match (case.item.kind, place, content, case.item.content_presence) {
        (ItemKind::Directory, _, _, _) => remove_item_here(case),
        (ItemKind::File, _, ContentChange::Changed | ContentChange::NotKnownWithoutHash, _) | (ItemKind::File, PlaceChange::Moved, ContentChange::Same, ContentPresence::OnDisk) => {
            row_step(RowStep::GiveUpBinding { item: case.versions() })
        }
        (ItemKind::File, PlaceChange::Same, ContentChange::Same, _) | (ItemKind::File, PlaceChange::Moved, ContentChange::Same, ContentPresence::NotOnDisk) => remove_item_here(case),
    }
}

/// The folder shows an item that has a binding, and no listing said what OneDrive has of
/// it: a store that was lost, or a daemon that died between marking an uploaded file and
/// its row. A listing under way will say; with none, OneDrive does not have the item, and
/// it is sent as a new one.
fn decide_bound_item_onedrive_never_listed(case: &ItemCase<'_>) -> ItemDecision {
    if case.is_listing_under_way {
        return ItemDecision::Wait(WaitReason::Listing);
    }
    row_step(RowStep::GiveUpBinding { item: case.versions() })
}

/// What keeps `sending` of the item from being sent, as the plan answers it; `None` when it
/// is sent.
fn find_decision_not_to_send(case: &ItemCase<'_>, sending: Sending) -> Option<ItemDecision> {
    Some(match find_sending_obstacle(sending, &case.sending)? {
        SendingObstacle::Ignored => ItemDecision::Exclusion(ExclusionReason::NameIgnored),
        SendingObstacle::ReadOnlyFolder => ItemDecision::Wait(WaitReason::User(Reason::NotAllowed(Some(READ_ONLY_FOLDER.to_owned())))),
        SendingObstacle::NameRefused(reason) => ItemDecision::Wait(WaitReason::User(reason)),
        SendingObstacle::TooLarge => ItemDecision::Wait(WaitReason::User(Reason::TooLarge)),
        SendingObstacle::OneDriveFull => ItemDecision::Wait(WaitReason::User(Reason::WaitingForSpace)),
        SendingObstacle::QuietTime { until } => ItemDecision::Wait(WaitReason::Time { until, reason: Reason::Changed }),
    })
}

/// What stands behind "not allowed now" for an item that waits for its folder to be
/// read-write again.
pub(crate) const READ_ONLY_FOLDER: &str = "the folder is read-only";

/// A step towards OneDrive that sends the item into the directory the folder shows it in,
/// under its name here: made by `step` from that directory's binding and the name. Waits
/// for the directory while OneDrive does not have it, and for a listing while the root of
/// the folder is not bound.
fn send_into_local_directory(case: &ItemCase<'_>, step: impl FnOnce(OneDriveItemReference, std::ffi::OsString) -> OneDriveStep) -> ItemDecision {
    let Some(place) = case.item.local.as_ref().and_then(|local| local.place.as_ref()) else { return ItemDecision::Fault };
    match &case.local_directory {
        LocalDirectoryBinding::Bound(directory) => onedrive_step(step(directory.clone(), place.name.clone())),
        LocalDirectoryBinding::NotBound(LocalItemId::ROOT) => ItemDecision::Wait(WaitReason::Listing),
        LocalDirectoryBinding::NotBound(directory) => ItemDecision::Wait(WaitReason::AnotherItem(*directory)),
        LocalDirectoryBinding::NoLocalPlace => ItemDecision::Fault,
    }
}

/// Is there here, never was in OneDrive: new here, it is sent, unless a rule says it is not.
fn decide_new_here(case: &ItemCase<'_>) -> ItemDecision {
    let item = case.versions();
    match case.item.kind {
        ItemKind::Directory => find_decision_not_to_send(case, Sending::NewDirectory)
            .unwrap_or_else(|| send_into_local_directory(case, |parent, name| OneDriveStep::MakeDirectory { item, parent, name })),
        ItemKind::File => find_decision_not_to_send(case, Sending::NewFile)
            .unwrap_or_else(|| send_into_local_directory(case, |parent, name| OneDriveStep::SendNewFile { item, parent, name })),
    }
}

/// Removed here, is there in OneDrive. A file whose content OneDrive changed comes back;
/// the removal of any other file is sent, guarded by the eTag, also when OneDrive moved it.
/// A directory's removal is sent unguarded, and stands.
fn decide_removed_here(case: &ItemCase<'_>, onedrive_content: ContentChange) -> ItemDecision {
    match (case.item.kind, onedrive_content) {
        (ItemKind::File, ContentChange::Changed | ContentChange::NotKnownWithoutHash) => show_onedrive_item(case),
        (ItemKind::File, ContentChange::Same) => {
            find_decision_not_to_send(case, Sending::Removal).unwrap_or_else(|| onedrive_step(OneDriveStep::SendFileRemoval { item: case.versions() }))
        }
        (ItemKind::Directory, _) => {
            find_decision_not_to_send(case, Sending::Removal).unwrap_or_else(|| onedrive_step(OneDriveStep::SendDirectoryRemoval { item: case.versions(), below: Vec::new() }))
        }
    }
}

/// An item of OneDrive the folder does not show appears here, where the folder can show
/// it: new in OneDrive, or coming back. An item the folder cannot show does not, and that
/// is said only once no listing is under way.
fn show_onedrive_item(case: &ItemCase<'_>) -> ItemDecision {
    match case.folder_visibility {
        FolderVisibility::NotKnownYet => ItemDecision::Wait(WaitReason::Listing),
        FolderVisibility::CannotBeShown if case.is_listing_under_way => ItemDecision::Wait(WaitReason::Listing),
        FolderVisibility::CannotBeShown => ItemDecision::Exclusion(ExclusionReason::CannotBeShown),
        FolderVisibility::NotInOneDrive => ItemDecision::Fault,
        FolderVisibility::CanBeShown => match case.onedrive_place_here {
            OneDrivePlaceHere::Free => folder_step(FolderStep::ShowItem { item: case.versions() }),
            OneDrivePlaceHere::DirectoryNotShown(directory) => ItemDecision::Wait(WaitReason::AnotherItem(directory)),
            OneDrivePlaceHere::NameTaken(holder) => ItemDecision::Wait(WaitReason::AnotherItem(holder)),
            OneDrivePlaceHere::NameUnusable => ItemDecision::Exclusion(ExclusionReason::CannotBeShown),
            OneDrivePlaceHere::NoOneDrivePlace => ItemDecision::Fault,
        },
    }
}

/// Is there on both sides: its place, then for a file its content, and last the download
/// of a pinned file whose content is not on disk.
fn decide_item_on_both_sides(case: &ItemCase<'_>, here: (PlaceChange, ContentChange), in_onedrive: (PlaceChange, ContentChange)) -> ItemDecision {
    if matches!(case.folder_visibility, FolderVisibility::NotKnownYet) {
        return ItemDecision::Wait(WaitReason::Listing);
    }
    if matches!(case.folder_visibility, FolderVisibility::NotInOneDrive) {
        return ItemDecision::Fault;
    }
    let place = decide_place(case, here.0, in_onedrive.0, (here.1, in_onedrive.1));
    if place != ItemDecision::NothingToDo || case.item.kind == ItemKind::Directory {
        return place;
    }
    let content = decide_content(case, here.1, in_onedrive.1);
    if content == ItemDecision::NothingToDo && case.is_pinned && case.item.content_presence == ContentPresence::NotOnDisk {
        return folder_step(FolderStep::DownloadContent { item: case.versions() });
    }
    content
}

/// The step that sends the item's place here to OneDrive.
fn send_move(case: &ItemCase<'_>) -> ItemDecision {
    let item = case.versions();
    find_decision_not_to_send(case, Sending::Place).unwrap_or_else(|| send_into_local_directory(case, |parent, name| OneDriveStep::SendMove { item, parent, name }))
}

/// OneDrive has the item where the folder cannot show it, and the item was not moved here:
/// it goes here as if removed, unless its content was changed here, which is then sent
/// into it where it is, the item staying here. Nothing is done while a listing is under way.
fn decide_item_that_cannot_be_shown(case: &ItemCase<'_>, here_content: ContentChange, onedrive_content: ContentChange) -> ItemDecision {
    if case.is_listing_under_way {
        return ItemDecision::Wait(WaitReason::Listing);
    }
    match (case.item.kind, here_content) {
        (ItemKind::File, ContentChange::Changed | ContentChange::NotKnownWithoutHash) => decide_content(case, here_content, onedrive_content),
        _ => remove_item_here(case),
    }
}

/// The table "its place", for an item both sides have.
///
/// - Same here, moved in OneDrive: it is moved here; moved to where the folder cannot show
///   it, [`decide_item_that_cannot_be_shown`].
/// - Moved here, same in OneDrive: the move is sent.
/// - Moved on both sides to one place: agree. To different places: OneDrive's stands and
///   the folder follows, where it can; where it cannot (the name is taken here, the name
///   cannot be held, the folder cannot show the item there), the local place stands and is
///   sent.
///
/// What was done to the content on each side is asked only where the item cannot be shown.
pub(crate) fn decide_place(case: &ItemCase<'_>, here: PlaceChange, in_onedrive: PlaceChange, (here_content, onedrive_content): (ContentChange, ContentChange)) -> ItemDecision {
    let can_be_shown = matches!(case.folder_visibility, FolderVisibility::CanBeShown);
    let item = case.item;
    match (here, in_onedrive) {
        (PlaceChange::Same, _) if !can_be_shown => decide_item_that_cannot_be_shown(case, here_content, onedrive_content),
        (PlaceChange::Same, PlaceChange::Same) => ItemDecision::NothingToDo,
        (PlaceChange::Same, PlaceChange::Moved) => match case.onedrive_place_here {
            OneDrivePlaceHere::Free => folder_step(FolderStep::MoveItem { item: case.versions() }),
            OneDrivePlaceHere::DirectoryNotShown(directory) => ItemDecision::Wait(WaitReason::AnotherItem(directory)),
            OneDrivePlaceHere::NameTaken(holder) => ItemDecision::Wait(WaitReason::AnotherItem(holder)),
            OneDrivePlaceHere::NameUnusable => decide_item_that_cannot_be_shown(case, here_content, onedrive_content),
            OneDrivePlaceHere::NoOneDrivePlace => ItemDecision::Fault,
        },
        (PlaceChange::Moved, PlaceChange::Same) => send_move(case),
        (PlaceChange::Moved, PlaceChange::Moved) => {
            let is_one_place = item.local.as_ref().map(|local| &local.place) == item.onedrive.as_ref().map(|onedrive| &onedrive.place);
            match case.onedrive_place_here {
                _ if is_one_place => row_step(RowStep::AgreeOnPlace { item: case.versions() }),
                _ if !can_be_shown && case.is_listing_under_way => ItemDecision::Wait(WaitReason::Listing),
                _ if !can_be_shown => send_move(case),
                OneDrivePlaceHere::Free => folder_step(FolderStep::MoveItem { item: case.versions() }),
                OneDrivePlaceHere::DirectoryNotShown(directory) => ItemDecision::Wait(WaitReason::AnotherItem(directory)),
                OneDrivePlaceHere::NameTaken(_) | OneDrivePlaceHere::NameUnusable => send_move(case),
                OneDrivePlaceHere::NoOneDrivePlace => ItemDecision::Fault,
            }
        }
    }
}

/// The table "its content", for a file both sides have.
///
/// - Same on both sides: nothing, unless the content versions are not one yet though the
///   bytes are (a hash computed here, a cTag or a time OneDrive changed): agree.
/// - Same here, changed in OneDrive: the file takes OneDrive's content, and the copy on
///   disk goes.
/// - Changed here, same in OneDrive: the content is sent.
/// - Changed on both sides: the same hash, agree; another, both are kept, the local one
///   giving up the binding; not known without a hash, the local content is hashed.
pub(crate) fn decide_content(case: &ItemCase<'_>, here: ContentChange, in_onedrive: ContentChange) -> ItemDecision {
    let item = case.item;
    let local = item.local.as_ref().and_then(|local| local.content.as_ref());
    let onedrive = item.onedrive.as_ref().and_then(|onedrive| onedrive.content.as_ref());
    match (here, in_onedrive) {
        (ContentChange::Same, ContentChange::Same) if local == onedrive && local == item.synced_content.as_ref() => ItemDecision::NothingToDo,
        (ContentChange::Same, ContentChange::Same) => row_step(RowStep::AgreeOnContent { item: case.versions() }),
        (ContentChange::Same, ContentChange::Changed | ContentChange::NotKnownWithoutHash) => folder_step(FolderStep::TakeOneDriveContent { item: case.versions() }),
        (ContentChange::Changed | ContentChange::NotKnownWithoutHash, ContentChange::Same) => {
            find_decision_not_to_send(case, Sending::Content).unwrap_or_else(|| onedrive_step(OneDriveStep::SendContent { item: case.versions() }))
        }
        (ContentChange::Changed | ContentChange::NotKnownWithoutHash, ContentChange::Changed | ContentChange::NotKnownWithoutHash) => match compare_local_content(local, onedrive) {
            ContentChange::Same => row_step(RowStep::AgreeOnContent { item: case.versions() }),
            ContentChange::Changed => row_step(RowStep::GiveUpBinding { item: case.versions() }),
            ContentChange::NotKnownWithoutHash => row_step(RowStep::HashLocalContent { item: case.versions() }),
        },
    }
}
