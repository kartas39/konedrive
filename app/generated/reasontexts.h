// Generated from the catalogue in crates/konedrive-text (src/reasons.rs, src/waits.rs) by src/cpp.rs.
// Do not edit: change the catalogue, then run `KONEDRIVE_UPDATE_GENERATED=1 cargo test -p konedrive-text`.
#pragma once

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
