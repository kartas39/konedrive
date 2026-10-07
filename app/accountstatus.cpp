#include "accountstatus.h"

#include "accountcontroller.h"
#include "synccontroller.h"
#include "transfermodel.h"

#include <KLocalizedString>

#include <QDateTime>
#include <QLocale>
#include <QTimer>

namespace
{
/// A state of Folder.Overall as the window shows it: the daemon's, or "offline" for one
/// this build does not know (none at all, from a daemon that has no such property).
QString shownState(const QString &state)
{
    static const QStringList known{QStringLiteral("ok"), QStringLiteral("syncing"), QStringLiteral("warning"), QStringLiteral("paused"), QStringLiteral("offline")};
    return known.contains(state) ? state : QStringLiteral("offline");
}

/// "40 s", "2 min", "1 h": how long something took.
QString durationText(qint64 seconds)
{
    if (seconds < 60) {
        return i18nc("@info duration", "%1 s", seconds);
    }
    if (seconds < 3600) {
        return i18nc("@info duration", "%1 min", seconds / 60);
    }
    return i18nc("@info duration", "%1 h", seconds / 3600);
}

/// Why a local scan runs (LocalScan.Reason), after "started 2 min ago, ".
QString scanReasonText(const QString &reason)
{
    if (reason == QLatin1String("start")) {
        return i18nc("@info why the local files are checked", "as syncing started");
    }
    if (reason == QLatin1String("read-write")) {
        return i18nc("@info why the local files are checked", "after the switch to read-write");
    }
    if (reason == QLatin1String("helper-back")) {
        return i18nc("@info why the local files are checked", "after the helper came back");
    }
    if (reason == QLatin1String("overflow")) {
        return i18nc("@info why the local files are checked", "after too many changes at once");
    }
    if (reason == QLatin1String("ignore-list")) {
        return i18nc("@info why the local files are checked", "after the ignore list changed");
    }
    if (reason == QLatin1String("periodic")) {
        return i18nc("@info why the local files are checked", "as part of the folder cannot be watched");
    }
    return reason;
}
}

AccountStatus::AccountStatus(AccountController *account, SyncController *sync, Clock clock, QObject *parent)
    : QObject(parent)
    , m_account(account)
    , m_sync(sync)
    , m_clock(clock ? std::move(clock) : Clock([] {
        return QDateTime::currentSecsSinceEpoch();
    }))
    , m_tick(new QTimer(this))
{
    m_tick->setInterval(TickMs);
    connect(m_tick, &QTimer::timeout, this, &AccountStatus::tick);
    connect(m_account, &AccountController::serviceAvailableChanged, this, &AccountStatus::update);
    connect(m_account, &AccountController::accountChanged, this, &AccountStatus::update);
    connect(m_sync, &SyncController::serviceAvailableChanged, this, &AccountStatus::update);
    connect(m_sync, &SyncController::syncChanged, this, &AccountStatus::update);
    connect(m_sync->transfers(), &TransferModel::countChanged, this, &AccountStatus::update);
    connect(m_sync->uploads(), &TransferModel::countChanged, this, &AccountStatus::update);
    update();
}

QString AccountStatus::iconName() const
{
    return iconFor(m_state);
}

QString AccountStatus::iconFor(const QString &state)
{
    if (state == QLatin1String("ok")) {
        return QStringLiteral("state-ok");
    }
    if (state == QLatin1String("syncing")) {
        return QStringLiteral("state-sync");
    }
    if (state == QLatin1String("warning")) {
        return QStringLiteral("state-warning");
    }
    if (state == QLatin1String("paused")) {
        return QStringLiteral("media-playback-pause");
    }
    return QStringLiteral("state-offline");
}

QString AccountStatus::heldBackText(const QString &reason)
{
    // The Status page's texts (StatusPage.qml, heldBackLine).
    if (reason == QLatin1String("metered")) {
        return i18n("Paused: metered connection");
    }
    if (reason == QLatin1String("on-battery")) {
        return i18n("Paused: on battery");
    }
    if (reason == QLatin1String("power-saver")) {
        return i18n("Paused: power-saver mode");
    }
    return i18n("Paused by itself");
}

