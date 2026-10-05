use std::collections::BTreeSet;

use konedrive_reason::{LocalSkip, Reason, WaitsFor};

use super::reasons::{self, REASONS};
use super::waits::{self, WaitSentence, STILL_HERE, WAITS};
use super::{fill, pieces, Client, Sentence};

fn bytes(n: u64) -> String {
    format!("{n} B")
}

fn said(stored: &str, client: Client) -> String {
    reasons::text(stored, client, &bytes)
}

/// Every key of `konedrive-reason` has an entry, and every entry is a key:
/// a new code without words, or with no decision that it has none, fails here.
#[test]
fn every_key_of_a_reason_and_a_skip_has_an_entry() {
    let keys: BTreeSet<&str> = Reason::ALL.iter().map(Reason::key).chain(LocalSkip::ALL.iter().map(LocalSkip::key)).collect();
    let entries: Vec<&str> = REASONS.iter().map(|entry| entry.key).collect();
    let unique: BTreeSet<&str> = entries.iter().copied().collect();
    assert_eq!(unique.len(), entries.len(), "a key has two entries");
    assert_eq!(unique, keys);
}

/// One of every variant of `WaitsFor` but `Other`. A new variant does not
/// compile until it is listed.
fn every_wait() -> Vec<WaitsFor> {
    let path = || "a/b".to_owned();
    let all = vec![
        WaitsFor::Cycle,
        WaitsFor::Uploads(3),
        WaitsFor::Changes(path()),
        WaitsFor::OpenForWriting(path()),
        WaitsFor::UnknownState(path()),
        WaitsFor::NotDownloaded(path()),
        WaitsFor::LocalOnly(path()),
        WaitsFor::MountedInside(path()),
        WaitsFor::MovedAway(path()),
    ];
    for waits in &all {
        match waits {
            WaitsFor::Cycle
            | WaitsFor::Uploads(_)
            | WaitsFor::Changes(_)
            | WaitsFor::OpenForWriting(_)
            | WaitsFor::UnknownState(_)
            | WaitsFor::NotDownloaded(_)
            | WaitsFor::LocalOnly(_)
            | WaitsFor::MountedInside(_)
            | WaitsFor::MovedAway(_) => {}
            WaitsFor::Other(_) => unreachable!("not a key of its own"),
        }
    }
    all
}

/// The same for what keeps an item here; and each sentence has the place
/// its variant fills.
#[test]
fn every_key_of_what_is_waited_for_has_an_entry() {
    let all = every_wait();
    let keys: Vec<&str> = all.iter().map(WaitsFor::key).collect();
    let entries: Vec<&str> = WAITS.iter().map(|entry| entry.key).collect();
    assert_eq!(entries, keys);
    for waits in &all {
        let text = waits::text(&waits.to_string()).unwrap();
        match waits::entry(waits.key()).unwrap().sentence {
            WaitSentence::Path(_) => assert!(text.contains("a/b") && !text.contains('{'), "{text}"),
            WaitSentence::Count { .. } => assert_eq!(text, "Still on this computer: 3 changes in it wait to be uploaded."),
            WaitSentence::Plain => assert_eq!(text, STILL_HERE),
        }
    }
    assert_eq!(waits::text(""), None);
    assert_eq!(waits::text("uploads:1").unwrap(), "Still on this computer: 1 change in it waits to be uploaded.");
    // What no key spells, though it begins like one.
    for stored in ["uploads:many", "cycle:1", "something-new:a/b"] {
        assert_eq!(waits::text(stored).unwrap(), STILL_HERE, "{stored}");
    }
}

