use std::os::unix::fs::MetadataExt;

use super::*;

fn root(uid: u32, dev: u64, ino: u64) -> Root {
    Root {
        uid,
        dev,
        ino,
        path: format!("/home/u{uid}/OneDrive"),
        root_id: format!("r{ino}"),
    }
}

#[test]
fn accepts_objects_on_a_registered_root_of_the_same_user() {
    let mut roots = Roots::default();
    roots.insert(root(1000, 42, 7));
    assert!(roots.may_act_on(1000, 42, 1000));
}

#[test]
fn refuses_another_users_objects_and_other_filesystems() {
    let mut roots = Roots::default();
    roots.insert(root(1000, 42, 7));
    assert!(!roots.may_act_on(1001, 42, 1001), "wrong uid");
    assert!(!roots.may_act_on(1000, 43, 1000), "wrong filesystem");
    assert!(!roots.may_act_on(1000, 42, 1001), "object owned by someone else");
}

#[test]
fn round_trips_through_the_state_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roots.json");
    let mut roots = Roots::default();
    roots.insert(root(1000, 42, 7));
    roots.save(&path).unwrap();
    let loaded = Roots::load(&path).unwrap();
    assert!(loaded.may_act_on(1000, 42, 1000));
}

#[test]
fn a_missing_state_file_loads_empty() {
    let dir = tempfile::tempdir().unwrap();
    let roots = Roots::load(&dir.path().join("absent.json")).unwrap();
    assert!(!roots.may_act_on(1000, 42, 1000));
}

/// The file lists where every user keeps their files; the helper is root
/// and nothing else needs to read it.
#[test]
fn the_state_file_is_not_world_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roots.json");
    let mut roots = Roots::default();
    roots.insert(root(1000, 42, 7));
    roots.save(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
    assert_eq!(mode, 0o600, "got {mode:o}");
}

/// `Restart=always` turns "refuse to start" into "never intercept
/// anything", so a damaged file must cost the registrations and nothing
/// more.
#[test]
fn a_corrupt_state_file_is_moved_aside_and_the_helper_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roots.json");
    std::fs::write(&path, b"{ this is not json at all").unwrap();

    let roots = Roots::load(&path).expect("a corrupt file must not stop startup");
    assert_eq!(roots.iter().count(), 0);
    assert!(!path.exists(), "the damaged file must not be left in place to fail again");
    let aside = dir.path().join("roots.corrupt");
    assert!(aside.exists(), "and it must be kept for a human to look at");
    assert_eq!(std::fs::read(&aside).unwrap(), b"{ this is not json at all");
}

#[test]
fn only_the_owner_can_unregister_a_root() {
    let mut roots = Roots::default();
    roots.insert(root(1000, 42, 7));
    assert!(
        roots.remove_owned(1001, "r7").is_none(),
        "another user must not be able to remove it"
    );
    assert!(roots.remove_owned(1000, "nonexistent").is_none());
    assert_eq!(roots.iter().count(), 1);
    let removed = roots.remove_owned(1000, "r7").expect("the owner can remove it");
    assert_eq!(removed.path, "/home/u1000/OneDrive", "and gets the entry back to undo it");
    assert_eq!(roots.iter().count(), 0);
}

#[test]
fn a_root_may_not_nest_in_or_contain_another() {
    let mut roots = Roots::default();
    roots.insert(Root {
        uid: 1000,
        dev: 42,
        ino: 7,
        path: "/home/u/OneDrive".into(),
        root_id: "a".into(),
    });

    assert_eq!(
        roots.nesting_conflict("/home/u/OneDrive/Work", 42, 9),
        Some(Nesting::Inside("a".into()))
    );
    assert_eq!(
        roots.nesting_conflict("/home/u", 42, 9),
        Some(Nesting::Contains("a".into()))
    );
    assert_eq!(
        roots.nesting_conflict("/home/u/OneDrive", 42, 7),
        Some(Nesting::SameDirectory("a".into())),
        "the same directory under a second id is the same overlap"
    );

    // Re-announcing an existing root is still allowed, but only by
    // lifting the previous entry out first — the check itself no longer
    // trusts a matching id.
    let previous = roots.take("a").expect("the entry is there");
    assert_eq!(roots.nesting_conflict("/home/u/OneDrive", 42, 7), None);
    roots.insert(previous);
    assert_eq!(roots.iter().count(), 1);
}

/// `by_id` is keyed by a string the client chooses, so the
/// key authorises nothing on its own; the helper must ask who owns it.
#[test]
fn a_root_id_belongs_to_the_user_who_registered_it() {
    let mut roots = Roots::default();
    roots.insert(root(1000, 42, 7));
    assert_eq!(roots.owner_of("r7"), Some(1000));
    assert_eq!(roots.owner_of("never-registered"), None);
}

/// The overlap a second user could previously hide behind a reused id:
/// with the id no longer skipped, their nested directory is refused.
#[test]
fn reusing_another_users_root_id_no_longer_hides_an_overlap() {
    let mut roots = Roots::default();
    roots.insert(Root {
        uid: 1000,
        dev: 42,
        ino: 7,
        path: "/home/alice/OneDrive".into(),
        root_id: "shared-id".into(),
    });
    assert_eq!(
        roots.nesting_conflict("/home/alice/OneDrive/Sub", 42, 8),
        Some(Nesting::Inside("shared-id".into())),
        "a nested path is an overlap however the asker labels it"
    );
}

/// The comparison is by path component, not by string prefix: a sibling
/// whose name merely starts with the same letters is not nested.
#[test]
fn a_sibling_with_a_similar_name_is_not_nested() {
    let mut roots = Roots::default();
    roots.insert(Root {
        uid: 1000,
        dev: 42,
        ino: 7,
        path: "/home/u/OneDrive".into(),
        root_id: "a".into(),
    });
    assert_eq!(roots.nesting_conflict("/home/u/OneDrive2", 42, 9), None);
    assert_eq!(roots.nesting_conflict("/home/u/One", 42, 9), None);
    assert_eq!(roots.nesting_conflict("/srv/other", 43, 9), None);
}

/// Ownership of a regular file is the whole test, with no
/// root anywhere — and not one bit less than ownership.
#[test]
fn clearing_an_ignore_mark_needs_only_ownership_of_a_regular_file() {
    assert!(may_clear_ignore(1000, 1000, true), "one's own file, no root needed");
    assert!(!may_clear_ignore(1000, 1001, true), "somebody else's file");
    assert!(!may_clear_ignore(1000, 1000, false), "not a regular file");
}

#[test]
fn has_root_for_is_per_user() {
    let mut roots = Roots::default();
    assert!(!roots.has_root_for(1000));
    roots.insert(root(1000, 42, 7));
    assert!(roots.has_root_for(1000));
    assert!(!roots.has_root_for(1001));
}
