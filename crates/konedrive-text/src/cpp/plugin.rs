//! The Dolphin plugin's file: the table behind `refusalText`
//! (`dolphin/src/refusaltext.cpp`), from [`crate::files`].

use super::{banner, i18nc, literal};
use crate::files::{ByOperation, Operation, ALREADY_WAITING, FAILED, NOT_RUNNING, REFUSALS, STOPPED, TOO_MANY_WAITING, WAS_NOT};
use crate::menu::{FREE_UP_WHY, KEPT_BY_A_FOLDER, NOT_IN_ONEDRIVE};
use crate::Client;

const SOURCES: &str = "src/files.rs, src/menu.rs";

/// The plugin's name of an operation (`Operation`, `dolphin/src/refusaltext.h`).
fn enumerator(operation: Operation) -> &'static str {
    match operation {
        Operation::Keep => "Operation::AlwaysKeep",
        Operation::Unpin => "Operation::Unpin",
        Operation::FreeUp => "Operation::FreeUpSpace",
        Operation::OpenOnline => "Operation::OpenOnline",
    }
}

/// The C++ expression of a place of a sentence the plugin shows.
fn place(name: &str) -> &'static str {
    match name {
        "file" => "file",
        "detail" => "detail",
        "was not" => "wasNotWords(operation)",
        "count" => "count",
        "by" => "blockedBy",
        other => panic!("no place {{{other}}} in a sentence of the plugin"),
    }
}

fn info(sentence: &str) -> String {
    i18nc("@info", sentence, &place)
}

fn tooltip(sentence: &str) -> String {
    i18nc("@info:tooltip", sentence, &place)
}

/// The plugin's name of a code of `free-up-why` (`MenuAnswer::FreeUpWhy`,
/// `dolphin/src/syncclient.h`): `pinned-above` is `PinnedAbove`.
fn why_enumerator(why: &str) -> String {
    let name: String = why
        .split('-')
        .map(|word| {
            let mut letters = word.chars();
            letters.next().map(|first| first.to_uppercase().chain(letters).collect::<String>()).unwrap_or_default()
        })
        .collect();
    format!("MenuAnswer::FreeUpWhy::{name}")
}

/// The statements that return the call of `operation`: one `return` when
/// every operation has the same, else a `switch` in which the operations
/// with the same call share it and one with none leaves; then `after`.
fn by_operation(calls: &ByOperation<Option<String>>, indent: &str, after: &str) -> String {
    let all: Vec<(Operation, Option<&String>)> = Operation::ALL.iter().map(|&operation| (operation, calls.of_ref(operation))).collect();
    if let Some(first) = all[0].1 {
        if all.iter().all(|(_, call)| *call == Some(first)) {
            return format!("{indent}return {first};\n");
        }
    }
    let mut distinct: Vec<Option<&String>> = Vec::new();
    for (_, call) in &all {
        if !distinct.contains(call) {
            distinct.push(*call);
        }
    }
    let mut out = format!("{indent}switch (operation) {{\n");
    for call in distinct {
        for (operation, _) in all.iter().filter(|(_, theirs)| *theirs == call) {
            out.push_str(&format!("{indent}case {}:\n", enumerator(*operation)));
        }
        match call {
            Some(call) => out.push_str(&format!("{indent}    return {call};\n")),
            None => out.push_str(&format!("{indent}    break;\n")),
        }
    }
    out.push_str(&format!("{indent}}}\n{indent}{after}\n"));
    out
}

fn each(sentences: &ByOperation<&str>, call: &dyn Fn(&str) -> String) -> ByOperation<Option<String>> {
    ByOperation {
        keep: Some(call(sentences.keep)),
        unpin: Some(call(sentences.unpin)),
        free_up: Some(call(sentences.free_up)),
        open_online: Some(call(sentences.open_online)),
    }
}

