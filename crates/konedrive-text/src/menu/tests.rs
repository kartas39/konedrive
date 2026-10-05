use super::{free_up_why, FREE_UP_WHY, KEPT_BY_A_FOLDER, NOT_IN_ONEDRIVE};
use crate::files::{entry, Operation};
use crate::{pieces, Client};

/// A tooltip is a whole sentence with no place but the folder above, and the
/// refusal it speaks for has a sentence of the plugin's for a free-up.
#[test]
fn the_tooltips_are_whole_and_stand_beside_a_refusals_sentence() {
    let whole = |tooltip: &str| {
        assert!(tooltip.starts_with(char::is_uppercase) && tooltip.ends_with('.') && !tooltip.contains('%'), "{tooltip}");
        assert!(pieces(tooltip).iter().all(|piece| matches!(piece, Ok(_) | Err("by"))), "{tooltip}");
    };
    for text in &FREE_UP_WHY {
        whole(text.tooltip);
        assert_eq!(free_up_why(text.why), Some(text.tooltip));
        if let Some(refusal) = &text.refusal {
            let sentence = entry(refusal).and_then(|entry| entry.sentence.of(Operation::FreeUp));
            assert!(sentence.and_then(|sentence| sentence.of(Client::Desktop)).is_some(), "{}", text.why);
        }
    }
    whole(KEPT_BY_A_FOLDER);
    whole(NOT_IN_ONEDRIVE);
    assert_eq!(free_up_why("a-new-code"), None);
}
