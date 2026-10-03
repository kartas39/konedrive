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
    // lifting the previous entry out first, as `with` does — the check
    // itself no longer trusts a matching id.
    let previous = roots.remove_owned(1000, "a").expect("the entry is there");
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

/// A root id as the daemon mints them, different for each `n`.
fn id(n: u32) -> String {
    format!("{n:08x}-0000-4000-8000-000000000000")
}

fn registered(uid: u32, ino: u64) -> Root {
    Root { uid, dev: 42, ino, path: format!("/home/u{uid}/folder{ino}"), root_id: id(ino as u32) }
}

/// `root_id` is whatever string a peer sends, up to a whole datagram, and it
/// is stored in `roots.json` and written to the log: only the form the
/// daemon mints is registered.
#[test]
fn a_root_is_registered_only_under_a_root_id() {
    let roots = Roots::default();
    for bad in ["", "measure-root", "x".repeat(60_000).as_str(), "1c2e4f5a-0b3c-1d5e-8f60-71829a3b4c5d"] {
        let root = Root { root_id: bad.to_owned(), ..registered(1000, 7) };
        assert_eq!(roots.with(root).unwrap_err(), Refused::NotAnId, "{:?}", shown_id(bad));
    }
    let accepted = roots.with(registered(1000, 7)).expect("a root id is taken");
    assert_eq!(accepted.roots.owner_of(&id(7)), Some(1000));
    assert!(accepted.displaced.is_none());
    assert_eq!(roots.iter().count(), 0, "the registrations decided on are left as they were");
}

/// One uid cannot grow the helper's list without end, and its bound is its
/// own: a root it already holds is still registered again, and another
/// user is not refused for it.
#[test]
fn a_uid_holds_a_bounded_number_of_roots() {
    let mut roots = Roots::default();
    for ino in 0..MAX_ROOTS_PER_UID as u64 {
        roots = roots.with(registered(1000, ino)).expect("within the bound").roots;
    }
    assert_eq!(roots.held_by(1000), MAX_ROOTS_PER_UID);
    let one_more = roots.with(registered(1000, 500)).unwrap_err();
    assert_eq!(one_more, Refused::TooMany);
    assert_eq!(one_more.errno(), libc::EDQUOT);

    let again = roots.with(registered(1000, 3)).expect("a root already held is announced again");
    assert_eq!(again.roots.held_by(1000), MAX_ROOTS_PER_UID);
    assert!(roots.with(registered(1001, 600)).is_ok(), "another user has a bound of its own");
}

/// An id registered again comes back with the entry it replaces, so that
/// the caller can unmark the old directory when it is another one; another
/// user's id is refused, and so is an overlap with any other root.
#[test]
fn registering_an_id_again_hands_back_the_entry_it_replaces() {
    let roots = Roots::default().with(registered(1000, 7)).unwrap().roots;

    let moved = Root { ino: 8, path: "/home/u1000/copy".into(), ..registered(1000, 7) };
    let accepted = roots.with(moved).expect("the user's own id, on another directory");
    let displaced = accepted.displaced.expect("the old entry");
    assert_eq!((displaced.ino, displaced.path.as_str()), (7, "/home/u1000/folder7"));
    assert_eq!(accepted.roots.iter().count(), 1);
    assert_eq!(accepted.roots.iter().next().unwrap().ino, 8);

    let same = roots.with(registered(1000, 7)).unwrap();
    assert_eq!(same.displaced.map(|old| old.ino), Some(7), "the same directory: nothing to unmark");

    let stolen = Root { uid: 1001, ..registered(1000, 7) };
    assert_eq!(roots.with(stolen).unwrap_err(), Refused::AnotherUsers);
    let nested = Root { path: "/home/u1000/folder7/sub".into(), ..registered(1000, 9) };
    assert_eq!(roots.with(nested).unwrap_err(), Refused::Overlap(Nesting::Inside(id(7))));
}

/// A root registered before the form of its id was checked is still the
/// user's to unregister, and nobody else's; and what the log says of such an
/// id is short and escaped.
#[test]
fn a_root_with_an_older_id_can_still_be_unregistered() {
    let mut roots = Roots::default();
    roots.insert(Root { root_id: "measure-root".into(), ..registered(1000, 7) });
    assert!(roots.without(1001, "measure-root").is_none(), "not another user's to remove");
    let (left, removed) = roots.without(1000, "measure-root").expect("its owner's to remove");
    assert_eq!(removed.ino, 7);
    assert_eq!(left.iter().count(), 0);
    assert_eq!(roots.iter().count(), 1, "the registrations decided on are left as they were");

    assert_eq!(shown_id(&id(7)), id(7));
    assert_eq!(shown_id("a\nb"), "\"a\\nb\"");
    let long = shown_id(&"x".repeat(60_000));
    assert!(long.len() < 80 && long.ends_with("(60000 bytes)"), "{long}");
}

/// A directory's name is its owner's to choose, line breaks and all, and a
/// path is up to `PATH_MAX` of them: what the log says of one is one line,
/// and short.
#[test]
fn a_path_in_the_log_is_escaped_and_cut() {
    assert_eq!(shown_path("/home/u/One\nDrive"), "\"/home/u/One\\nDrive\"");
    let long = shown_path(&format!("/{}", "d/".repeat(2000)));
    assert!(long.len() < 240 && long.ends_with("(4001 bytes)"), "{long}");
}

/// `with` compares the new directory with every *other* root, and hands the
/// id's own previous entry back: whether the id may move from that directory
/// to this one is asked of the entry. A directory inside the old one, or
/// containing it, shares marks with it, and taking the old tree's marks off
/// would leave the shared part unmarked until the new walk — so the helper
/// refuses the move while the old directory is still where it was.
#[test]
fn an_ids_own_previous_directory_is_asked_about_an_overlap() {
    let roots = Roots::default().with(registered(1000, 7)).unwrap().roots;
    let overlap = |path: &str| {
        let moved = Root { ino: 8, path: path.into(), ..registered(1000, 7) };
        let old = roots.with(moved).expect("no other root is in the way").displaced.unwrap();
        old.overlap_with(path)
    };
    assert_eq!(overlap("/home/u1000/folder7/sub"), Some(Nesting::Inside(id(7))));
    assert_eq!(overlap("/home/u1000"), Some(Nesting::Contains(id(7))));
    assert_eq!(overlap("/home/u1000/folder7"), Some(Nesting::Inside(id(7))), "the same path");
    assert_eq!(overlap("/home/u1000/folder70"), None, "a sibling shares nothing");
}
