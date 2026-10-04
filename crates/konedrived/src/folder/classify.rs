//! What a Graph item becomes in the tree, and whether it has a place in the
//! folder.

use konedrive_fs::{NAME_MAX, RESERVED_PREFIX};
use konedrive_graph::drive::item::DriveItem;
use konedrive_tree::{usable_id, Change, Kind, Placement, Row, SkipReason};

/// What a Graph item becomes in the tree.
pub fn classify(item: &DriveItem) -> Change {
    if item.deleted.is_some() {
        return Change::Delete(item.id.clone());
    }
    let mut row = Row {
        id: item.id.clone(),
        parent_id: item.parent_reference.as_ref().and_then(|p| p.id.clone()),
        name: item.name.clone().unwrap_or_default(),
        kind: if item.folder.is_some() || item.package.is_some() { Kind::Folder } else { Kind::File },
        size: 0,
        mtime: item.mtime(),
        etag: item.e_tag.clone(),
        ctag: item.c_tag.clone(),
        quickxor: item.quick_xor_hash().map(str::to_owned),
        mime: item.file.as_ref().and_then(|f| f.mime_type.clone()),
        placement: Placement::Placed,
    };
    if item.root.is_some() {
        row.parent_id = None;
        row.name = String::new();
        row.kind = Kind::Folder;
        return Change::Root(row);
    }
    if row.kind == Kind::File {
        row.size = item.size.unwrap_or(0);
    }
    if let Some(reason) = skip_reason(item, &row.name) {
        row.placement = Placement::Skipped(reason);
    }
    Change::Upsert(row)
}

fn skip_reason(item: &DriveItem, name: &str) -> Option<SkipReason> {
    if !usable_id(&item.id) {
        return Some(SkipReason::Unsupported);
    }
    if item.remote_item.is_some() {
        return Some(SkipReason::Shared);
    }
    if item.package.is_some() {
        return Some(SkipReason::OneNote);
    }
    if item.special_folder.as_ref().and_then(|s| s.name.as_deref()) == Some("vault") {
        return Some(SkipReason::PersonalVault);
    }
    if item.file.is_none() && item.folder.is_none() {
        return Some(SkipReason::Unsupported);
    }
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Some(SkipReason::Unsupported);
    }
    if name.len() > NAME_MAX {
        return Some(SkipReason::NameTooLong);
    }
    if name.starts_with(RESERVED_PREFIX) {
        return Some(SkipReason::ReservedName);
    }
    None
}

#[cfg(test)]
mod tests;
