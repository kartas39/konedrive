//! A name as OneDrive compares names: the one place a name is folded.

use std::ffi::{OsStr, OsString};

use icu_normalizer::ComposingNormalizerBorrowed;

/// A name in the form in which two names are one name to OneDrive, by the client's own
/// cautious rule: lower case, and Unicode's composed form (NFC). OneDrive takes two names
/// that differ by letters' case for one and does not say by which rule exactly; where this
/// rule and OneDrive's differ, OneDrive's answer to a request decides, and this rule only
/// foresees. Two names are compared by comparing what they fold to.
///
/// What it folds: the case of letters, as Unicode lowers it, and the forms Unicode has for
/// one composed letter. What it does not: letters Unicode calls compatible and not the same
/// (a ligature, a full-width letter), `ß` and `ss`, spaces and dots. A name that is not
/// UTF-8 folds to itself: no rule of letters is read into bytes that are no text.
///
/// Two places where the lowering is Rust's and reads more than one letter: a capital sigma
/// at the end of a word lowers to `ς` and elsewhere to `σ`, so `ΑΣ` and `ασ` do not fold to
/// one; and a capital I with a dot (`İ`) lowers to two characters, `i` and a combining dot,
/// which is not the fold of `i`. Each costs what a wrong rule costs: one refused request.
///
/// A fold is stored with its name (`items`' folded names), and looked up by equality. Its
/// tables are the compiler's (lower case) and `icu_normalizer`'s (NFC): an upgrade of
/// either may fold a name otherwise than its row says, and a lookup then misses with no
/// error. A change of the rule, or of what either table gives, is a new schema version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FoldedName(OsString);

impl FoldedName {
    /// `name`, folded.
    pub fn of(name: &OsStr) -> Self {
        let Some(text) = name.to_str() else { return Self(name.to_owned()) };
        // Composed before it is lowered and after: lowering reads one letter at a time, and
        // may leave a letter and its mark apart.
        let composed = ComposingNormalizerBorrowed::new_nfc();
        let lowered = composed.normalize(text).to_lowercase();
        Self(composed.normalize(&lowered).into_owned().into())
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }
}

#[cfg(test)]
mod tests;
