// Generated from the catalogue in crates/konedrive-text (src/reasons.rs, src/waits.rs) by src/cpp.rs.
// Do not edit: change the catalogue, then run `KONEDRIVE_UPDATE_GENERATED=1 cargo test -p konedrive-text`.
#include "reasontexts.h"

#include <KLocalizedString>

#include <QStringView>

namespace
{
/// A whole number as Rust's `u64` reads one: digits only, a `+` allowed before them.
bool wholeNumber(QStringView text, quint64 &value)
{
    if (text.startsWith(QLatin1Char('+'))) {
        text = text.mid(1);
    }
    if (text.isEmpty()) {
        return false;
    }
    for (const QChar c : text) {
        if (c < QLatin1Char('0') || c > QLatin1Char('9')) {
            return false;
        }
    }
    bool fits = false;
    value = text.toULongLong(&fits);
    return fits;
}

/// The sizes of `<needs>:<free>`.
bool sizesOf(QStringView detail, quint64 &needs, quint64 &free)
{
    const qsizetype colon = detail.indexOf(QLatin1Char(':'));
    return colon >= 0 && wholeNumber(detail.left(colon), needs) && wholeNumber(detail.mid(colon + 1), free);
}

/// Whether `: <detail>` behind this key is a detail: the keys that have a group.
bool takesDetail(QStringView key)
{
    static const QLatin1String keys[] = {
        QLatin1String("open-for-writing"),
        QLatin1String("name-characters"),
        QLatin1String("name-spaces"),
        QLatin1String("name-reserved"),
        QLatin1String("name-not-utf8"),
        QLatin1String("too-large"),
        QLatin1String("quota-exceeded"),
        QLatin1String("forbidden"),
        QLatin1String("refused"),
        QLatin1String("locked"),
        QLatin1String("not-found"),
        QLatin1String("changed-while-sending"),
        QLatin1String("parent-not-in-onedrive"),
        QLatin1String("hash-mismatch"),
        QLatin1String("paused"),
        QLatin1String("upload-session-open"),
        QLatin1String("name-held-by-an-upload"),
        QLatin1String("changed in OneDrive again and again"),
        QLatin1String("the upload session ended twice"),
        QLatin1String("not allowed now"),
        QLatin1String("blocked"),
        QLatin1String("not-read-on-disk"),
        QLatin1String("being-checked"),
        QLatin1String("quiet-time"),
        QLatin1String("network"),
        QLatin1String("local-error"),
        QLatin1String("index-error"),
        QLatin1String("upload-error"),
        QLatin1String("waiting-for-space"),
        QLatin1String("too-big"),
    };
    for (const QLatin1String known : keys) {
        if (key == known) {
            return true;
        }
    }
    return false;
}
}

ReasonParts reasonParts(const QString &stored)
{
    ReasonParts parts;
    const QLatin1String tooBig("too-big:");
    if (stored.startsWith(tooBig) && sizesOf(QStringView(stored).mid(tooBig.size()), parts.needs, parts.free)) {
        parts.key = QStringLiteral("too-big");
        parts.detail = stored.mid(tooBig.size());
        parts.hasDetail = true;
        parts.hasSizes = true;
        return parts;
    }
    const qsizetype at = stored.indexOf(QLatin1String(": "));
    if (at >= 0 && takesDetail(QStringView(stored).left(at))) {
        parts.key = stored.left(at);
        parts.detail = stored.mid(at + 2);
        parts.hasDetail = true;
        return parts;
    }
    parts.key = stored;
    return parts;
}

