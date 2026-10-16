use super::*;
use crate::status::sync_trouble::CycleError;

/// A snapshot whose folder is `folder`, the rest as at the start.
fn folder(folder: FolderStatus) -> SyncSnapshot {
    SyncSnapshot { folder, ..SyncSnapshot::default() }
}

#[test]
fn the_published_state_is_computed_from_the_registration_and_the_sync() {
    let mut s = folder(FolderStatus { root_state: RootState::Ready, ..FolderStatus::default() });
    assert_eq!(published_state(&s), "ready");
    s.cycle.listing = true;
    assert_eq!(published_state(&s), "listing");
    s.cycle.sync_trouble = Some(SyncTrouble::Cycle(CycleError::Offline("timed out".into())));
    assert_eq!(published_state(&s), "listing", "no network is said, not an error");
    s.cycle.sync_trouble = Some(SyncTrouble::Cycle(CycleError::SignedOut));
    assert_eq!(published_state(&s), "error");
    s.folder.root_state = RootState::Error;
    let path = std::path::PathBuf::from("/home/u/OneDrive");
    s.folder.trouble = Some(FolderTrouble::NoRootId { path });
    s.local.conflict_count = 1;
    let expected = format!("{}. {}", s.folder.trouble.as_ref().unwrap(), CycleError::SignedOut);
    assert_eq!(published_error(&s), expected, "a conflict is not a problem; it is not in LastError");
    assert_eq!(published_state(&SyncSnapshot::default()), "none");
}
