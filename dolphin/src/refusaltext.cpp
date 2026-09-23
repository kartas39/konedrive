#include "refusaltext.h"

#include "filestate.h"
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
    const bool download = operation == Operation::Download;
    const QString file = fileName(failure.path);
    const QString &name = failure.errorName;

    if (name == QLatin1String(AlreadyWaitingError)) {
        return i18nc("@info", "KOneDrive has not yet answered an earlier request for “%1”, so it was not asked again.", file);
    }
    if (name == QLatin1String(TooManyWaitingError)) {
        return download ? i18nc("@info",
                                "%2 requests to KOneDrive are already waiting for an answer, so “%1” was not downloaded. "
                                "Try again once some of them have finished.",
                                file,
                                SyncClient::MaxCallsInFlight)
                        : i18nc("@info",
                                "%2 requests to KOneDrive are already waiting for an answer, so “%1” was not freed up. "
                                "Try again once some of them have finished.",
                                file,
                                SyncClient::MaxCallsInFlight);
    }
    if (daemonNotRunning(name)) {
        return download ? i18nc("@info",
                                 "KOneDrive is not running, so “%1” was not downloaded. "
                                 "Start it with “systemctl --user start konedrived” and try again.",
                                 file)
                        : i18nc("@info",
                                 "KOneDrive is not running, so the space of “%1” was not freed up. "
                                 "Start it with “systemctl --user start konedrived” and try again.",
                                 file);
    }
    if (daemonStopped(name)) {
        return download ? i18nc("@info",
                                 "KOneDrive stopped before it finished downloading “%1”. "
                                 "Its emblem shows whether it is downloaded; if it is not, start KOneDrive again "
                                 "(“systemctl --user start konedrived”) and try again.",
                                 file)
                        : i18nc("@info",
                                 "KOneDrive stopped before it finished freeing up “%1”. "
                                 "Its emblem shows whether it still takes space; if it does, start KOneDrive again "
                                 "(“systemctl --user start konedrived”) and try again.",
                                 file);
    }

    const QString refusal = name.startsWith(KonedriveErrorPrefix) ? name.mid(KonedriveErrorPrefix.size()) : QString();

    if (refusal == QLatin1String("NoHelper")) {
        return download ? i18nc("@info",
                                 "The konedrive helper is not connected, so nothing was changed. “%1” was "
                                 "left half freed up, or is marked downloaded with nothing to show it was, and downloading it "
                                 "again must first have the helper stop letting it through unchecked — a download that failed "
                                 "partway would otherwise leave it reading as zeros. Try again once the helper is back "
                                 "(“konedrivectl sync status” shows when it is).",
                                 file)
                        : i18nc("@info",
                                 "The konedrive helper is not connected, so nothing was changed. Freeing up "
                                 "“%1” must first have the helper take off any mark that lets the file's "
                                 "opens through unchecked, or the emptied file could read as zeros from then on. Try again once "
                                 "KOneDrive is connected to the helper again — it reconnects on its own.",
                                 file);
    }
    if (refusal == QLatin1String("NoRoot")) {
        return i18nc("@info",
                      "KOneDrive has no sync folder registered, so nothing was done with “%1”. Register "
                      "the folder first with “konedrivectl sync register” — or "
                      "“konedrivectl sync register-without-interception” on a machine without the helper.",
                      file);
    }
    if (refusal == QLatin1String("NoSource")) {
        return i18nc("@info",
                      "KOneDrive does not know where to download “%1” from yet. Run "
                      "“konedrivectl sync populate-from” first; KOneDrive does not remember that directory "
                      "across a restart, so run it again after one (files already there are left alone).",
                      file);
    }
    if (refusal == QLatin1String("OutsideRoot")) {
        return i18nc("@info",
                      "“%1” is not a regular file inside KOneDrive's sync folder. Only files inside it can "
                      "be downloaded or freed up — not folders, symbolic links, or anything outside it.",
                      file);
    }
    if (refusal == QLatin1String("NotManaged")) {
        return download ? i18nc("@info",
                                 "“%1” is not a OneDrive file: it is a file of your own in the sync folder, "
                                 "so there is nothing to download.",
                                 file)
                        : i18nc("@info",
                                 "“%1” is not a OneDrive file: it is a file of your own in the sync folder, "
                                 "and KOneDrive never frees the space of a file it could not download again.",
                                 file);
    }
    if (refusal == QLatin1String("NotHydrated")) {
        return i18nc("@info", "“%1” is not downloaded, so there is no space to free — it already takes none.", file);
    }
    if (refusal == QLatin1String("ModifiedLocally")) {
        return download ? i18nc("@info",
                                 "“%1” was changed here and has not been uploaded, so downloading it again "
                                 "would overwrite your edits. It was left exactly as it is.",
                                 file)
                        : i18nc("@info",
                                 "“%1” was changed here and has not been uploaded, so freeing its space would "
                                 "lose your edits. It was left exactly as it is.",
                                 file);
    }
    if (refusal == QLatin1String("InUse")) {
        return i18nc("@info",
                      "“%1” is open in another program, so its space cannot be freed right now. Close it "
                      "there and try again.",
                      file);
    }

    // Failed, a name this plugin does not know, or an error from the bus
    // itself: the daemon's message is all there is, so it is kept whole.
    const QString detail = failure.message.isEmpty() ? name : failure.message;
    return download ? i18nc("@info", "Downloading “%1” failed: %2", file, detail)
                    : i18nc("@info", "Freeing up “%1” failed: %2", file, detail);
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
                + (operation == Operation::Download
                       ? i18ncp("@info", "One more file was not downloaded for the same reason.", "%1 more files were not downloaded for the same reason.", others)
                       : i18ncp("@info", "One more file was not freed up for the same reason.", "%1 more files were not freed up for the same reason.", others));
        }
        paragraphs.append(text);
    }
    return paragraphs.join(QLatin1Char('\n'));
}

} // namespace konedrive
