#include "uploadreasons.h"

#include <KLocalizedString>

#include <QLocale>
#include <QStringList>


QString uploadReasonText(const QString &reason)
{
    if (reason == QLatin1String("name-characters")) {
        return i18n("A name OneDrive refuses (it holds one of \" * : < > ? \\ |): rename it to upload it.");
    }
    if (reason == QLatin1String("name-spaces")) {
        return i18n("A name that starts or ends with a space, which OneDrive refuses: rename it to upload it.");
    }
    if (reason == QLatin1String("name-reserved")) {
        return i18n("A name OneDrive reserves: rename it to upload it.");
    }
    if (reason == QLatin1String("name-not-utf8")) {
        return i18n("A name that is not valid UTF-8: rename it to upload it.");
    }
    if (reason == QLatin1String("too-large")) {
        return i18n("Larger than OneDrive takes (250 GB).");
    }
    if (reason == QLatin1String("quota-exceeded")) {
        return i18n("OneDrive is full: free some space in OneDrive.");
    }
    if (reason == QLatin1String("forbidden")) {
        return i18n("This sign-in does not allow uploads: sign in again.");
    }
    if (reason == QLatin1String("open-for-writing")) {
        return i18n("Open for writing in another program: it goes up once closed.");
    }
    if (reason == QLatin1String("mass-delete")) {
        return i18n("Part of a large delete: delete it in OneDrive too, or restore it, on the Status page.");
    }
    if (reason == QLatin1String("symlink")) {
        return i18n("A symbolic link: never uploaded.");
    }
    if (reason == QLatin1String("fifo") || reason == QLatin1String("socket") || reason == QLatin1String("device")) {
        return i18n("Not a file or a folder: never uploaded.");
    }
    if (reason == QLatin1String("reserved-name")) {
        return i18n("A .konedrive- name, which KOneDrive keeps for itself: never uploaded.");
    }
    if (reason == QLatin1String("not-downloaded")) {
        return i18n("A file from another OneDrive folder that is not downloaded here.");
    }
    if (reason == QLatin1String("other-device")) {
        return i18n("On another filesystem mounted inside the folder: never uploaded.");
    }
    if (reason == QLatin1String("hard-link")) {
        return i18n("A file with other hard links: not uploaded.");
    }
    if (reason == QLatin1String("locked")) {
        return i18n("Locked in OneDrive (open for co-authoring): tried again later.");
    }
    if (reason == QLatin1String("refused")) {
        return i18n("Refused by OneDrive.");
    }
    if (reason.startsWith(QLatin1String("refused: "))) {
        return i18n("OneDrive refused it: %1", reason.mid(9));
    }
    if (reason == QLatin1String("waiting-for-space")) {
        return i18n("OneDrive is full: free up space in OneDrive, then Refresh.");
    }
    if (reason == QLatin1String("too-big")) {
        return i18n("Too big for the space left in OneDrive: free up space there, then Refresh.");
    }
    // too-big:<bytes needed>:<bytes free>
    if (reason.startsWith(QLatin1String("too-big:"))) {
        const QStringList parts = reason.mid(8).split(QLatin1Char(':'));
        if (parts.size() == 2) {
            const QLocale locale;
            return i18n("Too big: needs %1, %2 free.",
                        locale.formattedDataSize(parts.at(0).toLongLong()),
                        locale.formattedDataSize(parts.at(1).toLongLong()));
        }
    }
    return reason;
}
