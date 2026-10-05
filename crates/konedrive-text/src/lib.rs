//! What the codes mean, in words: the one catalogue of the sentences the
//! clients show for a reason (`konedrive-reason`'s `Reason`, `LocalSkip` and
//! `WaitsFor`) and for a refused operation on a file (`konedrive-dbus`'s
//! `Refusal`).
//!
//! The daemon sends codes and does not translate. The sentences are written
//! here once, in English: [`reasons`] for why a change is not uploaded,
//! [`waits`] for what keeps an item on this computer, [`files`] for a file
//! that was not kept on this device, unpinned, freed up or opened in
//! OneDrive, [`menu`] for why an entry of Dolphin's menu is disabled.
//! `konedrivectl` reads them directly ([`reasons::text`],
//! [`waits::text`], [`files::text`]). The window and the Dolphin plugin get
//! C++ files generated from them ([`cpp`]), every sentence inside `i18n`, so
//! translation stays in the clients, where KDE's tools expect it.
//!
//! A sentence is written as the window or the plugin shows it: a capital
//! first letter, a full stop, and named places for what is filled in
//! (`{detail}`, `{path}`, `{needs}`, `{file}`). Where the clients must differ
//! because they name their own controls, or say different things, there is
//! one for each ([`Sentence::Each`]), and only then.
//!
//! A new code gets its words here, or the crate's tests fail: every key of
//! `konedrive-reason` and every name of `Refusal` has an entry, and the
//! generated files in git are what the generator writes now.

pub mod cpp;
pub mod files;
pub mod menu;
pub mod reasons;
pub mod waits;

/// Who shows a sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    /// The window or the Dolphin plugin, which point at their own pages,
    /// buttons and emblems.
    Desktop,
    /// `konedrivectl`, which names the command to run.
    CommandLine,
}

/// A sentence of the catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sentence {
    /// The same for every client.
    Same(&'static str),
    /// One for each client, where they name their own controls or say
    /// different things.
    Each { desktop: &'static str, command_line: &'static str },
    /// The window's or the plugin's alone: `konedrivectl` says of this what
    /// it says of its other commands, from its own table.
    Desktop(&'static str),
}

impl Sentence {
    /// The sentence `client` shows; `None` where the catalogue has none for it.
    pub const fn of(&self, client: Client) -> Option<&'static str> {
        match (self, client) {
            (Self::Same(sentence), _) | (Self::Desktop(sentence), Client::Desktop) => Some(sentence),
            (Self::Each { desktop, .. }, Client::Desktop) => Some(desktop),
            (Self::Each { command_line, .. }, Client::CommandLine) => Some(command_line),
            (Self::Desktop(_), Client::CommandLine) => None,
        }
    }
}

/// A sentence cut at its places: `Ok` is text as it is written, `Err` the
/// name of a place (`{detail}` gives `Err("detail")`).
pub fn pieces(sentence: &str) -> Vec<Result<&str, &str>> {
    let mut pieces = Vec::new();
    let mut rest = sentence;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else { break };
        if open > 0 {
            pieces.push(Ok(&rest[..open]));
        }
        pieces.push(Err(&rest[open + 1..open + close]));
        rest = &rest[open + close + 1..];
    }
    if !rest.is_empty() {
        pieces.push(Ok(rest));
    }
    pieces
}

/// The sentence with its places filled in. What is filled in is never read
/// for places again; a place with no value stays as it is written.
pub fn fill(sentence: &str, places: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for piece in pieces(sentence) {
        match piece {
            Ok(text) => out.push_str(text),
            Err(name) => match places.iter().find(|(place, _)| *place == name) {
                Some((_, value)) => out.push_str(value),
                None => {
                    out.push('{');
                    out.push_str(name);
                    out.push('}');
                }
            },
        }
    }
    out
}

#[cfg(test)]
mod tests;
