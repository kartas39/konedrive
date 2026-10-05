use konedrive_dbus::Refusal;

use super::{entry, failed, text, Operation, Told, ALREADY_WAITING, FAILED, NOT_RUNNING, REFUSALS, STOPPED, TOO_MANY_WAITING, WAS_NOT};
use crate::{pieces, Client, Sentence};

/// Every name a call is refused under has an entry, in `Refusal`'s order: a
/// new refusal without words for the file operations, or with no decision
/// that it has none, fails here.
#[test]
fn every_name_of_a_refusal_has_an_entry() {
    let entries: Vec<&Refusal> = REFUSALS.iter().map(|entry| &entry.refusal).collect();
    let all: Vec<&Refusal> = Refusal::ALL.iter().collect();
    assert_eq!(entries, all);
}

/// A sentence as it is written: no `%`, and only places from `places`.
fn whole(sentence: &str, places: &[&str]) {
    for piece in pieces(sentence) {
        match piece {
            Ok(written) => assert!(!written.contains(['{', '}', '%']), "{sentence}"),
            Err(name) => assert!(places.contains(&name), "no place {{{name}}} here: {sentence}"),
        }
    }
    // A capital, the file in its quotes, or a place: the path the sentence is about, a number.
    assert!(sentence.starts_with(char::is_uppercase) || sentence.starts_with(['“', '{']), "{sentence}");
    // A full stop, or the daemon's message, which ends as it ends.
    assert!(sentence.ends_with('.') || sentence.ends_with("{detail}"), "{sentence}");
}

/// A sentence is written as the plugin shows it, names only places its
/// client fills, and the clients differ only where an entry says so.
#[test]
fn the_sentences_are_whole_and_their_places_are_filled() {
    for entry in &REFUSALS {
        for operation in Operation::ALL {
            let Some(sentence) = entry.sentence.of(operation) else { continue };
            whole(sentence.of(Client::Desktop).expect("the plugin has every sentence there is"), &["file", "detail"]);
            if let Some(command_line) = sentence.of(Client::CommandLine) {
                whole(command_line, &["file", "detail", "prefix", "folder"]);
            }
            if let Sentence::Each { desktop, command_line } = sentence {
                assert_ne!(desktop, command_line, "{}: one sentence is enough", entry.refusal);
            }
            // What is said without the daemon's message stands for sentences that have a place for it.
            if entry.without_detail.is_some() {
                assert!(sentence.of(Client::Desktop).unwrap().contains("{detail}"), "{}", entry.refusal);
            }
        }
        if let Some(bare) = entry.without_detail {
            whole(bare, &[]);
        }
    }
    for operation in Operation::ALL {
        whole(FAILED.of(operation), &["file", "detail"]);
        whole(STOPPED.of(operation), &["file"]);
        assert!(pieces(WAS_NOT.of(operation)).iter().all(Result::is_ok));
    }
    whole(NOT_RUNNING, &["file", "was not"]);
    whole(ALREADY_WAITING, &["file"]);
    whole(TOO_MANY_WAITING, &["file", "was not", "count"]);
}

/// How a refusal is worded for each client: one sentence, one for each, the
/// plugin's alone, none.
#[test]
fn a_refusal_is_worded_for_its_client() {
    let told = Told { file: "/r/a.bin", detail: "the daemon's words", prefix: "konedrivectl --account Test", folder: " (/r)" };
    let said = |operation, refusal: Refusal, client| text(operation, &refusal, client, &told);
    // One for every client.
    for client in [Client::Desktop, Client::CommandLine] {
        assert_eq!(
            said(Operation::FreeUp, Refusal::NotHydrated, client).unwrap(),
            "“/r/a.bin” is not downloaded, so there is no space to free — it already takes none."
        );
    }
    // One for each.
    assert_eq!(said(Operation::OpenOnline, Refusal::NotUploaded, Client::Desktop).unwrap(), "“/r/a.bin” is not uploaded yet, so it has no page in OneDrive.");
    assert_eq!(
        said(Operation::OpenOnline, Refusal::NotUploaded, Client::CommandLine).unwrap(),
        "/r/a.bin is not uploaded yet, so it has no page in OneDrive. It can be opened there once it is uploaded (`konedrivectl --account Test sync outbox`)."
    );
    assert!(said(Operation::Keep, Refusal::OutsideRoot, Client::CommandLine).unwrap().contains("inside the sync folder (/r) can be kept"));
    // The plugin's alone.
    assert_eq!(said(Operation::FreeUp, Refusal::NotAllowed, Client::Desktop).unwrap(), "Could not free up space: the daemon's words");
    assert_eq!(said(Operation::FreeUp, Refusal::NotAllowed, Client::CommandLine), None);
    // None: told as any failure.
    assert_eq!(said(Operation::Keep, Refusal::NotUp, Client::Desktop), None);
    assert_eq!(said(Operation::OpenOnline, Refusal::NoHelper, Client::Desktop), None);
    assert_eq!(said(Operation::Keep, Refusal::Other("org.example.New".to_owned()), Client::Desktop), None);
    assert!(entry(&Refusal::Other("org.example.New".to_owned())).is_none());
    assert_eq!(failed(Operation::Unpin, &told), "Unpinning “/r/a.bin” failed: the daemon's words");
    // With and without the daemon's message; what is filled in is not read for places.
    assert_eq!(said(Operation::OpenOnline, Refusal::Unreachable, Client::CommandLine).unwrap(), "OneDrive could not be reached: the daemon's words");
    let bare = Told { detail: "", ..told };
    assert_eq!(text(Operation::Keep, &Refusal::Unreachable, Client::Desktop, &bare).unwrap(), "OneDrive could not be reached.");
    let braces = Told { file: "{detail}", ..told };
    assert_eq!(failed(Operation::FreeUp, &braces), "Freeing up “{detail}” failed: the daemon's words");
}
