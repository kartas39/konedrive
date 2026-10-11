//! The items OneDrive refused, with the reason of each, as the window and `konedrivectl`
//! list and count them: those refused for good, by the reason OneDrive answered, and those
//! refused for lack of space. Read from the rows of `step_memory` by an index each, and only
//! where the item's row still says what was refused.

use konedrive_reason::Reason;
use rusqlite::Connection;

use super::list_said_facts;
use crate::model::LocalItemId;
use crate::TreeError;

/// An item OneDrive refused, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRefusalReason {
    pub item: LocalItemId,
    /// The reason of a refusal for good as OneDrive answered it, or `too-big` with what the
    /// file needs and the free space.
    pub reason: Reason,
}

const REFUSED: &str = "refusal_reason IS NOT NULL";
const REFUSED_FOR_SPACE: &str = "space_refusal_size IS NOT NULL";

/// Whether `found` is to be listed for `asked`: every reason when none is asked for, and
/// otherwise the reasons of that kind, whatever their detail.
fn is_asked_for(asked: Option<&Reason>, found: &Reason) -> bool {
    asked.is_none_or(|asked| asked.key() == found.key())
}

/// [`StepMemory::list_refusal_reasons`](super::StepMemory::list_refusal_reasons), inside
/// the caller's transaction.
pub(crate) fn list_refusal_reasons(conn: &Connection, reason: Option<&Reason>, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ItemRefusalReason>, TreeError> {
    let refused = list_said_facts(conn, REFUSED, after, limit, |said| said.refusal.map(|refusal| refusal.reason).filter(|found| is_asked_for(reason, found)))?;
    Ok(refused.into_iter().map(|(item, reason)| ItemRefusalReason { item, reason }).collect())
}

/// [`StepMemory::count_refusal_reasons`](super::StepMemory::count_refusal_reasons), inside
/// the caller's transaction.
pub(crate) fn count_refusal_reasons(conn: &Connection, reason: Option<&Reason>) -> Result<u64, TreeError> {
    Ok(list_refusal_reasons(conn, reason, None, usize::MAX)?.len() as u64)
}

/// [`StepMemory::list_space_refusal_reasons`](super::StepMemory::list_space_refusal_reasons),
/// inside the caller's transaction.
pub(crate) fn list_space_refusal_reasons(conn: &Connection, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ItemRefusalReason>, TreeError> {
    let refused = list_said_facts(conn, REFUSED_FOR_SPACE, after, limit, |said| said.space_refusal)?;
    Ok(refused.into_iter().map(|(item, space_refusal)| ItemRefusalReason { item, reason: Reason::TooBig(Some((space_refusal.content.size(), space_refusal.free_space))) }).collect())
}

/// [`StepMemory::count_space_refusals`](super::StepMemory::count_space_refusals), inside
/// the caller's transaction.
pub(crate) fn count_space_refusals(conn: &Connection) -> Result<u64, TreeError> {
    Ok(list_space_refusal_reasons(conn, None, usize::MAX)?.len() as u64)
}
