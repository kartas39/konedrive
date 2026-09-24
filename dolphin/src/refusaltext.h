// What to tell a person when the daemon did not do what they asked for.
//
// Matches the D-Bus error *name*, never the message, and says what happened
// to their file and what they can do -- the same things `konedrivectl` says
// (crates/konedrivectl/src/lib.rs, `refusal_text`), in Dolphin's words.

#pragma once

#include <QList>
#include <QString>

namespace konedrive
{

enum class Operation {
    /// "Always keep on this device", checking it: calls Pin(paths).
    AlwaysKeep,
    /// "Always keep on this device", unchecking it: calls Unpin(paths).
    /// Windows-like -- this only unpins, and never frees space on its own.
    Unpin,
    /// "Free up space": calls FreeUp(paths).
    FreeUpSpace,
};

/// Not D-Bus errors: the names under which a file that was not sent at all
/// is reported -- one still waiting for an earlier answer, or one past the
/// cap on calls waiting at once.
inline constexpr char AlreadyWaitingError[] = "org.konedrive.Dolphin.AlreadyWaiting";
inline constexpr char TooManyWaitingError[] = "org.konedrive.Dolphin.TooManyWaiting";

struct Failure {
    QString path;
    QString errorName;
    QString message;
};

/// One file's refusal, as a sentence naming the file.
QString refusalText(Operation operation, const Failure &failure);

/// One message for any number of failures: each different reason once,
/// explained for the first file it happened to, with a count of the others.
QString failureSummary(Operation operation, const QList<Failure> &failures);

} // namespace konedrive
