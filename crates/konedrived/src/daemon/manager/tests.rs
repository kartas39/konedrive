use std::path::PathBuf;

use super::{HalfRemoved, ManagerError};
use crate::config::ConfigError;

/// SY5: a removal that failed half-way says only what is true of the folder, and answers
/// under the name its failure always had.
#[test]
fn a_half_removal_says_what_was_done_under_the_failures_own_name() {
    let folder = || Some(PathBuf::from("/home/u/OneDrive"));
    let said = |forgotten, still_recorded, signed_out| HalfRemoved { forgotten, still_recorded, signed_out }.text("Personal", "why");

    let forgotten = said(folder(), false, false);
    assert!(forgotten.contains("why") && forgotten.contains("/home/u/OneDrive is no longer registered"), "{forgotten}");
    assert!(!forgotten.contains("signed out"), "{forgotten}");
    let record_kept = said(folder(), true, true);
    assert!(record_kept.contains("signed out") && record_kept.contains("config.toml still records it"), "{record_kept}");
    assert!(!record_kept.contains("no longer registered"), "{record_kept}");
    let untouched = said(None, true, false);
    assert!(untouched.contains("left as it was") && !untouched.contains("no longer registered"), "{untouched}");
    assert!(!said(None, false, false).contains("folder"));

    let half = |forgotten| HalfRemoved { forgotten, still_recorded: true, signed_out: true };
    let gone = half(folder()).refusal("Personal", ManagerError::from(ConfigError::NoAccount("3f9a".into())));
    assert!(matches!(gone, ManagerError::NoAccount(_)), "{gone:?}");
    let gone = gone.to_string();
    assert!(gone.starts_with("there is no account \"3f9a\" in config.toml any more"), "{gone}");
    assert!(gone.contains("its sign-in is deleted and its folder /home/u/OneDrive forgotten"), "{gone}");
    assert!(gone.contains("Start konedrived again") && !gone.contains("Remove it again") && !gone.contains("still records"), "{gone}");
    let unwritable = half(None).refusal("Personal", ManagerError::from(ConfigError::Write("read-only file system".into())));
    assert!(matches!(&unwritable, ManagerError::Failed(said) if said.contains("is not removed: read-only file system")), "{unwritable:?}");
}
