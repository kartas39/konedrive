//! The C++ the window is built with, written from the catalogue: every
//! sentence a literal inside `i18n` or `i18np`, so `xgettext` finds it, with
//! `%1`, `%2` for its places.
//!
//! The files are kept in git ([`files`] says where), and the C++ build does
//! not run cargo. The crate's `generated` test writes them again and fails
//! when one in git differs; run with `KONEDRIVE_UPDATE_GENERATED=1` it
//! rewrites them instead.

use konedrive_reason::{known_group, LocalSkip, Reason, TOO_BIG_PREFIX};

use crate::reasons::{takes_sizes, ReasonText, REASONS};
use crate::waits::{WaitSentence, STILL_HERE, WAITS};
use crate::{pieces, Client};

/// A generated file.
pub struct Generated {
    /// Where it is kept, from the top of the repository.
    pub path: &'static str,
    pub content: String,
}

/// Every generated file, as the catalogue has it now.
pub fn files() -> Vec<Generated> {
    vec![
        Generated { path: "app/generated/reasontexts.h", content: window_header() },
        Generated { path: "app/generated/reasontexts.cpp", content: window_source() },
    ]
}

/// The first lines of a generated file.
fn banner() -> String {
    "// Generated from the catalogue in crates/konedrive-text (src/reasons.rs, src/waits.rs) by src/cpp.rs.\n\
     // Do not edit: change the catalogue, then run `KONEDRIVE_UPDATE_GENERATED=1 cargo test -p konedrive-text`.\n"
        .to_owned()
}

