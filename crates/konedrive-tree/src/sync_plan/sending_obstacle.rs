//! What is not sent to OneDrive, and why: the rules a step towards OneDrive is asked before
//! it is planned. A decision of what is sent and of values the plan was given; it reads no
//! row.

use konedrive_reason::Reason;

use super::SendingState;

/// What a step towards OneDrive sends of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sending {
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

impl Sending {
    /// Whether the item's name here is sent.
    fn has_name(self) -> bool {
        matches!(self, Sending::NewDirectory | Sending::NewFile | Sending::Place)
    }

    /// Whether a file's content is sent.
    fn has_content(self) -> bool {
        matches!(self, Sending::NewFile | Sending::Content)
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
    /// The largest size OneDrive takes.
    pub(crate) largest_file_size: u64,
    /// The time the quiet time of the file's content ends; `None` when it has ended.
    pub(crate) quiet_until: Option<i64>,
    pub(crate) is_onedrive_full: bool,
    pub(crate) sending_state: SendingState,
    /// Since when the item is unsent ([`Item::unsent_since`](crate::Item::unsent_since)).
    pub(crate) unsent_since: Option<u64>,
}

/// Why something of an item is not sent now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendingObstacle {
    /// The item is left here: never sent.
    Ignored,
    /// The folder is read-only, and the item came to be unsent after it was switched.
    ReadOnlyFolder,
    /// OneDrive cannot hold the name; it is not sent and not altered.
    NameRefused(Reason),
    /// The file is larger than OneDrive takes.
    TooLarge,
    /// OneDrive is full: no content is sent. Moves and removals go on.
    OneDriveFull,
    /// The content was changed within the quiet time, which ends at `until`.
    QuietTime { until: i64 },
}

/// Why `sending` of an item is not sent, under `conditions`; `None` when it is. The first
/// that holds, what never ends by itself before what does:
///
/// - an ignored item is never sent, whatever is sent of it;
/// - a read-only folder sends only what was unsent when it was switched;
/// - a name OneDrive cannot hold, where the name is sent;
/// - a file larger than OneDrive takes, OneDrive being full, and the quiet time, where
///   content is sent.
pub(crate) fn find_sending_obstacle(sending: Sending, conditions: &SendingConditions) -> Option<SendingObstacle> {
    if conditions.is_ignored {
        return Some(SendingObstacle::Ignored);
    }
    if let SendingState::ReadOnly { last_unsent_since } = conditions.sending_state {
        if conditions.unsent_since.is_none_or(|since| since > last_unsent_since) {
            return Some(SendingObstacle::ReadOnlyFolder);
        }
    }
    if sending.has_name() {
        if let Some(reason) = &conditions.name_refusal {
            return Some(SendingObstacle::NameRefused(reason.clone()));
        }
    }
    if sending.has_content() {
        if conditions.size.is_some_and(|size| size > conditions.largest_file_size) {
            return Some(SendingObstacle::TooLarge);
        }
        if conditions.is_onedrive_full {
            return Some(SendingObstacle::OneDriveFull);
        }
        if let Some(until) = conditions.quiet_until {
            return Some(SendingObstacle::QuietTime { until });
        }
    }
    None
}

#[cfg(test)]
mod tests;
