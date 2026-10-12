use super::*;

/// Every reason is written as it was read, byte for byte: with a detail,
/// with sizes, and what no variant spells.
#[test]
fn a_stored_reason_is_written_back_as_it_was_read() {
    let typed = [
        ("refused: The name is not allowed", Reason::Refused(Some("The name is not allowed".into()))),
        ("refused: ", Reason::Refused(Some(String::new()))),
        ("refused", Reason::Refused(None)),
        ("not allowed now: the folder is read-only: for now", Reason::NotAllowed(Some("the folder is read-only: for now".into()))),
        ("too-big:300:20", Reason::TooBig(Some((300, 20)))),
        ("too-big", Reason::TooBig(None)),
        ("quiet-time", Reason::QuietTime),
    ];
    for (stored, reason) in typed {
        assert_eq!(Reason::parse(stored), reason, "{stored}");
        assert_eq!(reason.to_string(), stored);
    }
    // No variant spells these: a key that takes no detail with one behind it, sizes
    // not written as the daemon writes them, an older version's text, nothing at all.
    for stored in ["network: connection reset", "too-big:0300:20", "too-big:x", "hash-mismatch:01ABC", "unreadable kind \"frobnicate\"", "error sending request", ""] {
        let reason = Reason::parse(stored);
        assert_eq!(reason, Reason::Other(stored.to_owned()), "{stored}");
        assert_eq!(reason.to_string(), stored);
    }
}

/// The key, the group and the detail of what no variant spells are what
/// the string says.
#[test]
fn what_no_variant_spells_still_has_its_key_and_group() {
    let of = |stored: &str| {
        let reason = Reason::parse(stored);
        (reason.key().to_owned(), reason.group(), reason.detail())
    };
    assert_eq!(of("network: connection reset"), ("network".into(), Some(Group::Waiting), Some("connection reset".into())));
    assert_eq!(of("too-big:0300:20"), ("too-big".into(), Some(Group::OneAction), Some("0300:20".into())));
    assert_eq!(of("locked: odd"), ("locked".into(), Some(Group::Waiting), Some("odd".into())));
    assert_eq!(of("something-new"), ("something-new".into(), None, None));
    assert!(Reason::parse("too-big:0300:20").waits_for_space() && Reason::parse("too-big:x").waits_for_space());
    assert_eq!(Reason::parse("too-big:0300:20").sizes(), Some((300, 20)));
    assert!(!Reason::TooBig(None).waits_for_space() && !Reason::Quota.waits_for_space());
    assert!(Reason::parse("").is_empty() && !Reason::Blocked.is_empty());
}

/// The key of a variant is the key its stored string is summed under.
#[test]
fn a_variants_key_is_the_key_of_its_stored_string() {
    for reason in Reason::ALL {
        assert_eq!(key_of(&reason.to_string()), reason.key());
        assert_eq!(Reason::from_key(reason.key()), Some(reason.clone()));
        assert_eq!(known_group(reason.key()), reason.group(), "{reason}");
        assert_eq!(reason.detail(), None, "{reason}");
    }
    for group in Group::ALL {
        assert_eq!(Group::parse(group.as_str()), Some(group));
    }
}

/// The group of every key, written out: what `NotUploadedSummary()` lists
/// each reason under.
#[test]
fn every_key_has_the_group_it_had() {
    let table: [(Option<Group>, &[&str]); 4] = [
        (Some(Group::OneAction), &["quota-exceeded", "waiting-for-space", "too-big", "forbidden"]),
        (
            Some(Group::PerFile),
            &[
                "name-characters",
                "name-spaces",
                "name-reserved",
                "name-not-utf8",
                "too-large",
                "refused",
                "blocked",
                "not-read-on-disk",
            ],
        ),
        (
            Some(Group::Waiting),
            &[
                "open-for-writing",
                "being-checked",
                "quiet-time",
                "locked",
                "not-found",
                "changed-while-sending",
                "parent-not-in-onedrive",
                "hash-mismatch",
                "network",
                "local-error",
                "index-error",
                "upload-error",
                "paused",
                "upload-session-open",
                "name-held-by-an-upload",
                "changed in OneDrive again and again",
                "the upload session ended twice",
                "not allowed now",
            ],
        ),
        (None, &["something-new", "symlink", "unreadable"]),
    ];
    let mut keys = 0;
    for (group, of) in table {
        for key in of {
            assert_eq!(known_group(key), group, "{key}");
            keys += 1;
        }
    }
    // Every key of the table is above, and three that are not in it.
    assert_eq!(keys, Reason::ALL.len() + 3);
}

/// What an item still waits for is stored and sent as one string, and read
/// back as it was written; a word nobody knows is kept.
#[test]
fn what_an_item_waits_for_is_read_as_it_was_written() {
    let all = [
        WaitsFor::Cycle,
        WaitsFor::Uploads(3),
        WaitsFor::OpenForWriting("/home/u/OneDrive/docs/new: file.txt".into()),
    ];
    for waits in all {
        assert_eq!(WaitsFor::parse(&waits.to_string()), waits);
    }
    assert_eq!(WaitsFor::Uploads(2).to_string(), "uploads:2");
    assert_eq!(WaitsFor::parse("changes:docs/a.txt"), WaitsFor::Other("changes:docs/a.txt".into()), "an older daemon's key");
    assert_eq!(WaitsFor::parse("something-new:x"), WaitsFor::Other("something-new:x".into()));
    assert_eq!(WaitsFor::parse("something-new:x").key(), "something-new");
}
