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
