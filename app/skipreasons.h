#pragma once

#include <QString>

/// Why the folder cannot show an item OneDrive has, in the window's words: the reason
/// `Skipped()` gives, and a `name-taken-in-onedrive` event's detail. The same sentences,
/// word for word, as `konedrivectl`'s `skip_reason_text`, which a test of its own
/// compares with this source; a reason this build does not know reads as the last one.
QString skipReasonText(const QString &reason);
