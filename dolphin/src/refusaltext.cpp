#include "refusaltext.h"

#include "filestate.h"
#include "generated/refusaltexts.h"
#include "syncclient.h"

#include <KLocalizedString>

#include <QHash>

namespace konedrive
{

namespace
{
const QLatin1String KonedriveErrorPrefix("org.konedrive.Error.");

/// Why the call never reached a running daemon. The bus answers
/// ServiceUnknown or NameHasNoOwner when nothing owns the name and nothing
/// can start it; it answers Spawn.* or TimedOut, or relays one of systemd's
/// own errors, when starting it on demand failed.
bool daemonNotRunning(const QString &name)
{
    return name == QLatin1String("org.freedesktop.DBus.Error.ServiceUnknown")
        || name == QLatin1String("org.freedesktop.DBus.Error.NameHasNoOwner")
        || name == QLatin1String("org.freedesktop.DBus.Error.TimedOut")
        || name.startsWith(QLatin1String("org.freedesktop.DBus.Error.Spawn."))
        || name.startsWith(QLatin1String("org.freedesktop.systemd1."));
}

/// The daemon left the bus while the call was waiting for it. The calls are
/// made with no timeout, so this is the only way to get NoReply.
bool daemonStopped(const QString &name)
{
    return name == QLatin1String("org.freedesktop.DBus.Error.NoReply");
}

/// Refusals with the same key are "the same reason".
QString reasonKey(const Failure &failure)
{
    if (daemonNotRunning(failure.errorName)) {
        return QStringLiteral("not-running");
    }
    if (daemonStopped(failure.errorName)) {
        return QStringLiteral("stopped");
    }
    if (failure.errorName == QLatin1String(AlreadyWaitingError) || failure.errorName == QLatin1String(TooManyWaitingError)) {
        return failure.errorName;
    }
    const QString refusal = failure.errorName.startsWith(KonedriveErrorPrefix) ? failure.errorName.mid(KonedriveErrorPrefix.size()) : QString();
    if (!refusal.isEmpty() && refusal != QLatin1String("Failed")) {
        return failure.errorName;
    }
    // Failed, or a name that is not ours: the message is all there is, and
    // two different messages are two different reasons.
    return failure.errorName + QLatin1Char('\n') + failure.message;
}
} // namespace

QString refusalText(Operation operation, const Failure &failure)
{
    const QString file = fileName(failure.path);
    const QString &name = failure.errorName;

    if (name == QLatin1String(AlreadyWaitingError)) {
        return alreadyWaitingSentence(file);
    }
    if (name == QLatin1String(TooManyWaitingError)) {
        return tooManyWaitingSentence(operation, file, SyncClient::MaxCallsInFlight);
    }
    if (daemonNotRunning(name)) {
        return notRunningSentence(operation, file);
    }
    if (daemonStopped(name)) {
        return stoppedSentence(operation, file);
    }
    // The name decides, never the message; the message is only what a
    // sentence has a place for.
    const QString sentence = refusalSentence(operation, name, file, failure.message);
    if (!sentence.isEmpty()) {
        return sentence;
    }
    // Failed, a name this plugin does not know, or an error from the bus
    // itself: the daemon's message is all there is, so it is kept whole.
    return failedSentence(operation, file, failure.message.isEmpty() ? name : failure.message);
}

QString failureSummary(Operation operation, const QList<Failure> &failures)
{
    QStringList order;
    QHash<QString, QList<Failure>> byReason;
    for (const Failure &failure : failures) {
        const QString key = reasonKey(failure);
        if (!byReason.contains(key)) {
            order.append(key);
        }
        byReason[key].append(failure);
    }

    QStringList paragraphs;
    for (const QString &key : std::as_const(order)) {
        const QList<Failure> &group = byReason[key];
        QString text = refusalText(operation, group.first());
        const int others = group.size() - 1;
        // Ours end with a full stop; the daemon's own message, shown whole
        // for Failed, usually does not.
        if (others > 0 && !text.endsWith(QLatin1Char('.')) && !text.endsWith(QLatin1Char('!')) && !text.endsWith(QLatin1Char('?'))
            && !text.endsWith(QChar(0x2026))) {
            text += QLatin1Char('.');
        }
        if (others > 0 && key == QLatin1String(AlreadyWaitingError)) {
            text += QLatin1Char(' ') + i18ncp("@info", "One more file was not asked again either.", "%1 more files were not asked again either.", others);
        } else if (others > 0) {
            text += QLatin1Char(' ')
                + (operation == Operation::FreeUpSpace
                       ? i18ncp("@info", "One more file was not freed up for the same reason.", "%1 more files were not freed up for the same reason.", others)
                       : i18ncp("@info",
                                "One more file was not %2 for the same reason.",
                                "%1 more files were not %2 for the same reason.",
                                others,
                                wasNotWords(operation)));
        }
        paragraphs.append(text);
    }
    return paragraphs.join(QLatin1Char('\n'));
}

} // namespace konedrive
