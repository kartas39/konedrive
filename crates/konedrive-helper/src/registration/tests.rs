use super::*;

/// As `register_root` asks it: the id names an entry but does
/// not own one, so an entry already held by somebody else is refused and
/// a user's own is a re-registration.
#[test]
fn a_root_id_held_by_another_user_is_refused() {
    let mut roots = roots::Roots::default();
    roots.insert(roots::Root {
        uid: 1000,
        dev: 42,
        ino: 7,
        path: "/home/alice/OneDrive".into(),
        root_id: "shared-id".into(),
    });
    let refused = |uid: u32| roots.owner_of("shared-id").is_some_and(|other| other != uid);
    assert!(refused(1001), "another user must not take over the id");
    assert!(!refused(1000), "the owner re-registering its own root must not be refused");
    assert!(
        !roots.owner_of("unused-id").is_some_and(|other| other != 1001),
        "an unused id is free for anyone"
    );
}