QString AccountStatus::until(qint64 unixSeconds) const
{
    const QDateTime when = QDateTime::fromSecsSinceEpoch(unixSeconds);
    if (when.date() == QDateTime::fromSecsSinceEpoch(m_clock()).date()) {
        return QLocale().toString(when.time(), QLocale::ShortFormat);
    }
    return QLocale().toString(when, QLocale::ShortFormat);
}

QString AccountStatus::ago(qint64 unixSeconds) const
{
    const qint64 seconds = qMax<qint64>(0, m_clock() - unixSeconds);
    if (seconds < 10) {
        return i18nc("@info time", "just now");
    }
    if (seconds < 60) {
        return i18nc("@info time", "%1 s ago", seconds);
    }
    if (seconds < 3600) {
        return i18nc("@info time", "%1 min ago", seconds / 60);
    }
    if (seconds < 86400) {
        return i18nc("@info time", "%1 h ago", seconds / 3600);
    }
    return i18ncp("@info time", "%1 day ago", "%1 days ago", seconds / 86400);
}

void AccountStatus::tick()
{
    update();
}

void AccountStatus::update()
{
    QString state;
    QString text;
    QString attention;
    bool ages = false;

    const int downloads = m_sync->transfers()->count();
    const qint64 checked = m_sync->lastChecked();
    // While the notification socket is up, changes arrive as they happen: "live" says more
    // than when the last poll ran, and nothing on the line ages.
    const bool live = m_sync->liveChanges() == QLatin1String("connected");
    QString checkedText;
    if (live) {
        checkedText = i18nc("@info status: changes from OneDrive arrive as they happen", "live");
    } else if (checked > 0) {
        checkedText = i18nc("@info status, %1 is a time like '20 s ago'", "checked %1", ago(checked));
    }

    // The daemon decides the state and says why (Folder.Overall): the reason chooses the
    // words here, and nothing is worked out from the other properties or from a sentence.
    const QString reason = m_sync->overallReason();
    state = shownState(m_sync->overallState());
    if (!m_account->serviceAvailable() || !m_sync->serviceAvailable()) {
        // The one state that is the window's to say: no daemon can say it is not running.
        state = QStringLiteral("offline");
        text = i18n("The KOneDrive service is not running");
    } else if (reason == QLatin1String("signing-in")) {
        text = i18n("Signing in…");
    } else if (reason == QLatin1String("signed-out")) {
        text = i18n("Signed out of OneDrive");
    } else if (reason == QLatin1String("no-folder")) {
        text = i18n("No OneDrive folder yet");
    } else if (reason == QLatin1String("starting")) {
        // A folder that is recorded and not up yet, with nothing known to be wrong: it is
        // being brought up, or waits for the helper to connect (the start of a session).
        text = i18n("Starting…");
    } else if (reason == QLatin1String("stopped")) {
        // Folder.Trouble is then everything the folder stopped on, shown as it is.
        text = m_sync->trouble().isEmpty() ? i18n("Syncing has stopped") : m_sync->trouble();
    } else {
        // The line under the folder: what the account is doing, in words, from the counts
        // and the times. The trouble that does not stop the folder is said first, whatever
        // the reason is (deletes held, a pause).
        const bool listing = m_sync->rootState() == QLatin1String("listing");
        const int uploads = m_sync->uploads()->count();
        const uint pending = m_sync->pendingCount();
        const QString heldBack = m_sync->heldBack();
        if (listing) {
            text = i18n("Listing your OneDrive: %1 items so far", m_sync->itemsListed());
        } else if (!m_sync->trouble().isEmpty()) {
            text = m_sync->trouble();
            text[0] = text.at(0).toUpper();
        } else if (m_sync->paused()) {
            text = m_sync->pausedUntil() > 0 ? i18n("Paused until %1", until(m_sync->pausedUntil())) : i18n("Paused");
        } else if (!heldBack.isEmpty()) {
            text = heldBackText(heldBack);
        } else if (downloads > 0 && uploads > 0) {
            text = i18nc("@info status: downloading N files, uploading M files",
                         "Downloading %1, uploading %2",
                         i18np("1 file", "%1 files", downloads),
                         i18np("1 file", "%1 files", uploads));
        } else if (downloads > 0) {
            text = i18np("Downloading 1 file", "Downloading %1 files", downloads);
        } else if (uploads > 0) {
            text = i18np("Uploading 1 file", "Uploading %1 files", uploads);
        } else if (pending > 0) {
            text = i18np("1 change waiting to upload", "%1 changes waiting to upload", pending);
        } else {
            text = i18n("Up to date");
        }
        if (!listing && !checkedText.isEmpty()) {
            text = i18nc("@info status: what, then when it last checked", "%1 · %2", text, checkedText);
            ages = !live;
        }

        // Why the account needs attention, where the line does not say it: by the reason.
        if (reason == QLatin1String("deletes-held")) {
            attention = i18np("1 item deleted here waits for you: delete it in OneDrive too, or restore it",
                              "%1 items deleted here wait for you: delete them in OneDrive too, or restore them",
                              m_sync->heldCount());
        } else if (reason == QLatin1String("conflicts")) {
            attention = i18np("1 changed file was moved out of the way", "%1 changed files were moved out of the way", m_sync->conflictCount());
        } else if (reason == QLatin1String("quota-full")) {
            attention = i18np("OneDrive is full: 1 file waits for space", "OneDrive is full: %1 files wait for space", m_sync->spaceWaitingCount());
        } else if (reason == QLatin1String("too-big")) {
            attention = i18np("1 file is too big for the space left in OneDrive", "%1 files are too big for the space left in OneDrive", m_sync->tooBigCount());
        } else if (reason == QLatin1String("blocked")) {
            attention = i18np("1 change cannot be uploaded", "%1 changes cannot be uploaded", m_sync->blockedCount());
        } else if (reason == QLatin1String("not-updated")) {
            attention = m_sync->notUpdated();
        } else if (reason == QLatin1String("helper-unavailable")) {
            attention = i18n("The konedrive helper is not available: files are not kept in step, and nothing downloads when it is opened.");
        }
    }

    // The local scan: never a percentage, since only the base's count is known in advance.
    QString scanLine;
    const QString scanState = m_sync->scanState();
    if (scanState == QLatin1String("running")) {
        QString seen = i18nc("@info local scan: N folders and M files",
                             "%1 and %2",
                             i18ncp("@info local scan", "1 folder", "%1 folders", m_sync->scanDirectories()),
                             i18ncp("@info local scan", "1 file", "%1 files", m_sync->scanFiles()));
        if (m_sync->scanExpected() > 0) {
            seen = i18nc("@info local scan: what was seen, of about N", "%1, of about %2", seen, m_sync->scanExpected());
        }
        scanLine = i18nc("@info local scan: what was seen, when it started, why",
                         "Checking local files: %1 — started %2, %3",
                         seen,
                         ago(m_sync->scanStarted()),
                         scanReasonText(m_sync->scanReason()));
        ages = true;
    } else if (scanState == QLatin1String("idle")) {
        if (m_sync->scanFinished() > 0) {
            scanLine = i18nc("@info local scan: when, how long it took",
                             "Local files last checked %1 (took %2)",
                             ago(m_sync->scanFinished()),
                             durationText(m_sync->scanTook()));
            ages = true;
        } else {
            scanLine = i18n("Local files not checked yet");
        }
    }

    // The timer runs only while the text says how long ago.
    if (ages && !m_tick->isActive()) {
        m_tick->start();
    } else if (!ages && m_tick->isActive()) {
        m_tick->stop();
    }

    if (state == m_state && text == m_text && attention == m_attention && scanLine == m_scanLine) {
        return;
    }
    m_state = state;
    m_text = text;
    m_attention = attention;
    m_scanLine = scanLine;
    Q_EMIT changed();
}
