// --- The skip-reason wording is one sentence, shared with the window -----

/// Parses `whyText`'s `if (reason == QLatin1String("<reason>")) { return
/// i18n("<sentence>"); }` branches out of `app/synccontroller.cpp`'s source,
/// in order, as `(reason, sentence)` pairs — plus the function's final,
/// unconditional `return i18n("<sentence>");` (the fallback for anything not
/// named above) as the pair `("unsupported", <that sentence>)`, matching the
/// name `skip_reason_text`'s own fallback answers to.
fn parse_why_text_branches(cpp: &str) -> Vec<(String, String)> {
    let start = cpp.find("QString whyText").expect("whyText(...) not found in synccontroller.cpp");
    let end = start + cpp[start..].find("\n}\n").expect("no closing brace found for whyText");
    let mut rest = &cpp[start..end];
    let mut pairs = Vec::new();
    while let Some(reason_at) = rest.find("QLatin1String(\"") {
        let after_reason_open = &rest[reason_at + "QLatin1String(\"".len()..];
        let reason_end = after_reason_open.find('"').expect("unterminated QLatin1String");
        let reason = after_reason_open[..reason_end].to_owned();

        let after_reason = &after_reason_open[reason_end..];
        let sentence_at = after_reason.find("i18n(\"").expect("no i18n(...) after this QLatin1String");
        let after_sentence_open = &after_reason[sentence_at + "i18n(\"".len()..];
        let sentence_end = after_sentence_open.find("\");").expect("unterminated i18n(...)");
        let sentence = after_sentence_open[..sentence_end].to_owned();

        pairs.push((reason, sentence));
        rest = &after_sentence_open[sentence_end..];
    }
    // What is left is the tail after the last named branch: the function's
    // final, unconditional return — the fallback.
    let fallback_at = rest.find("i18n(\"").expect("no fallback return i18n(...) after the named branches");
    let after_fallback_open = &rest[fallback_at + "i18n(\"".len()..];
    let fallback_end = after_fallback_open.find("\");").expect("unterminated fallback i18n(...)");
    pairs.push(("unsupported".to_owned(), after_fallback_open[..fallback_end].to_owned()));
    pairs
}

/// `konedrivectl::skip_reason_text` and the window's `whyText`
/// (`app/synccontroller.cpp`) are meant to say exactly the same thing for
/// each reason (see `crates/konedrivectl/src/lib.rs`'s doc comment on
/// `skip_reason_text`), so a person reading `konedrivectl sync skipped` and
/// a person reading the window see one explanation, not two that happen to
/// agree today. Checking only "does the Rust sentence appear somewhere in
/// the C++ file" (the guard's first cut) would still pass if two branches'
/// bodies were swapped — every sentence would still be *present*, just
/// answering the wrong reason. Parsing each branch's own (reason, sentence)
/// pair out of the C++ source and comparing it against
/// `skip_reason_text(reason)` directly closes that gap: it fails if a
/// reason's C++ sentence and its Rust sentence disagree, in either
/// direction, including a swap between two reasons that both still have
/// *a* sentence, just not the *right* one.
#[test]
fn skip_reason_text_matches_every_branch_of_the_windows_whytext() {
    let cpp = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../app/synccontroller.cpp"
    ))
    .unwrap();
    let branches = parse_why_text_branches(&cpp);
    assert_eq!(
        branches.iter().map(|(reason, _)| reason.as_str()).collect::<Vec<_>>(),
        vec!["name-too-long", "personal-vault", "shared", "onenote", "reserved-name", "unsupported"],
        "whyText's branches changed shape; update this parser or the reason list"
    );
    for (reason, sentence) in &branches {
        assert_eq!(
            konedrivectl::skip_reason_text(reason),
            sentence,
            "app/synccontroller.cpp's whyText(\"{reason}\") and \
             konedrivectl::skip_reason_text(\"{reason}\") must say exactly the same thing"
        );
    }
}

// --- The activity's words are one contract with the window --------------

/// One of the window's source files: from `app/`, or from the directory
/// `KONEDRIVE_APP_SOURCE` names — how the guard below is shown to fail on a
/// changed copy without touching `app/` itself.
fn app_source(name: &str) -> String {
    let dir = std::env::var("KONEDRIVE_APP_SOURCE")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../app").to_owned());
    std::fs::read_to_string(format!("{dir}/{name}")).unwrap_or_else(|e| panic!("{dir}/{name}: {e}"))
}

/// Every activity kind the window branches on: each `kind ==
/// QLatin1String("…")` in `app/activitymodel.cpp`, once.
fn kinds_the_window_branches_on(cpp: &str) -> Vec<String> {
    const MARK: &str = "kind == QLatin1String(\"";
    let mut kinds = Vec::new();
    let mut rest = cpp;
    while let Some(at) = rest.find(MARK) {
        let after = &rest[at + MARK.len()..];
        let end = after.find('"').expect("unterminated QLatin1String");
        kinds.push(after[..end].to_owned());
        rest = &after[end..];
    }
    kinds.sort();
    kinds.dedup();
    kinds
}

/// The window turns the daemon's events into
/// notifications by their kind and by one exact detail (A2 in the
/// limitations log), and nothing else ties the two sides together. Every
/// kind `app/activitymodel.cpp` branches on must be one the daemon sends
/// (`Kind::as_str`), the ones its notifications hang on must be among them,
/// and "not enough disk space" must be `activity::NO_DISK_SPACE` word for
/// word — so a rename on either side fails here, not in a user's tray.
#[test]
fn the_window_branches_on_the_daemons_own_activity_words() {
    use konedrived::status::activity::{Kind, NO_DISK_SPACE};
    let cpp = app_source("activitymodel.cpp");
    let sent: Vec<&str> = Kind::ALL.iter().map(|kind| kind.as_str()).collect();
    let window = kinds_the_window_branches_on(&cpp);
    for kind in &window {
        assert!(sent.contains(&kind.as_str()), "the window branches on {kind:?}, which the daemon never sends ({sent:?})");
    }
    for kind in [Kind::UpdateFailed, Kind::Failed, Kind::Conflict] {
        assert!(
            window.iter().any(|k| k == kind.as_str()),
            "the window no longer branches on {:?}, which the daemon sends: {window:?}",
            kind.as_str()
        );
    }
    assert!(
        cpp.contains(&format!("QLatin1String(\"{NO_DISK_SPACE}\")")),
        "the window does not recognise the daemon's words for a full disk, {NO_DISK_SPACE:?}"
    );
}