/// A C++ string literal.
fn literal(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A sentence as KDE's `i18n` takes it: `%1`, `%2` for its places, in the
/// order they are first named, and the names in that order. A `%` of the
/// sentence itself would be read as a place, so there is none.
fn numbered(sentence: &str) -> (String, Vec<&str>) {
    assert!(!sentence.contains('%'), "a % in a sentence is a place to i18n: {sentence}");
    let mut names: Vec<&str> = Vec::new();
    let mut out = String::new();
    for piece in pieces(sentence) {
        match piece {
            Ok(text) => out.push_str(text),
            Err(name) => {
                let at = names.iter().position(|known| *known == name).unwrap_or_else(|| {
                    names.push(name);
                    names.len() - 1
                });
                out.push_str(&format!("%{}", at + 1));
            }
        }
    }
    (out, names)
}

/// `i18n("…", a, b)`: the sentence with the C++ expression of each place.
fn i18n(sentence: &str, expression: &dyn Fn(&str) -> &'static str) -> String {
    let (text, names) = numbered(sentence);
    let mut call = format!("i18n({}", literal(&text));
    for name in names {
        call.push_str(", ");
        call.push_str(expression(name));
    }
    call.push(')');
    call
}

/// The keys behind which `: <detail>` is read as a detail (`key_of`): those
/// of either table that have a group.
fn keys_with_a_detail() -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let reasons = Reason::ALL.iter().map(|reason| reason.key().to_owned());
    let skips = LocalSkip::ALL.iter().map(|skip| skip.key().to_owned());
    for key in reasons.chain(skips) {
        if known_group(&key).is_some() && !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

fn window_header() -> String {
    let mut out = banner();
    out.push_str(
        r#"#pragma once

#include <QString>

/// A reason as the daemon sends it, cut as konedrive-reason's `key_of` cuts
/// it: the key it is listed under, and what stands behind it (after
/// `<key>: `, or the sizes after `too-big:`).
struct ReasonParts {
    QString key;
    QString detail;
    bool hasDetail = false;
    /// The sizes of `too-big:<needs>:<free>`, in bytes.
    bool hasSizes = false;
    quint64 needs = 0;
    quint64 free = 0;
};

ReasonParts reasonParts(const QString &stored);

/// The window's sentence for a reason; `needs` and `free` are the sizes of a
/// `too-big` one, in words. Empty for a reason with no sentence, which is
/// shown as the daemon wrote it.
QString reasonSentence(const ReasonParts &parts, const QString &needs, const QString &free);

/// What Skipped() says keeps an item on this computer that the folder cannot
/// hold any more (`<key>:<detail>`), in words; empty for an item that is not
/// here (the empty string).
QString stillHereSentence(const QString &waits);
"#,
    );
    out
}

fn window_source() -> String {
    let mut out = banner();
    out.push_str(
        r#"#include "reasontexts.h"

#include <KLocalizedString>

#include <QStringView>

namespace
{
/// A whole number as Rust's `u64` reads one: digits only, a `+` allowed before them.
bool wholeNumber(QStringView text, quint64 &value)
{
    if (text.startsWith(QLatin1Char('+'))) {
        text = text.mid(1);
    }
    if (text.isEmpty()) {
        return false;
    }
    for (const QChar c : text) {
        if (c < QLatin1Char('0') || c > QLatin1Char('9')) {
            return false;
        }
    }
    bool fits = false;
    value = text.toULongLong(&fits);
    return fits;
}

/// The sizes of `<needs>:<free>`.
bool sizesOf(QStringView detail, quint64 &needs, quint64 &free)
{
    const qsizetype colon = detail.indexOf(QLatin1Char(':'));
    return colon >= 0 && wholeNumber(detail.left(colon), needs) && wholeNumber(detail.mid(colon + 1), free);
}

/// Whether `: <detail>` behind this key is a detail: the keys that have a group.
bool takesDetail(QStringView key)
{
    static const QLatin1String keys[] = {
"#,
    );
    for key in keys_with_a_detail() {
        out.push_str(&format!("        QLatin1String({}),\n", literal(&key)));
    }
    let prefix = literal(TOO_BIG_PREFIX);
    let too_big = literal(TOO_BIG_PREFIX.trim_end_matches(':'));
    out.push_str(&format!(
        r#"    }};
    for (const QLatin1String known : keys) {{
        if (key == known) {{
            return true;
        }}
    }}
    return false;
}}
}}

ReasonParts reasonParts(const QString &stored)
{{
    ReasonParts parts;
    const QLatin1String tooBig({prefix});
    if (stored.startsWith(tooBig) && sizesOf(QStringView(stored).mid(tooBig.size()), parts.needs, parts.free)) {{
        parts.key = QStringLiteral({too_big});
        parts.detail = stored.mid(tooBig.size());
        parts.hasDetail = true;
        parts.hasSizes = true;
        return parts;
    }}
    const qsizetype at = stored.indexOf(QLatin1String(": "));
    if (at >= 0 && takesDetail(QStringView(stored).left(at))) {{
        parts.key = stored.left(at);
        parts.detail = stored.mid(at + 2);
        parts.hasDetail = true;
        return parts;
    }}
    parts.key = stored;
    return parts;
}}

QString reasonSentence(const ReasonParts &parts, const QString &needs, const QString &free)
{{
"#
    ));
    for entry in REASONS {
        out.push_str(&reason_branch(entry));
    }
    out.push_str(
        r#"    Q_UNUSED(needs)
    Q_UNUSED(free)
    return QString();
}

QString stillHereSentence(const QString &waits)
{
    if (waits.isEmpty()) {
        return QString();
    }
    const qsizetype colon = waits.indexOf(QLatin1Char(':'));
    const QString key = colon < 0 ? waits : waits.left(colon);
    const QString detail = colon < 0 ? QString() : waits.mid(colon + 1);
"#,
    );
    for entry in WAITS {
        let key = literal(entry.key);
        match entry.sentence {
            WaitSentence::Plain => {}
            WaitSentence::Path(sentence) => {
                let call = i18n(sentence, &|name| match name {
                    "path" => "detail",
                    other => panic!("{}: no place {{{other}}} in a sentence with a path", entry.key),
                });
                out.push_str(&format!("    if (key == QLatin1String({key})) {{\n        return {call};\n    }}\n"));
            }
            WaitSentence::Count { one, many } => {
                let (many, names) = numbered(many);
                assert_eq!(names, ["count"], "{}: the sentence for many has the count, and nothing else", entry.key);
                assert!(pieces(one).iter().all(Result::is_ok), "{}: the sentence for one has no place", entry.key);
                out.push_str(&format!(
                    "    if (key == QLatin1String({key})) {{\n        quint64 count = 0;\n        if (wholeNumber(detail, count)) {{\n            return i18np({}, {}, count);\n        }}\n    }}\n",
                    literal(one),
                    literal(&many)
                ));
            }
        }
    }
    out.push_str(&format!("    return i18n({});\n}}\n", literal(STILL_HERE)));
    out
}

/// The branch of one key in `reasonSentence`; nothing for a key with no sentence.
fn reason_branch(entry: &ReasonText) -> String {
    let place = |name: &str| match name {
        "key" => "parts.key",
        "detail" => "parts.detail",
        "needs" => "needs",
        "free" => "free",
        other => panic!("{}: no place {{{other}}} in a reason's sentence", entry.key),
    };
    let bare = entry.bare.map(|sentence| sentence.of(Client::Window));
    let detailed = entry.detailed.map(|sentence| sentence.of(Client::Window));
    if bare.is_none() && detailed.is_none() {
        return String::new();
    }
    let mut out = format!("    if (parts.key == QLatin1String({})) {{\n", literal(entry.key));
    if let Some(sentence) = bare {
        assert!(!takes_sizes(sentence) && !sentence.contains("{detail}"), "{}: nothing stands behind the key alone", entry.key);
        out.push_str(&format!("        if (!parts.hasDetail) {{\n            return {};\n        }}\n", i18n(sentence, &place)));
    }
    if let Some(sentence) = detailed {
        let given = if takes_sizes(sentence) { "parts.hasSizes" } else { "parts.hasDetail" };
        out.push_str(&format!("        if ({given}) {{\n            return {};\n        }}\n", i18n(sentence, &place)));
    }
    out.push_str("        return QString();\n    }\n");
    out
}
