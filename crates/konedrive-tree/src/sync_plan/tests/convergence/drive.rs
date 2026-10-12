//! OneDrive in these tests: the items it has, by their ids, and what changes them — another
//! client, and the requests of this client.

use std::collections::BTreeMap;

use crate::item_store::tests::listed_drive::reference;
use crate::model::{Change, ContentVersion, Item, ItemKind, Kind, LocalItemId, Placement, Row};
use crate::{ConfirmedItem, FoldedName, ItemVersions, OneDriveStep};

/// The id of the drive's root in these tests.
pub(crate) const ROOT_ID: &str = "R";

/// An item as OneDrive has it: where, and a file's content as `(size, mtime)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriveItem {
    pub(crate) parent: String,
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) content: Option<(u64, i64)>,
    /// Whether the folder can show it, by its own placement.
    pub(crate) is_placed: bool,
    /// The eTag OneDrive has it under: every change of it gives another.
    pub(crate) etag: String,
    /// The content this client sent into it, while no other client wrote into it since.
    pub(crate) sent_content: Option<(u64, i64)>,
}

impl DriveItem {
    /// An item another client makes: one the folder can show.
    pub(crate) fn new(parent: &str, name: &str, kind: Kind, content: Option<(u64, i64)>) -> Self {
        Self { parent: parent.to_owned(), name: name.to_owned(), kind, content, is_placed: true, etag: String::new(), sent_content: None }
    }
}

/// What OneDrive answered a request of this client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestOutcome {
    /// It did what was asked: the item as it has it now, or nothing for a removal.
    Confirmed(Option<ConfirmedItem>),
    /// The name is taken (409).
    NameTaken,
    /// The item or the directory is gone (404), the item changed (412), or the move
    /// would put a directory into itself (400): nothing was done.
    Refused,
}

/// What OneDrive has, and the changes made there: by another client, each keeping
/// OneDrive's own rules (one item per name in a directory, as OneDrive compares names; no
/// directory in itself), and by the requests of this client ([`request`](Self::request)).
/// Each is said by the listing that follows. Read off the rows before every listing, it is
/// what the rows say OneDrive has; kept from one listing to the next, it is a state of its
/// own, which the rows follow only by a commit or a listing.
pub(crate) struct OneDriveChanges {
    pub(crate) items: BTreeMap<String, DriveItem>,
    pub(crate) listed: Vec<Change>,
    etag_count: u64,
}

impl OneDriveChanges {
    /// What the rows say OneDrive has. `etag_count`: a number no eTag was made of yet.
    pub(crate) fn of(rows: &[Item], etag_count: u64) -> Self {
        let item_id = |row: &Item| row.onedrive_item.as_ref().map(|reference| reference.item_id.as_str().to_owned());
        let id_of: BTreeMap<LocalItemId, String> = rows.iter().filter_map(|row| Some((row.local_item_id, item_id(row)?))).collect();
        let mut items = BTreeMap::new();
        for row in rows {
            let (Some(id), Some(onedrive)) = (item_id(row), &row.onedrive) else { continue };
            let Some(place) = &onedrive.place else { continue };
            let content = onedrive.content.as_ref().map(|content| (content.size(), content.mtime().0));
            let kind = if row.kind == ItemKind::File { Kind::File } else { Kind::Folder };
            let etag = row.onedrive_etag.clone().unwrap_or_default();
            items.insert(id, DriveItem { parent: id_of[&place.parent_local_item_id].clone(), name: place.name.to_string_lossy().into_owned(), kind, content, is_placed: row.placement == Placement::Placed, etag, sent_content: None });
        }
        Self { items, listed: Vec::new(), etag_count }
    }

    pub(crate) fn is_in(&self, id: &str, directory: &str) -> bool {
        let mut at = id.to_owned();
        for _ in 0..=self.items.len() {
            if at == directory {
                return true;
            }
            match self.items.get(&at) {
                Some(item) => at = item.parent.clone(),
                None => return false,
            }
        }
        true
    }

    fn is_name_free(&self, id: &str, parent: &str, name: &str) -> bool {
        let folded = FoldedName::of(name.as_ref());
        !self.items.iter().any(|(other, item)| other != id && item.parent == parent && FoldedName::of(item.name.as_ref()) == folded)
    }

