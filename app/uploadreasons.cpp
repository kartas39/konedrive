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
    if (reason == QLatin1String("mounted-inside")) {
        return i18n("Another filesystem is mounted inside a folder no longer synced here: the folder stays until it is unmounted.");
    }
    if (reason == QLatin1String("leaving-not-found")) {
        return i18n("Not found in OneDrive, which still lists it, in a folder no longer synced here: kept until OneDrive's listing says it was removed, or it is changed again.");
    }
    if (reason == QLatin1String("unknown-state")) {
        return i18n("A file whose KOneDrive state cannot be read, in a folder no longer synced here: the folder stays until it is fixed or removed.");
    }
    if (reason == QLatin1String("hard-link")) {
        return i18n("A file with other hard links: not uploaded.");
    }
    if (reason == QLatin1String("locked")) {
        return i18n("Locked in OneDrive (open for co-authoring): tried again later.");
    }
    if (reason == QLatin1String("network")) {
        return i18n("OneDrive could not be reached: tried again later.");
    }
    if (reason == QLatin1String("local-error")) {
        return i18n("The local file could not be read: tried again later.");
    }
    if (reason == QLatin1String("index-error")) {
        return i18n("KOneDrive's local index failed: tried again later.");
    }
    if (reason == QLatin1String("upload-error")) {
        return i18n("The upload failed: tried again later.");
    }
    if (reason == QLatin1String("refused")) {
        return i18n("Refused by OneDrive.");
    }
    if (reason == QLatin1String("moved-out-not-opened")) {
        return i18n("Moved out of the folder before it was downloaded, and it cannot be opened for the download now: tried again later.");
    }
    if (reason.startsWith(QLatin1String("moved-out-not-opened: "))) {
        return i18n("Moved out of the folder before it was downloaded, and it cannot be opened for the download now: tried again later (%1).", reason.mid(22));
    }
    if (reason == QLatin1String("paused")) {
        return i18n("Paused with the account: it goes on when the pause ends.");
    }
    if (reason == QLatin1String("upload-session-open")) {
        return i18n("Its name in OneDrive is held by an upload of this folder that has not ended: tried again later.");
    }
    if (reason == QLatin1String("name-held-by-an-upload")) {
        return i18n("Its name in OneDrive is held by an unfinished upload (another device, or one abandoned): tried again later.");
    }
    if (reason == QLatin1String("changed in OneDrive again and again") || reason == QLatin1String("changing in OneDrive again and again")) {
        return i18n("It keeps changing in OneDrive: tried again later.");
    }
    if (reason == QLatin1String("the upload session ended twice")) {
        return i18n("OneDrive ended the upload twice: tried again later.");
    }
    if (reason == QLatin1String("not allowed now")) {
        return i18n("Uploads are not allowed now: it goes on when they are.");
    }
    if (reason.startsWith(QLatin1String("not allowed now: "))) {
        return i18n("Uploads are not allowed now: it goes on when they are (%1).", reason.mid(17));
    }
    if (reason == QLatin1String("state-unreadable")) {
        return i18n("The file's KOneDrive state cannot be read: it stays here until the file is replaced.");
    }
    if (reason.startsWith(QLatin1String("state-unreadable: "))) {
        return i18n("The file's KOneDrive state cannot be read: it stays here until the file is replaced (%1).", reason.mid(18));
    }
    for (const QLatin1String key : {QLatin1String("no-name"), QLatin1String("no-item"), QLatin1String("no-guard"), QLatin1String("no-handle"),
                                    QLatin1String("bad-handle"), QLatin1String("another-item"), QLatin1String("blocked")}) {
        if (reason == key) {
            return i18n("KOneDrive's record of this change is incomplete (%1): it stays here until the file is changed again.", reason);
        }
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
