// Generated from the catalogue in crates/konedrive-text (src/files.rs, src/menu.rs) by src/cpp.rs.
// Do not edit: change the catalogue, then run `KONEDRIVE_UPDATE_GENERATED=1 cargo test -p konedrive-text`.
#pragma once

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