    /// `id` is `item` from now on, where OneDrive's rules let it be; whether it is.
    pub(crate) fn put(&mut self, id: &str, item: DriveItem) -> bool {
        let is_in_directory = item.parent == ROOT_ID || self.items.get(&item.parent).is_some_and(|parent| parent.kind == Kind::Folder);
        if !is_in_directory || !self.is_name_free(id, &item.parent, &item.name) || self.is_in(&item.parent, id) {
            return false;
        }
        // Whatever another client did to it, what it holds is no longer told as what this
        // client sent.
        self.items.insert(id.to_owned(), DriveItem { sent_content: None, ..item });
        self.change(id);
        true
    }

    /// The content of the item a request was answered with is not told as the one this
    /// client sent any more: the client may come to know it by a listing, before the
    /// answer or without it, and then reads it as any content OneDrive has.
    pub(crate) fn forget_what_was_sent(&mut self, answer: &RequestOutcome) {
        if let RequestOutcome::Confirmed(Some(confirmed)) = answer {
            if let Some(item) = self.items.get_mut(confirmed.item.item_id.as_str()) {
                item.sent_content = None;
            }
        }
    }

    /// The two items have each the other's place.
    pub(crate) fn exchange(&mut self, first: &str, second: &str) -> bool {
        let (one, other) = (self.items[first].clone(), self.items[second].clone());
        if self.is_in(&one.parent, second) || self.is_in(&other.parent, first) {
            return false;
        }
        self.items.insert(first.to_owned(), DriveItem { parent: other.parent.clone(), name: other.name.clone(), sent_content: None, ..one.clone() });
        self.items.insert(second.to_owned(), DriveItem { parent: one.parent, name: one.name, sent_content: None, ..other });
        self.change(first);
        self.change(second);
        true
    }

    /// `id` goes, with all OneDrive has in it; the listing names the item alone.
    pub(crate) fn remove(&mut self, id: &str) {
        let below: Vec<String> = self.items.keys().filter(|other| self.is_in(other, id)).cloned().collect();
        for gone in below {
            self.items.remove(&gone);
        }
        self.listed.push(Change::Delete(id.to_owned()));
    }

    /// The other client makes `change`, which it was to make before, in OneDrive as it is
    /// now, where OneDrive's rules still let it: what a later page of a listing says after
    /// the steps of this client ran in between.
    pub(crate) fn make_again(&mut self, change: &Change) {
        match change {
            Change::Upsert(row) => {
                let content = (row.kind == Kind::File).then_some((row.size, row.mtime));
                let item = DriveItem { is_placed: row.placement == Placement::Placed, ..DriveItem::new(&row.parent_id.clone().unwrap_or_default(), &row.name, row.kind, content) };
                // Where OneDrive's rules do not let it be so any more, the other client
                // gives it a name nothing has, in the root: it does not stay where an
                // earlier page left it, which may be under another item's name.
                self.etag_count += 1;
                let elsewhere = DriveItem { parent: ROOT_ID.to_owned(), name: format!("elsewhere-{}", self.etag_count), ..item.clone() };
                let _ = self.put(&row.id, item) || self.put(&row.id, elsewhere);
            }
            Change::Delete(id) if self.items.contains_key(id) => self.remove(id),
            Change::Delete(_) | Change::Root(_) => {}
        }
    }

    /// `id` changed: it has a new eTag, and the listing that follows says it as it is now.
    fn change(&mut self, id: &str) {
        self.etag_count += 1;
        let etag = format!("l-{}", self.etag_count);
        self.items.get_mut(id).expect("the item that changed").etag = etag;
        let listed = self.read(id);
        self.listed.push(listed);
    }

    /// `id` as OneDrive has it now, as a listing or a reading of the one item says it: gone,
    /// when it has no such item.
    pub(crate) fn read(&self, id: &str) -> Change {
        let Some(item) = self.items.get(id) else { return Change::Delete(id.to_owned()) };
        let (size, mtime) = item.content.unwrap_or((0, 500));
        let hash = item.content.map(|(size, mtime)| format!("h{size}-{mtime}"));
        Change::Upsert(Row {
            id: id.to_owned(),
            parent_id: Some(item.parent.clone()),
            name: item.name.clone(),
            kind: item.kind,
            size,
            mtime,
            etag: Some(item.etag.clone()),
            ctag: hash.as_ref().map(|hash| format!("c-{hash}")),
            quickxor: hash,
            mime: None,
            placement: if item.is_placed { Placement::Placed } else { Placement::Skipped(crate::model::SkipReason::Unsupported) },
        })
    }

