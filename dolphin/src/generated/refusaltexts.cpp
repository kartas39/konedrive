// Generated from the catalogue in crates/konedrive-text (src/files.rs, src/menu.rs) by src/cpp.rs.
// Do not edit: change the catalogue, then run `KONEDRIVE_UPDATE_GENERATED=1 cargo test -p konedrive-text`.
#include "refusaltexts.h"

#include <KLocalizedString>

namespace konedrive
{

QString refusalSentence(Operation operation, const QString &errorName, const QString &file, const QString &detail)
{
    if (errorName == QLatin1String("org.konedrive.Error.InUse")) {
        return i18nc("@info", "“%1” is open in another program, so its space cannot be freed right now. Close it there and try again.", file);
    }
    if (errorName == QLatin1String("org.konedrive.Error.NoRoot")) {
        return i18nc("@info", "The folder holding “%1” is no longer registered with KOneDrive, so nothing was done with it.", file);
    }
    if (errorName == QLatin1String("org.konedrive.Error.NoHelper")) {
        switch (operation) {
        case Operation::AlwaysKeep:
            return i18nc("@info", "The konedrive helper is not connected, so nothing was changed. Keeping “%1” on this device must first have the helper stop letting its opens through unchecked — a download that failed partway would otherwise leave it reading as zeros. Try again once the helper is back (“konedrivectl sync status” shows when it is).", file);
        case Operation::Unpin:
            return i18nc("@info", "The konedrive helper is not connected, so nothing was changed. Unpinning “%1” needs the helper too, the same as any other change under KOneDrive's watch. Try again once KOneDrive is connected to the helper again — it reconnects on its own.", file);
        case Operation::FreeUpSpace:
            return i18nc("@info", "The konedrive helper is not connected, so nothing was changed. Freeing up “%1” must first have the helper take off any mark that lets the file's opens through unchecked, or the emptied file could read as zeros from then on. Try again once KOneDrive is connected to the helper again — it reconnects on its own.", file);
        case Operation::OpenOnline:
            break;
        }
        return QString();
    }
    if (errorName == QLatin1String("org.konedrive.Error.NotManaged")) {
        switch (operation) {
        case Operation::AlwaysKeep:
            return i18nc("@info", "“%1” is not a OneDrive file: it is a file of your own in the sync folder, so there is nothing for KOneDrive to keep downloaded.", file);
        case Operation::Unpin:
            return i18nc("@info", "“%1” is not a OneDrive file: it is a file of your own in the sync folder, so it was never pinned.", file);
        case Operation::FreeUpSpace:
            return i18nc("@info", "“%1” is not a OneDrive file: it is a file of your own in the sync folder, and KOneDrive never frees the space of a file it could not download again.", file);
        case Operation::OpenOnline:
            return i18nc("@info", "“%1” is not a OneDrive file: it is a file of your own in the sync folder, so it has no page in OneDrive.", file);
        }
        return QString();
    }
    if (errorName == QLatin1String("org.konedrive.Error.NotHydrated")) {
        return i18nc("@info", "“%1” is not downloaded, so there is no space to free — it already takes none.", file);
    }
    if (errorName == QLatin1String("org.konedrive.Error.ModifiedLocally")) {
        switch (operation) {
        case Operation::AlwaysKeep:
            return i18nc("@info", "“%1” was changed here and has not been uploaded, so downloading it again would overwrite your edits. It was left exactly as it is.", file);
        case Operation::Unpin:
            return i18nc("@info", "“%1” was changed here and has not been uploaded, so it was left exactly as it is.", file);
        case Operation::FreeUpSpace:
            return i18nc("@info", "“%1” was changed here and has not been uploaded, so freeing its space would lose your edits. It was left exactly as it is.", file);
        case Operation::OpenOnline:
            break;
        }
        return QString();
    }
    if (errorName == QLatin1String("org.konedrive.Error.OutsideRoot")) {
        switch (operation) {
        case Operation::AlwaysKeep:
        case Operation::Unpin:
        case Operation::FreeUpSpace:
            return i18nc("@info", "“%1” is not inside any of KOneDrive's folders, so nothing was done with it. Only files and folders inside them can be kept on this device or freed up.", file);
        case Operation::OpenOnline:
            return i18nc("@info", "“%1” is not inside any of KOneDrive's folders, so it has no page in OneDrive.", file);
        }
        return QString();
    }
    if (errorName == QLatin1String("org.konedrive.Error.NotSignedIn")) {
        switch (operation) {
        case Operation::AlwaysKeep:
        case Operation::Unpin:
        case Operation::FreeUpSpace:
            break;
        case Operation::OpenOnline:
            return i18nc("@info", "The account is not signed in, so OneDrive cannot be asked for the page of “%1”. Sign in and try again.", file);
        }
        return QString();
    }
    if (errorName == QLatin1String("org.konedrive.Error.NoSource")) {
        return i18nc("@info", "KOneDrive does not know where to download “%1” from yet. Run “konedrivectl sync populate-from” first; KOneDrive does not remember that directory across a restart, so run it again after one (files already there are left alone).", file);
    }
    if (errorName == QLatin1String("org.konedrive.Error.NotAllowed")) {
        switch (operation) {
        case Operation::AlwaysKeep:
        case Operation::Unpin:
        case Operation::OpenOnline:
            return i18nc("@info", "Could not change what is kept on this device: %1", detail);
        case Operation::FreeUpSpace:
            return i18nc("@info", "Could not free up space: %1", detail);
        }
        return QString();
    }
    if (errorName == QLatin1String("org.konedrive.Error.NotUploaded")) {
        switch (operation) {
        case Operation::AlwaysKeep:
        case Operation::Unpin:
            break;
        case Operation::FreeUpSpace:
            return i18nc("@info", "“%1” is not uploaded yet, so freeing it up would lose the changes made here. It was left exactly as it is.", file);
        case Operation::OpenOnline:
            return i18nc("@info", "“%1” is not uploaded yet, so it has no page in OneDrive.", file);
        }
        return QString();
    }
    if (errorName == QLatin1String("org.konedrive.Error.Unreachable")) {
        if (detail.isEmpty()) {
            return i18nc("@info", "OneDrive could not be reached.");
        }
        return i18nc("@info", "OneDrive could not be reached: %1", detail);
    }
    return QString();
}

QString failedSentence(Operation operation, const QString &file, const QString &detail)
{
    switch (operation) {
    case Operation::AlwaysKeep:
        return i18nc("@info", "Keeping “%1” on this device failed: %2", file, detail);
    case Operation::Unpin:
        return i18nc("@info", "Unpinning “%1” failed: %2", file, detail);
    case Operation::FreeUpSpace:
        return i18nc("@info", "Freeing up “%1” failed: %2", file, detail);
    case Operation::OpenOnline:
        return i18nc("@info", "Opening “%1” in OneDrive failed: %2", file, detail);
    }
    return detail;
}

QString wasNotWords(Operation operation)
{
    switch (operation) {
    case Operation::AlwaysKeep:
        return i18nc("@info how a file was not changed", "kept on this device");
    case Operation::Unpin:
        return i18nc("@info how a file was not changed", "unpinned");
    case Operation::FreeUpSpace:
        return i18nc("@info how a file was not changed", "freed up");
    case Operation::OpenOnline:
        return i18nc("@info how a file was not changed", "opened in OneDrive");
    }
    return QString();
}

QString notRunningSentence(Operation operation, const QString &file)
{
    return i18nc("@info", "KOneDrive is not running, so “%1” was not %2. Start it with “systemctl --user start konedrived” and try again.", file, wasNotWords(operation));
}

QString stoppedSentence(Operation operation, const QString &file)
{
    switch (operation) {
    case Operation::AlwaysKeep:
        return i18nc("@info", "KOneDrive stopped before it finished downloading everything of “%1”. Its emblem shows whether it is fully downloaded yet; if it is not, start KOneDrive again (“systemctl --user start konedrived”) and try again.", file);
    case Operation::Unpin:
        return i18nc("@info", "KOneDrive stopped before it finished unpinning “%1”. Its emblem shows whether it is still pinned; if it is, start KOneDrive again (“systemctl --user start konedrived”) and try again.", file);
    case Operation::FreeUpSpace:
        return i18nc("@info", "KOneDrive stopped before it finished freeing up “%1”. Its emblem shows whether it still takes space; if it does, start KOneDrive again (“systemctl --user start konedrived”) and try again.", file);
    case Operation::OpenOnline:
        return i18nc("@info", "KOneDrive stopped before it found the page of “%1” in OneDrive. Start KOneDrive again (“systemctl --user start konedrived”) and try again.", file);
    }
    return QString();
}

QString alreadyWaitingSentence(const QString &file)
{
    return i18nc("@info", "KOneDrive has not yet answered an earlier request for “%1”, so it was not asked again.", file);
}

QString tooManyWaitingSentence(Operation operation, const QString &file, int count)
{
    return i18nc("@info", "%1 requests to KOneDrive are already waiting for an answer, so “%2” was not %3. Try again once some of them have finished.", count, file, wasNotWords(operation));
}

QString freeUpWhyToolTip(MenuAnswer::FreeUpWhy why, const QString &blockedBy)
{
    switch (why) {
    case MenuAnswer::FreeUpWhy::PinnedAbove:
        return i18nc("@info:tooltip", "Kept on this device because “%1” is; unpin it first.", blockedBy);
    case MenuAnswer::FreeUpWhy::NoHelper:
        return i18nc("@info:tooltip", "The konedrive helper is not connected. Try again once it is — it reconnects on its own.");
    case MenuAnswer::FreeUpWhy::NotUploaded:
        return i18nc("@info:tooltip", "Not uploaded yet: freeing it up would lose the changes made here.");
    case MenuAnswer::FreeUpWhy::Unknown:
        return i18nc("@info:tooltip", "KOneDrive cannot tell yet whether a change here waits to be uploaded. Try again in a moment.");
    case MenuAnswer::FreeUpWhy::NotSaid:
        break;
    }
    return QString();
}

QString keptByAFolderToolTip(const QString &blockedBy)
{
    return i18nc("@info:tooltip", "Kept on this device because “%1” is.", blockedBy);
}

QString notInOneDriveToolTip()
{
    return i18nc("@info:tooltip", "Not in OneDrive yet.");
}

} // namespace konedrive