pub(super) fn header() -> String {
    let mut out = banner(SOURCES);
    out.push_str(
        r#"#pragma once

#include "refusaltext.h"
#include "syncclient.h"

#include <QString>

namespace konedrive
{

/// The plugin's sentence for a call of `operation` refused under the D-Bus
/// error name `errorName`; `file` is the file's name, `detail` the daemon's
/// message. Empty for a name with no sentence of its own, which is told as
/// any failure (failedSentence).
QString refusalSentence(Operation operation, const QString &errorName, const QString &file, const QString &detail);

/// A refusal with no sentence of its own: `detail` is all there is.
QString failedSentence(Operation operation, const QString &file, const QString &detail);

/// "…, so “file” was not <these words>."
QString wasNotWords(Operation operation);

/// The call never reached a running daemon.
QString notRunningSentence(Operation operation, const QString &file);

/// The daemon left the bus while the call waited for it.
QString stoppedSentence(Operation operation, const QString &file);

/// The file was not sent: an earlier call for it has no answer yet.
QString alreadyWaitingSentence(const QString &file);

/// The file was not sent: `count` calls wait already.
QString tooManyWaitingSentence(Operation operation, const QString &file, int count);

/// Why "Free up space" is disabled, in a tooltip's length; `blockedBy` is the
/// folder above that keeps the item. Empty when no reason is said.
QString freeUpWhyToolTip(MenuAnswer::FreeUpWhy why, const QString &blockedBy);

/// "Always keep on this device" is checked and cannot be unchecked.
QString keptByAFolderToolTip(const QString &blockedBy);

/// "Open in OneDrive" is disabled.
QString notInOneDriveToolTip();

} // namespace konedrive
"#,
    );
    out
}

pub(super) fn source() -> String {
    let mut out = banner(SOURCES);
    out.push_str(
        r#"#include "refusaltexts.h"

#include <KLocalizedString>

namespace konedrive
{

QString refusalSentence(Operation operation, const QString &errorName, const QString &file, const QString &detail)
{
"#,
    );
    for entry in &REFUSALS {
        // As `files::text` decides: whether the operation has a sentence first, and only
        // then what is said instead of it without the daemon's message.
        let calls = entry.sentence.map(|sentence| {
            let call = info(sentence.and_then(|sentence| sentence.of(Client::Desktop))?);
            Some(match entry.without_detail {
                Some(bare) => format!("detail.isEmpty() ? {} : {call}", info(bare)),
                None => call,
            })
        });
        if Operation::ALL.iter().all(|&operation| calls.of_ref(operation).is_none()) {
            continue;
        }
        let name = entry.refusal.known_name().expect("an entry is a name this build knows");
        out.push_str(&format!("    if (errorName == QLatin1String({})) {{\n", literal(name)));
        out.push_str(&by_operation(&calls, "        ", "return QString();"));
        out.push_str("    }\n");
    }
    out.push_str(
        r#"    return QString();
}

QString failedSentence(Operation operation, const QString &file, const QString &detail)
{
"#,
    );
    out.push_str(&by_operation(&each(&FAILED, &info), "    ", "return detail;"));
    out.push_str("}\n\nQString wasNotWords(Operation operation)\n{\n");
    let words = |words: &str| i18nc("@info how a file was not changed", words, &place);
    out.push_str(&by_operation(&each(&WAS_NOT, &words), "    ", "return QString();"));
    out.push_str("}\n\nQString notRunningSentence(Operation operation, const QString &file)\n{\n");
    out.push_str(&format!("    return {};\n", info(NOT_RUNNING)));
    out.push_str("}\n\nQString stoppedSentence(Operation operation, const QString &file)\n{\n");
    out.push_str(&by_operation(&each(&STOPPED, &info), "    ", "return QString();"));
    out.push_str("}\n\nQString alreadyWaitingSentence(const QString &file)\n{\n");
    out.push_str(&format!("    return {};\n", info(ALREADY_WAITING)));
    out.push_str("}\n\nQString tooManyWaitingSentence(Operation operation, const QString &file, int count)\n{\n");
    out.push_str(&format!("    return {};\n", info(TOO_MANY_WAITING)));
    out.push_str("}\n\nQString freeUpWhyToolTip(MenuAnswer::FreeUpWhy why, const QString &blockedBy)\n{\n    switch (why) {\n");
    for text in &FREE_UP_WHY {
        out.push_str(&format!("    case {}:\n        return {};\n", why_enumerator(text.why), tooltip(text.tooltip)));
    }
    out.push_str("    case MenuAnswer::FreeUpWhy::NotSaid:\n        break;\n    }\n    return QString();\n");
    out.push_str("}\n\nQString keptByAFolderToolTip(const QString &blockedBy)\n{\n");
    out.push_str(&format!("    return {};\n", tooltip(KEPT_BY_A_FOLDER)));
    out.push_str("}\n\nQString notInOneDriveToolTip()\n{\n");
    out.push_str(&format!("    return {};\n", tooltip(NOT_IN_ONEDRIVE)));
    out.push_str("}\n\n} // namespace konedrive\n");
    out
}
