use super::*;

/// second guard, kept per uid: an unregistration withholds
/// ignore marks only from its own user's files, so nobody can keep other
/// users' files unmarked by unregistering roots of their own in a loop.
#[test]
fn an_unregistration_counts_against_its_own_users_files_only() {
    let unregistrations = Unregistrations::new();
    let read = unregistrations.now();
    assert!(!unregistrations.since(read, Some(1000)), "nothing has happened yet");

    unregistrations.bump(1000);
    assert!(unregistrations.since(read, Some(1000)), "the walk began after the read");
    assert!(!unregistrations.since(read, Some(1001)), "and it was not user 1001's root");
    assert!(unregistrations.since(read, None), "a file whose owner is unknown counts it");
    let later = unregistrations.now();
    unregistrations.bump(1000);
    assert!(unregistrations.since(later, Some(1000)), "the walk's end counts too");
    assert!(!unregistrations.since(unregistrations.now(), Some(1000)), "read after both");
}

/// Past what is remembered, nobody can say whose an unregistration was,
/// so it counts against everyone.
#[test]
fn an_unregistration_that_is_no_longer_remembered_counts_against_everyone() {
    let unregistrations = Unregistrations::new();
    let read = unregistrations.now();
    for _ in 0..=UNREGISTRATIONS_REMEMBERED {
        unregistrations.bump(1000);
    }
    assert!(unregistrations.since(read, Some(1001)));
}
