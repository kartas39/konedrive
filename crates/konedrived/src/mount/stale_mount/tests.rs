use std::path::Path;

use super::{ask_statistics, decide_is_mount_stale, is_stale_mount_at, StatisticsAnswer};

#[test]
fn a_mount_is_stale_when_its_statistics_are_refused_as_not_connected() {
    assert!(decide_is_mount_stale(StatisticsAnswer::Refused { errno: libc::ENOTCONN }));
    assert!(decide_is_mount_stale(StatisticsAnswer::Refused { errno: libc::ECONNABORTED }), "the request that was under way when the daemon went");
    assert!(!decide_is_mount_stale(StatisticsAnswer::Answered), "its daemon runs");
    assert!(!decide_is_mount_stale(StatisticsAnswer::NoneInTime), "a slow daemon is not a dead one");
    assert!(!decide_is_mount_stale(StatisticsAnswer::Refused { errno: libc::EACCES }), "another refusal says nothing of the daemon");
    assert!(!decide_is_mount_stale(StatisticsAnswer::Refused { errno: libc::EIO }));
}

/// A plain directory answers its statistics, a path that is not there is refused, and
/// neither is a stale mount.
#[test]
fn a_directory_with_no_mount_over_it_is_no_stale_mount() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(ask_statistics(dir.path()), StatisticsAnswer::Answered);
    assert_eq!(ask_statistics(&dir.path().join("absent")), StatisticsAnswer::Refused { errno: libc::ENOENT });
    assert!(!is_stale_mount_at(dir.path()));
    assert!(!is_stale_mount_at(&dir.path().join("absent")));
    assert!(!is_stale_mount_at(Path::new("/proc")), "a mount of another kind");
}
