//! What is not sent to OneDrive, and why: the rules a step towards OneDrive is asked before
//! it is planned. A decision of what is sent and of values the plan was given; it reads no
//! row.

use konedrive_reason::Reason;

use super::NextTry;

/// What a step towards OneDrive sends of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SentPart {
    /// A directory made here: its name.
    NewDirectory,
    /// A file made here: its name and its content.
    NewFile,
    /// The content of a file OneDrive has.
    Content,
    /// A move or a rename: the item's name here.
    Place,
    Removal,
}

impl SentPart {
    /// Whether the item's name here is sent.
    fn has_name(self) -> bool {
        matches!(self, SentPart::NewDirectory | SentPart::NewFile | SentPart::Place)
    }

    /// Whether a file's content is sent.
    fn has_content(self) -> bool {
        matches!(self, SentPart::NewFile | SentPart::Content)
    }
}

/// What the plan was given about one item that says whether something of it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SendingConditions {
    /// The item is left here: it has no binding, and its name or that of a directory above
    /// it is ignored.
    pub(crate) is_ignored: bool,
    /// Why OneDrive cannot hold the item's name here; `None` when it can.
    pub(crate) name_refusal: Option<Reason>,
    /// The size of the file's content here; `None` for a directory.
    pub(crate) size: Option<u64>,
    /// The time the quiet time of the file's content ends; `None` when it has ended.
    pub(crate) quiet_until: Option<i64>,
    pub(crate) is_onedrive_full: bool,
    /// Why OneDrive refused what was sent of the item, for good or for lack of space, as
    /// the steps remember it; `None` when it did not.
    pub(crate) refusal: Option<Reason>,
    /// The next try of what is sent of the item, as the steps remember it after a failure.
    pub(crate) next_try: Option<NextTry>,
    /// The time now, in Unix seconds.
    pub(crate) now: i64,
}

/// Why something of an item is not sent now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendingObstacle {
    /// The item is left here: never sent.
    Ignored,
    /// OneDrive cannot hold the name; it is not sent and not altered.
    NameRefused(Reason),
    /// OneDrive is full: no content is sent. Moves and removals go on.
    OneDriveFull,
    /// The content was changed within the quiet time, which ends at `until`.
    QuietTime { until: i64 },
    /// OneDrive refused what was sent of the item, for good or for lack of space: it is not
    /// sent again until the user did something about it.
    Refused(Reason),
    /// A try failed for this reason, and the next is not before `until`.
    TriedAgainLater { until: i64, reason: Reason },
}

/// Why `sent_part` of an item is not sent, under `conditions`; `None` when it is. The first
/// that holds, what never ends by itself before what does:
///
/// - an ignored item is never sent, whatever is sent of it;
/// - a name OneDrive cannot hold, where the name is sent;
/// - what OneDrive refused of the item, as the steps remember it, whatever is sent;
/// - OneDrive being full, and the quiet time, where content is sent;
/// - a next try that is not before a time still to come, whatever is sent.
///
/// A file larger than OneDrive takes is not foreseen: OneDrive's `413` is kept as a
/// refusal for good (`docs/design/sync.md` §6.6).
pub(crate) fn find_sending_obstacle(sent_part: SentPart, conditions: &SendingConditions) -> Option<SendingObstacle> {
    if conditions.is_ignored {
        return Some(SendingObstacle::Ignored);
    }
    if sent_part.has_name() {
        if let Some(reason) = &conditions.name_refusal {
            return Some(SendingObstacle::NameRefused(reason.clone()));
        }
    }
    if let Some(reason) = &conditions.refusal {
        return Some(SendingObstacle::Refused(reason.clone()));
    }
    if sent_part.has_content() {
        if conditions.is_onedrive_full {
            return Some(SendingObstacle::OneDriveFull);
        }
        if let Some(until) = conditions.quiet_until {
            return Some(SendingObstacle::QuietTime { until });
        }
    }
    if let Some(next_try) = conditions.next_try.as_ref().filter(|next_try| next_try.not_before > conditions.now) {
        return Some(SendingObstacle::TriedAgainLater { until: next_try.not_before, reason: next_try.reason.clone() });
    }
    None
}

#[cfg(test)]
mod tests;
