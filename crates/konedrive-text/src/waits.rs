//! What keeps an item on this computer that the folder cannot hold any more:
//! the sentence of every key of `WaitsFor` (the third field of a line of
//! `Skipped()`). The same for every client.
//!
//! The places: `{path}` (what keeps it) and `{count}` (how many changes).

use konedrive_reason::WaitsFor;

use crate::fill;

/// The sentence of a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitSentence {
    /// With the `{path}` of what keeps the item.
    Path(&'static str),
    /// With the `{count}` of changes that wait: for one, and for any other number.
    Count { one: &'static str, many: &'static str },
    /// Nothing to name: [`STILL_HERE`].
    Plain,
}

/// The words of one key.
#[derive(Debug, Clone, Copy)]
pub struct WaitText {
    /// The key, as `konedrive-reason` spells it.
    pub key: &'static str,
    pub sentence: WaitSentence,
}

/// What is said when nothing can be named: no cycle has looked yet, or the
/// string is one no key spells.
pub const STILL_HERE: &str = "Still on this computer: it leaves once nothing in it waits to be uploaded.";

/// Every key of `WaitsFor`, in its order.
pub const WAITS: &[WaitText] = &[
    WaitText { key: "cycle", sentence: WaitSentence::Plain },
    WaitText {
        key: "uploads",
        sentence: WaitSentence::Count {
            one: "Still on this computer: 1 change in it waits to be uploaded.",
            many: "Still on this computer: {count} changes in it wait to be uploaded.",
        },
    },
    WaitText { key: "changes", sentence: WaitSentence::Path("Still on this computer: what was done at {path} on this computer has not reached OneDrive yet.") },
    WaitText { key: "open-for-writing", sentence: WaitSentence::Path("Still on this computer: {path} is open in a program.") },
    WaitText {
        key: "unknown-state",
        sentence: WaitSentence::Path("Still on this computer: whether {path} holds changes cannot be read. Move it out of the folder or delete it."),
    },
    WaitText {
        key: "not-downloaded",
        sentence: WaitSentence::Path("Still on this computer: {path} is not downloaded and is not where OneDrive has it. Move it out of the folder."),
    },
    WaitText {
        key: "local-only",
        sentence: WaitSentence::Path("Still on this computer: {path} is only here (its name is on the ignore list). Move it out of the folder or delete it."),
    },
    WaitText { key: "mounted-inside", sentence: WaitSentence::Path("Still on this computer: another filesystem is mounted at {path}. Unmount it.") },
    WaitText {
        key: "moved-in-onedrive",
        sentence: WaitSentence::Path(
            "Still on this computer: {path} was moved in OneDrive, and the name it has there is taken on this computer. Rename or move what has that name.",
        ),
    },
];

/// The entry of a key.
pub fn entry(key: &str) -> Option<&'static WaitText> {
    WAITS.iter().find(|entry| entry.key == key)
}

/// What `Skipped()` says keeps an item here (`waits`, as sent), in words;
/// `None` for an item that is not on this computer (the empty string).
pub fn text(waits: &str) -> Option<String> {
    if waits.is_empty() {
        return None;
    }
    let waits = WaitsFor::parse(waits);
    // What no key spells may still begin like one (`uploads:many`).
    let sentence = match &waits {
        WaitsFor::Other(_) => None,
        known => entry(known.key()).map(|entry| entry.sentence),
    };
    Some(match (sentence, &waits) {
        (Some(WaitSentence::Count { one, .. }), WaitsFor::Uploads(1)) => one.to_owned(),
        (Some(WaitSentence::Count { many, .. }), WaitsFor::Uploads(count)) => fill(many, &[("count", &count.to_string())]),
        (Some(WaitSentence::Path(sentence)), known) => fill(sentence, &[("path", known.path().unwrap_or_default())]),
        _ => STILL_HERE.to_owned(),
    })
}
