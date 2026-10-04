//! One row, one step (§3.6, §4, §5, §6): `mkdir`, `move` and `delete` in
//! [`meta`], content in [`super::content`], what they share in [`shared`] —
//! where the row's object is, which folder it goes into, a name that is
//! taken, the commit, the conflict copy — and the blocking sections their
//! file calls run in ([`sections`]).

use std::sync::Arc;

use super::engine::{outcome_of, Engine, NoSpace, Outcome};
use super::local;
use crate::folder::disk::Disk;
use konedrive_tree::outbox::{OutboxKind, OutboxRow};

mod meta;
mod sections;
mod shared;

pub(super) use meta::delete;
pub(crate) use sections::Sections;
pub(super) use sections::{blocking, blocking_under, off, share, tree};
pub(super) use shared::{answer_row, cancel_session, commit_row, copy, follow_cloud, local_name, locate, name_taken, never_uploaded, parent_of, upload_as_new, wanted_name, Guard, Named, Ours};

pub(super) async fn run(e: &Arc<Engine>, disk: &Arc<Disk>, row: OutboxRow) -> Outcome {
    let rel = row.rel.clone();
    let result = match row.kind {
        OutboxKind::Create | OutboxKind::Update => super::content::run(e, disk, row).await,
        OutboxKind::Mkdir => meta::mkdir(e, disk, row).await,
        OutboxKind::Move => meta::moved(e, disk, row).await,
        OutboxKind::Delete => delete(e, row).await,
        // The object is downloaded before its item goes (WR5).
        OutboxKind::MoveOut => super::move_out::run(e, disk, row).await,
    };
    match result.or_else(outcome_of) {
        Ok(outcome) => outcome,
        // Refused for lack of space: the quota decides whether the account
        // is full or only this file too big (`space`).
        Err(NoSpace) => {
            let disk = Arc::clone(disk);
            let size = blocking(move || Ok(local::size_at(&disk, &rel))).await.ok().flatten();
            e.space_refused(size.unwrap_or(0)).await
        }
    }
}

#[cfg(test)]
mod tests;
