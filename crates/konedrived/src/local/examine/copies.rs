//! What becomes of an entry that carries an item's id and is not the item: a copy that
//! kept its attributes, a file from another folder or account, the wrong kind.

use konedrive_tree::outbox::LocalSkip;
use konedrive_tree::{ActivityKind, ActivityRow};

use super::listing::EntryIx;
use super::{lossy, ExamineError, Run};
use crate::local::entry::{Entry, StateAttr, Type};

/// What is done with a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fate {
    /// The user's own file or directory: its marks come off, and it is
    /// uploaded as new, a directory with its contents.
    Stripped,
    /// A downloaded file with other names, whose other names stripping would
    /// change too: listed.
    HardLink,
    /// A file that is not downloaded cannot be read here, and one whose
    /// state cannot be read is not known to be downloaded: listed — or, when
    /// it is marked as not downloaded, surely nobody's file and holds no
    /// data, removed.
    NotDownloaded,
}

pub(super) fn fate(e: &Entry) -> Fate {
    match e.ty {
        Type::File if e.hydrated() && e.nlink > 1 => Fate::HardLink,
        Type::Dir => Fate::Stripped,
        Type::File if e.hydrated() => Fate::Stripped,
        _ => Fate::NotDownloaded,
    }
}

impl Run<'_, '_, '_> {
    /// Entry `ix` is a copy ([`fate`]). `certain`: the item's recorded
    /// object was seen in this run, so the entry is certainly not the item's
    /// file. Only then is a file not downloaded that holds no data removed
    /// ([`Hands::remove_empty`](super::hands::Hands::remove_empty)). "Not
    /// seen" is not "gone": the entry may be the item's own file, moved
    /// where this run did not look; and an id this store does not know may
    /// be another account's, whose move-out waits to download the file where
    /// it went.
    pub(super) fn copy(&mut self, ix: EntryIx, certain: bool) -> Result<(), ExamineError> {
        let listing = self.listing;
        let e = &listing[ix];
        match fate(e) {
            Fate::HardLink => self.list(ix, LocalSkip::HardLink),
            Fate::NotDownloaded => {
                if certain && self.hands.remove_empty(e) {
                    self.decisions.is_link(ix);
                    tracing::info!("{} was marked as not downloaded, is not its item's file and held no data; it is removed", e.rel.display());
                    let path = self.root_path.as_deref().map_or_else(|| e.rel.clone(), |root| root.join(&e.rel));
                    self.outcome.activity.push(ActivityRow {
                        at: self.ex.now,
                        kind: ActivityKind::Removed,
                        path: path.display().to_string(),
                        detail: format!("removed an empty copy of {}: it held no content", lossy(&e.name)),
                    });
                } else {
                    // Marks that say nothing readable are said as that: the
                    // file is not known to be one that is not downloaded.
                    let damaged = e.ty == Type::File && matches!(e.state, StateAttr::Absent | StateAttr::Corrupt);
                    self.list(ix, if damaged { LocalSkip::BadState } else { LocalSkip::NotDownloaded });
                }
            }
            Fate::Stripped => {
                if self.strip(e)? {
                    if e.ty == Type::Dir && !listing.whole(&e.rel) {
                        self.outcome.out.recheck.tree(&e.rel);
                    }
                    self.outcome.out.stripped.push(e.rel.clone());
                    self.decisions.is_new(ix);
                } else {
                    // Not stripped, because it was refused, because it went,
                    // or because its name holds another object by now: it is
                    // not uploaded as new, and what was listed inside it gets
                    // no row in this run (`new_object`). In any other run, and
                    // at the worker, the id it may still carry is no folder
                    // to go into (`upload::steps::shared::dir_id`).
                    self.decisions.is_link(ix);
                    self.decisions.give_up(&e.rel);
                }
            }
        }
        Ok(())
    }

    /// A copy that stays as it is, said in the skipped list unless its name
    /// is ignored.
    fn list(&mut self, ix: EntryIx, reason: LocalSkip) {
        let e = &self.listing[ix];
        if !self.ex.ignore.matches(&e.name) {
            self.skip(&e.rel, reason);
        }
        self.decisions.is_link(ix);
    }
}