QString reasonSentence(const ReasonParts &parts, const QString &needs, const QString &free)
{
    if (parts.key == QLatin1String("open-for-writing")) {
        if (!parts.hasDetail) {
            return i18n("Open for writing in another program: it goes up once closed.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("name-characters")) {
        if (!parts.hasDetail) {
            return i18n("A name OneDrive refuses (it holds one of \" * : < > ? \\ |): rename it to upload it.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("name-spaces")) {
        if (!parts.hasDetail) {
            return i18n("A name that starts or ends with a space, which OneDrive refuses: rename it to upload it.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("name-reserved")) {
        if (!parts.hasDetail) {
            return i18n("A name OneDrive reserves: rename it to upload it.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("name-not-utf8")) {
        if (!parts.hasDetail) {
            return i18n("A name that is not valid UTF-8: rename it to upload it.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("too-large")) {
        if (!parts.hasDetail) {
            return i18n("Larger than OneDrive takes (250 GB).");
        }
        return QString();
    }
    if (parts.key == QLatin1String("quota-exceeded")) {
        if (!parts.hasDetail) {
            return i18n("OneDrive is full: free some space in OneDrive.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("forbidden")) {
        if (!parts.hasDetail) {
            return i18n("This sign-in does not allow uploads: sign in again.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("refused")) {
        if (!parts.hasDetail) {
            return i18n("Refused by OneDrive.");
        }
        if (parts.hasDetail) {
            return i18n("OneDrive refused it: %1", parts.detail);
        }
        return QString();
    }
    if (parts.key == QLatin1String("locked")) {
        if (!parts.hasDetail) {
            return i18n("Locked in OneDrive (open for co-authoring): tried again later.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("paused")) {
        if (!parts.hasDetail) {
            return i18n("Paused with the account: it goes on when the pause ends.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("upload-session-open")) {
        if (!parts.hasDetail) {
            return i18n("Its name in OneDrive is held by an upload of this folder that has not ended: tried again later.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("name-held-by-an-upload")) {
        if (!parts.hasDetail) {
            return i18n("Its name in OneDrive is held by an unfinished upload (another device, or one abandoned): tried again later.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("changed in OneDrive again and again")) {
        if (!parts.hasDetail) {
            return i18n("It keeps changing in OneDrive: tried again later.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("the upload session ended twice")) {
        if (!parts.hasDetail) {
            return i18n("OneDrive ended the upload twice: tried again later.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("not allowed now")) {
        if (!parts.hasDetail) {
            return i18n("Uploads are not allowed now: it goes on when they are.");
        }
        if (parts.hasDetail) {
            return i18n("Uploads are not allowed now: it goes on when they are (%1).", parts.detail);
        }
        return QString();
    }
    if (parts.key == QLatin1String("blocked")) {
        if (!parts.hasDetail) {
            return i18n("KOneDrive's record of this change is incomplete (%1): it stays here until the file is changed again.", parts.key);
        }
        return QString();
    }
    if (parts.key == QLatin1String("not-read-on-disk")) {
        if (!parts.hasDetail) {
            return i18n("KOneDrive could not read this on disk when it started: nothing of it is uploaded until it can be read at a start.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("being-checked")) {
        if (!parts.hasDetail) {
            return i18n("Being checked: KOneDrive is asking OneDrive for the whole folder again, and goes on by itself.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("quiet-time")) {
        if (!parts.hasDetail) {
            return i18n("Changed a moment ago: it goes up once it has been left alone for a moment.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("network")) {
        if (!parts.hasDetail) {
            return i18n("OneDrive could not be reached: tried again later.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("local-error")) {
        if (!parts.hasDetail) {
            return i18n("The local file could not be read: tried again later.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("index-error")) {
        if (!parts.hasDetail) {
            return i18n("KOneDrive's local index failed: tried again later.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("upload-error")) {
        if (!parts.hasDetail) {
            return i18n("The upload failed: tried again later.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("waiting-for-space")) {
        if (!parts.hasDetail) {
            return i18n("OneDrive is full: free up space in OneDrive, then Refresh.");
        }
        return QString();
    }
    if (parts.key == QLatin1String("too-big")) {
        if (!parts.hasDetail) {
            return i18n("Too big for the space left in OneDrive: free up space there, then Refresh.");
        }
        if (parts.hasSizes) {
            return i18n("Too big: needs %1, %2 free.", needs, free);
        }
        return QString();
    }
    Q_UNUSED(needs)
    Q_UNUSED(free)
    return QString();
}

QString stillHereSentence(const QString &waits)
{
    if (waits.isEmpty()) {
        return QString();
    }
    const qsizetype colon = waits.indexOf(QLatin1Char(':'));
    const QString key = colon < 0 ? waits : waits.left(colon);
    const QString detail = colon < 0 ? QString() : waits.mid(colon + 1);
    if (key == QLatin1String("uploads")) {
        quint64 count = 0;
        if (wholeNumber(detail, count)) {
            return i18np("Still on this computer: 1 change in it waits to be uploaded.", "Still on this computer: %1 changes in it wait to be uploaded.", count);
        }
    }
    if (key == QLatin1String("open-for-writing")) {
        return i18n("Still on this computer: %1 is open in a program.", detail);
    }
    return i18n("Still on this computer: it leaves once nothing in it waits to be uploaded.");
}
