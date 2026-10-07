//! Why an entry of Dolphin's menu is disabled, in a tooltip's length: the
//! plugin's alone (`konedrivectl sync menu` prints the answer of
//! `Files.Menu` as it is).
//!
//! "Free up space" is disabled with a code (`free-up-why`, the daemon's
//! `sync/menu.rs`), most of which say beforehand what a refusal of the call
//! would say: [`WhyText::refusal`] names it, and its long sentence is in
//! [`crate::files`]. The place of a tooltip: `{by}`, the folder above that
//! keeps the item on this device (`blocked-by`).

use konedrive_dbus::Refusal;

/// The words of one code of `free-up-why`.
#[derive(Debug, Clone)]
pub struct WhyText {
    /// The code, as the daemon spells it.
    pub why: &'static str,
    /// What `FreeUp` would be refused under; `None` for a code that is no refusal.
    pub refusal: Option<Refusal>,
    pub tooltip: &'static str,
}

/// Every code of `free-up-why`, in the daemon's order.
pub static FREE_UP_WHY: [WhyText; 4] = [
    WhyText { why: "pinned-above", refusal: Some(Refusal::NotAllowed), tooltip: "Kept on this device because “{by}” is; unpin it first." },
    WhyText {
        why: "no-helper",
        refusal: Some(Refusal::NoHelper),
        tooltip: "The konedrive helper is not connected. Try again once it is — it reconnects on its own.",
    },
    WhyText { why: "not-uploaded", refusal: Some(Refusal::NotUploaded), tooltip: "Not uploaded yet: freeing it up would lose the changes made here." },
    // The daemon cannot tell now: no refusal says this.
    WhyText { why: "unknown", refusal: None, tooltip: "KOneDrive cannot tell yet whether a change here waits to be uploaded. Try again in a moment." },
];

/// "Always keep on this device" is checked and cannot be unchecked
/// (`always-keep` is `on-locked`).
pub const KEPT_BY_A_FOLDER: &str = "Kept on this device because “{by}” is.";

/// "Open in OneDrive" is disabled: the item has no page there yet.
pub const NOT_IN_ONEDRIVE: &str = "Not in OneDrive yet.";

/// The tooltip of a code; `None` for one this build does not know.
pub fn free_up_why(why: &str) -> Option<&'static str> {
    FREE_UP_WHY.iter().find(|entry| entry.why == why).map(|entry| entry.tooltip)
}

#[cfg(test)]
mod tests;
