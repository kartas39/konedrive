#pragma once

#include <QString>

/// What an outbox row's reason, an `upload-failed` event's detail or a
/// NotUploaded() reason means, in the window's words: the same meanings as
/// konedrivectl's `upload_reason_text`, pointing at the window instead of
/// commands. A reason it does not know is kept as the daemon wrote it.
QString uploadReasonText(const QString &reason);
