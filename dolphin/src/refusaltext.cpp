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

/// "…, so “file” was not %1." -- the same shape for every generic refusal.
QString wasNot(Operation operation)
{
    switch (operation) {
    case Operation::AlwaysKeep:
        return i18nc("@info how a file was not changed", "kept on this device");
    case Operation::Unpin:
        return i18nc("@info how a file was not changed", "unpinned");
    case Operation::FreeUpSpace:
        return i18nc("@info how a file was not changed", "freed up");
    }
    return {};
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
        return i18nc("@info", "KOneDrive has not yet answered an earlier request for “%1”, so it was not asked again.", file);
    }
    if (name == QLatin1String(TooManyWaitingError)) {
        return i18nc("@info",
                      "%2 requests to KOneDrive are already waiting for an answer, so “%1” was not %3. "
                      "Try again once some of them have finished.",
                      file,
                      SyncClient::MaxCallsInFlight,
                      wasNot(operation));
    }
    if (daemonNotRunning(name)) {
        return i18nc("@info",
                      "KOneDrive is not running, so “%1” was not %2. "
                      "Start it with “systemctl --user start konedrived” and try again.",
                      file,
                      wasNot(operation));
    }
    if (daemonStopped(name)) {
        switch (operation) {
        case Operation::AlwaysKeep:
            return i18nc("@info",
                          "KOneDrive stopped before it finished downloading everything of “%1”. "
                          "Its emblem shows whether it is fully downloaded yet; if it is not, start KOneDrive again "
                          "(“systemctl --user start konedrived”) and try again.",
                          file);
        case Operation::Unpin:
            return i18nc("@info",
                          "KOneDrive stopped before it finished unpinning “%1”. "
                          "Its emblem shows whether it is still pinned; if it is, start KOneDrive again "
                          "(“systemctl --user start konedrived”) and try again.",
                          file);
        case Operation::FreeUpSpace:
            return i18nc("@info",
                          "KOneDrive stopped before it finished freeing up “%1”. "
                          "Its emblem shows whether it still takes space; if it does, start KOneDrive again "
                          "(“systemctl --user start konedrived”) and try again.",
                          file);
        }
    }

    const QString refusal = name.startsWith(KonedriveErrorPrefix) ? name.mid(KonedriveErrorPrefix.size()) : QString();

    if (refusal == QLatin1String("NoHelper")) {
        switch (operation) {
        case Operation::AlwaysKeep:
            return i18nc("@info",
                          "The konedrive helper is not connected, so nothing was changed. Keeping “%1” "
                          "on this device must first have the helper stop letting its opens through unchecked — a "
                          "download that failed partway would otherwise leave it reading as zeros. Try again once "
                          "the helper is back (“konedrivectl sync status” shows when it is).",
                          file);
        case Operation::Unpin:
            return i18nc("@info",
                          "The konedrive helper is not connected, so nothing was changed. Unpinning “%1” needs "
                          "the helper too, the same as any other change under KOneDrive's watch. Try again once "
                          "KOneDrive is connected to the helper again — it reconnects on its own.",
                          file);
        case Operation::FreeUpSpace:
            return i18nc("@info",
                          "The konedrive helper is not connected, so nothing was changed. Freeing up "
                          "“%1” must first have the helper take off any mark that lets the file's "
                          "opens through unchecked, or the emptied file could read as zeros from then on. Try again once "
                          "KOneDrive is connected to the helper again — it reconnects on its own.",
                          file);
        }
    }
    if (refusal == QLatin1String("NoRoot")) {
        return i18nc("@info",
                      "The folder holding “%1” is no longer registered with KOneDrive, so nothing was "
                      "done with it.",
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
                      "“%1” is not inside any of KOneDrive's folders, so nothing was done with it. Only files "
                      "and folders inside them can be kept on this device or freed up.",
                      file);
    }
    if (refusal == QLatin1String("NotManaged")) {
        switch (operation) {
        case Operation::AlwaysKeep:
            return i18nc("@info",
                          "“%1” is not a OneDrive file: it is a file of your own in the sync folder, "
                          "so there is nothing for KOneDrive to keep downloaded.",
                          file);
        case Operation::Unpin:
            return i18nc("@info",
                          "“%1” is not a OneDrive file: it is a file of your own in the sync folder, "
                          "so it was never pinned.",
                          file);
        case Operation::FreeUpSpace:
            return i18nc("@info",
                          "“%1” is not a OneDrive file: it is a file of your own in the sync folder, "
                          "and KOneDrive never frees the space of a file it could not download again.",
                          file);
        }
    }
    if (refusal == QLatin1String("NotHydrated")) {
        return i18nc("@info", "“%1” is not downloaded, so there is no space to free — it already takes none.", file);
    }
    if (refusal == QLatin1String("ModifiedLocally")) {
        switch (operation) {
        case Operation::AlwaysKeep:
            return i18nc("@info",
                          "“%1” was changed here and has not been uploaded, so downloading it again "
                          "would overwrite your edits. It was left exactly as it is.",
                          file);
        case Operation::Unpin:
            return i18nc("@info", "“%1” was changed here and has not been uploaded, so it was left exactly as it is.", file);
        case Operation::FreeUpSpace:
            return i18nc("@info",
                          "“%1” was changed here and has not been uploaded, so freeing its space would "
                          "lose your edits. It was left exactly as it is.",
                          file);
        }
    }
    // Free up refused for a file with changes waiting to be uploaded (write
    // design §9). The name is matched ahead of the daemon's side, which W5b adds.
    if (refusal == QLatin1String("NotUploaded") && operation == Operation::FreeUpSpace) {
        return i18nc("@info",
                      "“%1” is not uploaded yet, so freeing it up would lose the changes made here. "
                      "It was left exactly as it is.",
                      file);
    }
    if (refusal == QLatin1String("InUse")) {
        return i18nc("@info",
                      "“%1” is open in another program, so its space cannot be freed right now. Close it "
                      "there and try again.",
                      file);
    }
    if (refusal == QLatin1String("NotAllowed")) {
        // The daemon refuses the whole call if any path it was given is
        // pinned by an ancestor, and its own message already names that
        // path and the folder that pins it first ("<path> is pinned by
        // <folder>: unpin it first", pinning.md §7) -- so it
        // is shown as is, with no file name of ours put in front of it:
        // `failure.path` is whichever path in the batch this Failure
        // happens to be for, not necessarily the one the daemon meant
        // (review #18).
        return operation == Operation::FreeUpSpace ? i18nc("@info", "Could not free up space: %1", failure.message)
                                                    : i18nc("@info", "Could not change what is kept on this device: %1", failure.message);
    }

    // Failed, a name this plugin does not know, or an error from the bus
    // itself: the daemon's message is all there is, so it is kept whole.
    const QString detail = failure.message.isEmpty() ? name : failure.message;
    switch (operation) {
    case Operation::AlwaysKeep:
        return i18nc("@info", "Keeping “%1” on this device failed: %2", file, detail);
    case Operation::Unpin:
        return i18nc("@info", "Unpinning “%1” failed: %2", file, detail);
    case Operation::FreeUpSpace:
        return i18nc("@info", "Freeing up “%1” failed: %2", file, detail);
    }
    return detail;
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
                                wasNot(operation)));
        }
        paragraphs.append(text);
    }
    return paragraphs.join(QLatin1Char('\n'));
}

} // namespace konedrive