/// A sentence is written as the window shows it, names only places that are
/// filled, and the clients differ only where an entry says so.
#[test]
fn the_sentences_are_whole_and_their_places_are_filled() {
    for entry in REASONS {
        for (sentence, places) in [(entry.bare, &["key"][..]), (entry.detailed, &["detail", "needs", "free"][..])] {
            let Some(sentence) = sentence else { continue };
            for client in [Client::Desktop, Client::CommandLine] {
                let text = sentence.of(client).expect("a reason's sentence is every client's");
                assert!(text.starts_with(char::is_uppercase), "{}: {text}", entry.key);
                for piece in pieces(text) {
                    match piece {
                        Ok(written) => assert!(!written.contains(['{', '}', '%']), "{}: {text}", entry.key),
                        Err(name) => assert!(places.contains(&name), "{}: no place {{{name}}} here: {text}", entry.key),
                    }
                }
                // A sentence ends with a full stop, or with the detail it gives.
                assert!(text.ends_with('.') || text.ends_with("{detail}"), "{}: {text}", entry.key);
            }
            if let Sentence::Each { desktop, command_line } = sentence {
                assert_ne!(desktop, command_line, "{}: one sentence is enough", entry.key);
            }
        }
        // Nothing with a detail is worded while the key alone is not.
        assert!(entry.bare.is_some() || entry.detailed.is_none(), "{}", entry.key);
    }
}

/// How a stored reason is cut and worded: the key alone, a detail, sizes,
/// and what has no sentence, which comes back as it was sent.
#[test]
fn a_stored_reason_is_worded_by_its_key_and_what_stands_behind_it() {
    assert_eq!(reasons::split("refused: bad: name"), ("refused", Some("bad: name")));
    assert_eq!(reasons::split("too-big:300:20"), ("too-big", Some("300:20")));
    assert_eq!(reasons::split("network"), ("network", None));
    assert_eq!(reasons::split("mass-delete: 3"), ("mass-delete: 3", None));

    assert_eq!(said("refused", Client::Desktop), "Refused by OneDrive.");
    assert_eq!(said("refused: bad: name", Client::CommandLine), "OneDrive refused it: bad: name");
    assert_eq!(said("refused: {key}", Client::Desktop), "OneDrive refused it: {key}");
    assert_eq!(said("no-guard", Client::Desktop), "KOneDrive's record of this change is incomplete (no-guard): it stays here until the file is changed again.");
    assert_eq!(said("too-big:300:20", Client::Desktop), "Too big: needs 300 B, 20 B free.");
    assert_eq!(said("too-big:0300:+20", Client::CommandLine), "Too big: needs 300 B, 20 B free.");
    assert_eq!(said("state-unreadable: Input/output error (os error 5)", Client::Desktop), "The file's KOneDrive state cannot be read: it stays here until the file is replaced (Input/output error (os error 5)).");
    for stored in ["download-failed", "download-failed: errno 5", "network: connection reset", "symlink: x", "too-big: x", "too-big:x:1", "too-big:", "ignored", "something-new", ""] {
        for client in [Client::Desktop, Client::CommandLine] {
            assert_eq!(said(stored, client), stored);
        }
    }
    assert_eq!(fill("{a} and {b}, {a}", &[("a", "{b}"), ("b", "2")]), "{b} and 2, {b}");
}

/// The clients say the same but where each names its own controls.
#[test]
fn the_clients_differ_only_in_their_own_controls() {
    let differing: Vec<&str> = REASONS.iter().filter(|entry| said(entry.key, Client::Desktop) != said(entry.key, Client::CommandLine)).map(|entry| entry.key).collect();
    assert_eq!(differing, ["mass-delete", "waiting-for-space", "too-big"]);
    assert_eq!(said("mass-delete", Client::Desktop), "Part of a large delete: delete it in OneDrive too, or restore it, on the Status page.");
    assert_eq!(said("mass-delete", Client::CommandLine), "Part of a large delete: confirm it (`sync deletes confirm`) or undo it (`sync deletes restore`).");
    assert_eq!(said("waiting-for-space", Client::Desktop), "OneDrive is full: free up space in OneDrive, then Refresh.");
    assert_eq!(said("waiting-for-space", Client::CommandLine), "Waiting for space: OneDrive is full.");
    assert_eq!(said("too-big", Client::CommandLine), "Too big for the space left in OneDrive: free up space there, then `sync refresh`.");
}
