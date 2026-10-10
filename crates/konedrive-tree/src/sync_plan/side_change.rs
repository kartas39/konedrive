//! What one side did to one item since the two sides last agreed, read off its row: the
//! local version against the synced one, and the OneDrive version against the synced one.
//! Whether the item exists there, and when it does, its place and its content.

use crate::model::{ContentVersion, Item, ItemVersion};

/// What a side did to an item, against what the two sides last agreed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SideChange {
    /// The side does not have the item, and nothing was agreed on: it never had it.
    NeverWas,
    /// The side does not have the item, and something was agreed on: it removed it.
    Removed,
    /// The side has the item. When nothing was agreed on it is new there: its place then
    /// reads as moved, and a file's content as changed.
    IsThere { place: PlaceChange, content: ContentChange },
}

/// The place a side has an item at, against the synced place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaceChange {
    Same,
    /// Another directory or another name, compared as the names were given.
    Moved,
}

/// The content a side has of a file, against another content. A directory has none, and
/// reads as the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentChange {
    Same,
    Changed,
    /// A program changed the content here and no hash of it is known, while the other
    /// content has one: only hashing the local content tells whether the bytes are the
    /// same.
    NotKnownWithoutHash,
}

impl SideChange {
    /// What was done here: the local version of `item` against what was agreed on.
    pub(crate) fn of_local(item: &Item) -> Self {
        Self::of(item, item.local.as_ref(), compare_local_content)
    }

    /// What was done in OneDrive: the OneDrive version of `item` against what was agreed on.
    pub(crate) fn of_onedrive(item: &Item) -> Self {
        Self::of(item, item.onedrive.as_ref(), compare_onedrive_content)
    }

    fn of(item: &Item, version: Option<&ItemVersion>, compare_content: fn(Option<&ContentVersion>, Option<&ContentVersion>) -> ContentChange) -> Self {
        let was_agreed_on = item.synced_place.is_some() || item.synced_content.is_some();
        match version {
            None if was_agreed_on => SideChange::Removed,
            None => SideChange::NeverWas,
            Some(version) => SideChange::IsThere {
                place: if version.place == item.synced_place { PlaceChange::Same } else { PlaceChange::Moved },
                content: compare_content(version.content.as_ref(), item.synced_content.as_ref()),
            },
        }
    }
}

/// The hash of a content, when one is known.
fn hash_of(content: &ContentVersion) -> Option<&str> {
    match content {
        ContentVersion::Hashed { hash, .. } => hash.as_deref(),
        ContentVersion::ChangedHere { .. } => None,
    }
}

/// The cTag of a content, when OneDrive gave one.
fn ctag_of(content: &ContentVersion) -> Option<&str> {
    match content {
        ContentVersion::Hashed { ctag, .. } => ctag.as_deref(),
        ContentVersion::ChangedHere { .. } => None,
    }
}

/// The local content of a file against another content, the synced one or OneDrive's. The
/// first that holds:
///
/// - the same content version, whole: the same;
/// - another size: changed, and no hash is needed to know it;
/// - no hash here and the cTag of the other, with its time in whole seconds: the same. Such
///   a local content was taken from the mark of a copy on disk, and the mark survives an
///   edit in place, so the cTag alone does not say it;
/// - a hash on both: the same when the hashes are, whatever the times and the cTags say;
/// - no hash here and one there: not known without hashing the local content;
/// - no hash there: changed. No hash of what OneDrive has can be computed here, so a local
///   content that is not that content version is kept as another content.
pub(crate) fn compare_local_content(local: Option<&ContentVersion>, other: Option<&ContentVersion>) -> ContentChange {
    match (local, other) {
        (None, None) => ContentChange::Same,
        (None, Some(_)) | (Some(_), None) => ContentChange::Changed,
        (Some(local), Some(other)) if local == other => ContentChange::Same,
        (Some(local), Some(other)) if local.size() != other.size() => ContentChange::Changed,
        (Some(local), Some(other)) if hash_of(local).is_none() && ctag_of(local).is_some() && ctag_of(local) == ctag_of(other) && local.mtime().0 == other.mtime().0 => ContentChange::Same,
        (Some(local), Some(other)) => match (hash_of(local), hash_of(other)) {
            (Some(local), Some(other)) if local == other => ContentChange::Same,
            (Some(_), Some(_)) => ContentChange::Changed,
            (None, Some(_)) => ContentChange::NotKnownWithoutHash,
            (_, None) => ContentChange::Changed,
        },
    }
}

/// The content OneDrive has of a file against the synced one: the same, or changed. The
/// same content version, a hash on both that is the same, or, where a hash is missing, a
/// cTag on both that is the same, with the same size: the same. Never "not known": what
/// OneDrive has cannot be hashed here.
pub(crate) fn compare_onedrive_content(onedrive: Option<&ContentVersion>, synced: Option<&ContentVersion>) -> ContentChange {
    match (onedrive, synced) {
        (None, None) => ContentChange::Same,
        (None, Some(_)) | (Some(_), None) => ContentChange::Changed,
        (Some(onedrive), Some(synced)) if onedrive == synced => ContentChange::Same,
        (Some(onedrive), Some(synced)) if onedrive.size() != synced.size() => ContentChange::Changed,
        (Some(onedrive), Some(synced)) => {
            let same = match (hash_of(onedrive), hash_of(synced)) {
                (Some(onedrive), Some(synced)) => onedrive == synced,
                _ => ctag_of(onedrive).is_some() && ctag_of(onedrive) == ctag_of(synced),
            };
            if same {
                ContentChange::Same
            } else {
                ContentChange::Changed
            }
        }
    }
}

#[cfg(test)]
mod tests;