    /// What was changed since the listing before, as the page of a listing fetched now
    /// says it: each item that changed once, as it is now — one that changed and then went
    /// with a directory above it is named gone, as a delta of OneDrive names it; one that
    /// went with a directory and did not change before is not named.
    pub(crate) fn fetch_changes(&mut self) -> Vec<Change> {
        let mut changed: Vec<String> = Vec::new();
        for change in std::mem::take(&mut self.listed).iter().rev() {
            if !changed.iter().any(|id| id == change.id()) {
                changed.push(change.id().to_owned());
            }
        }
        changed.iter().rev().map(|id| self.read(id)).collect()
    }

    /// The item `id` as OneDrive answers a request about it.
    fn confirm(&self, id: &str) -> Option<ConfirmedItem> {
        let item = self.items.get(id)?;
        let content = item.content.map(|(size, mtime)| ContentVersion::Hashed { size, mtime, mtime_nsec: 0, hash: Some(format!("h{size}-{mtime}")), ctag: Some(format!("c-h{size}-{mtime}")) });
        Some(ConfirmedItem { item: reference(id), parent: reference(&item.parent), name: item.name.clone().into(), content, etag: item.etag.clone() })
    }

    /// One request of this client, as OneDrive takes it: guarded by the eTag the step
    /// carries where the step says so, refused where OneDrive's rules refuse it, and done
    /// otherwise, whatever becomes of its answer. The removal of a directory is unguarded
    /// and takes all that is in it; an item that is gone already is a removal confirmed.
    pub(crate) fn request(&mut self, step: &OneDriveStep) -> RequestOutcome {
        let versions = step.item();
        let content_of = |versions: &ItemVersions| versions.local.as_ref().and_then(|local| local.content.as_ref()).map(|content| (content.size(), content.mtime().0));
        let is_directory = |drive: &Self, id: &str| id == ROOT_ID || drive.items.get(id).is_some_and(|item| item.kind == Kind::Folder);
        if let OneDriveStep::MakeDirectory { parent, name, .. } | OneDriveStep::SendNewFile { parent, name, .. } = step {
            let (parent, name) = (parent.item_id.as_str(), name.to_string_lossy().into_owned());
            if !is_directory(self, parent) {
                return RequestOutcome::Refused;
            }
            if !self.is_name_free("", parent, &name) {
                return RequestOutcome::NameTaken;
            }
            self.etag_count += 1;
            let (id, content) = (format!("N{}", self.etag_count), content_of(versions));
            let kind = if content.is_some() { Kind::File } else { Kind::Folder };
            self.items.insert(id.clone(), DriveItem { sent_content: content, ..DriveItem::new(parent, &name, kind, content) });
            self.change(&id);
            return RequestOutcome::Confirmed(self.confirm(&id));
        }
        let Some(id) = versions.onedrive_item.as_ref().map(|item| item.item_id.as_str().to_owned()) else { return RequestOutcome::Refused };
        let Some(item) = self.items.get(&id).cloned() else {
            let is_removal = matches!(step, OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. });
            return if is_removal { RequestOutcome::Confirmed(None) } else { RequestOutcome::Refused };
        };
        let is_changed = versions.onedrive_etag.as_deref() != Some(item.etag.as_str());
        match step {
            OneDriveStep::SendDirectoryRemoval { .. } => self.remove(&id),
            _ if is_changed => return RequestOutcome::Refused,
            OneDriveStep::SendFileRemoval { .. } => self.remove(&id),
            OneDriveStep::SendContent { .. } => {
                let content = content_of(versions);
                self.items.insert(id.clone(), DriveItem { content, sent_content: content, ..item });
                self.change(&id);
            }
            OneDriveStep::SendMove { parent, name, .. } => {
                let (parent, name) = (parent.item_id.as_str(), name.to_string_lossy().into_owned());
                if !is_directory(self, parent) || self.is_in(parent, &id) {
                    return RequestOutcome::Refused;
                }
                if !self.is_name_free(&id, parent, &name) {
                    return RequestOutcome::NameTaken;
                }
                self.items.insert(id.clone(), DriveItem { parent: parent.to_owned(), name, ..item });
                self.change(&id);
            }
            OneDriveStep::MakeDirectory { .. } | OneDriveStep::SendNewFile { .. } => unreachable!("answered above"),
        }
        RequestOutcome::Confirmed(self.confirm(&id))
    }
}
