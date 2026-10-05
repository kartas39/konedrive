#pragma once

#include <QString>

/// What an outbox row's reason, an `upload-failed` event's detail or a
/// NotUploaded() reason means, in the window's words. The sentences are the
/// catalogue's (`crates/konedrive-text`), which `konedrivectl` reads too:
/// `generated/reasontexts.cpp` is written from it. A reason with no sentence
/// there is kept as the daemon wrote it.
QString uploadReasonText(const QString &reason);
