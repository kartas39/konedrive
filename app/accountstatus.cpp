#include "accountstatus.h"

#include "accountcontroller.h"
#include "synccontroller.h"
#include "timetext.h"
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

/// A sentence of the daemon's, which starts in lower case, as a sentence of its own.
QString sentence(QString text)
{
    if (!text.isEmpty()) {
        text[0] = text.at(0).toUpper();
    }
    return text;
}

/// Why a local scan runs (LocalScan.Reason), after "started 2 min ago, ".
QString scanReasonText(const QString &reason)
{
    if (reason == QLatin1String("start")) {
        return i18nc("@info why the local files are checked", "as syncing started");
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
    return timeText(unixSeconds, m_clock());
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
    QString headline;
    QString detail;
    Action action = Action::None;
    QString troubleLine;
    QString issuesLine;
    bool ages = false;

    const int downloads = m_sync->transfers()->count();
    const int uploads = m_sync->uploads()->count();
    const uint pending = m_sync->pendingCount();
    const qint64 checked = m_sync->lastChecked();
    // While the notification socket is up, changes arrive as they happen: "live" says more
    // than when the last poll ran, and nothing on the line ages.
    const bool live = m_sync->liveChanges() == QLatin1String("connected");
    // How fresh the folder is: after a line ("checked 20 s ago"), and as a sentence of its own.
    QString checkedText;
    QString freshness;
    if (live) {
        checkedText = i18nc("@info status: changes from OneDrive arrive as they happen", "live");
        freshness = i18nc("@info status: changes from OneDrive arrive as they happen", "Changes from OneDrive arrive as they happen");
    } else if (checked > 0) {
        checkedText = i18nc("@info status, %1 is a time like '20 s ago'", "checked %1", ago(checked));
        freshness = i18nc("@info status, %1 is a time like '20 s ago'", "Checked %1", ago(checked));
    }
    const QString trouble = sentence(m_sync->trouble());

    // The daemon decides the state and says why (Folder.Overall): the reason chooses the
    // words here, and nothing is worked out from the other properties or from a sentence.
    const QString reason = m_sync->overallReason();
    state = shownState(m_sync->overallState());
    if (!m_account->serviceAvailable() || !m_sync->serviceAvailable()) {
        // The one state that is the window's to say: no daemon can say it is not running.
        state = QStringLiteral("offline");
        text = headline = i18n("The KOneDrive service is not running");
        detail = i18n("Until it runs, the folder shows only the files already downloaded, and nothing syncs.");
        action = Action::StartService;
    } else if (reason == QLatin1String("signing-in")) {
        text = headline = i18n("Signing in…");
        detail = i18n("Finish signing in in your browser.");
        action = Action::CancelSignIn;
    } else if (reason == QLatin1String("signed-out")) {
        text = headline = i18n("Signed out of OneDrive");
        detail = i18n("Sign in again to go on syncing. The files already on this device stay.");
        action = Action::SignIn;
    } else if (reason == QLatin1String("no-folder")) {
        text = i18n("No OneDrive folder yet");
        headline = i18n("Choose where your OneDrive appears");
        detail = i18n("It shows up in an empty folder, and files download when you open them.");
        action = Action::ChooseFolder;
    } else if (reason == QLatin1String("starting")) {
        // A folder that is recorded and not up yet, with nothing known to be wrong: it is
        // being brought up (the start of a session).
        text = headline = i18n("Starting…");
    } else if (reason == QLatin1String("stopped")) {
        // Folder.Trouble is then everything the folder stopped on, shown as it is.
        text = m_sync->trouble().isEmpty() ? i18n("Syncing has stopped") : m_sync->trouble();
        headline = i18n("Syncing stopped");
        detail = trouble;
        action = Action::TryAgain;
    } else {
        // The first listing of the drive runs: the daemon says so as the reason (Folder.State has no such word).
        const bool listing = reason == QLatin1String("listing");
        // OneDrive asked the account to wait (a Retry-After): nothing is listed or moved
        // until then, and Transfers.RetryAfter says the seconds left.
        const bool throttled = reason == QLatin1String("throttled");
        const QString heldBack = m_sync->heldBack();
        // What the folder is doing, in words, from the times: the head's, with no count.
        // The tray's line counts what moves, since nothing else there does.
        QString doing;
        QString counted;
        if (m_sync->paused()) {
            doing = counted = m_sync->pausedUntil() > 0 ? i18n("Paused until %1", until(m_sync->pausedUntil())) : i18n("Paused until you resume");
        } else if (!heldBack.isEmpty()) {
            doing = counted = heldBackText(heldBack);
        } else if (downloads + uploads > 0) {
            doing = i18n("Syncing");
            counted = i18np("Syncing 1 file", "Syncing %1 files", downloads + uploads);
        } else if (pending > 0) {
            doing = i18n("Changes will upload shortly");
            counted = i18np("1 change will upload shortly", "%1 changes will upload shortly", pending);
        } else {
            doing = counted = i18n("Up to date");
        }
        const QString doingChecked = checkedText.isEmpty() ? doing : i18nc("@info status: what, then when it last checked", "%1 · %2", doing, checkedText);
        const QString countedChecked = checkedText.isEmpty() ? counted : i18nc("@info status: what, then when it last checked", "%1 · %2", counted, checkedText);

        // The line of the tray: the trouble that does not stop the folder is said first,
        // whatever the reason is (a pause).
        if (throttled) {
            text = m_sync->retryAfter() > 0 ? i18n("OneDrive asked to wait: %1 left", durationText(m_sync->retryAfter())) : i18n("OneDrive asked to wait");
        } else if (listing) {
            text = i18n("Getting your OneDrive ready: %1 items so far", m_sync->itemsListed());
        } else if (!trouble.isEmpty()) {
            text = checkedText.isEmpty() ? trouble : i18nc("@info status: what, then when it last checked", "%1 · %2", trouble, checkedText);
        } else {
            text = countedChecked;
        }
        if (!listing && !throttled && !checkedText.isEmpty()) {
            ages = !live;
        }

        // The head of Status, by the reason.
        if (throttled) {
            headline = i18n("OneDrive asked to wait");
            detail = m_sync->retryAfter() > 0 ? i18n("%1 left", durationText(m_sync->retryAfter())) : QString();
        } else if (listing) {
            headline = i18n("Getting your OneDrive ready");
            detail = i18np("1 item so far", "%1 items so far", m_sync->itemsListed());
        } else if (reason == QLatin1String("unreachable")) {
            headline = i18n("Can't reach OneDrive");
            detail = trouble.isEmpty() ? freshness : trouble;
        } else if (reason == QLatin1String("conflicts") || reason == QLatin1String("quota-full") || reason == QLatin1String("too-big")
                   || reason == QLatin1String("blocked") || reason == QLatin1String("not-updated") || reason == QLatin1String("trouble")) {
            // What needs the user is the line under the head, or a sentence of its own there;
            // the button is what ends a pause, which the folder may be in too.
            headline = i18n("Needs your attention");
            detail = doingChecked;
            if (m_sync->paused()) {
                action = Action::Resume;
            } else if (!heldBack.isEmpty()) {
                action = Action::SyncAnyway;
            }
        } else if (m_sync->paused()) {
            headline = doing;
            detail = i18n("Nothing syncs in the background. Files still download when you open them, and changes made here wait.");
            action = Action::Resume;
        } else if (!heldBack.isEmpty()) {
            headline = doing;
            detail = i18n("Nothing syncs in the background, as set in Settings. Files still download when you open them.");
            action = Action::SyncAnyway;
        } else {
            headline = doing;
            detail = freshness;
        }
        // Unreachable says its trouble in the sentence under the headline already.
        if (reason != QLatin1String("unreachable")) {
            troubleLine = trouble;
        }
        // Changes made here are not uploaded: the account is read-only, or two-way with a
        // folder that runs read-only for now (Folder.Writable).
        QString readOnly;
        if (m_account->mode() == QLatin1String("read-only")) {
            readOnly = i18nc("@info status: the folder's mode", "Read-only");
        } else if (!m_sync->writable()) {
            readOnly = i18nc("@info status: the folder's mode", "Read-only for now");
        }
        if (!readOnly.isEmpty()) {
            detail = detail.isEmpty() ? readOnly : i18nc("@info status: the folder's mode, then the sentence under the head", "%1 · %2", readOnly, detail);
        }

        // Why the account needs attention, where the line does not say it: by the reason.
        if (reason == QLatin1String("conflicts")) {
            attention = i18np("Kept both versions of 1 file changed here and in OneDrive", "Kept both versions of %1 files changed here and in OneDrive", m_sync->conflictCount());
        } else if (reason == QLatin1String("quota-full")) {
            attention = i18np("OneDrive is full: 1 file waits for space", "OneDrive is full: %1 files wait for space", m_sync->spaceWaitingCount());
        } else if (reason == QLatin1String("too-big")) {
            attention = i18np("1 file is too big for the space left in OneDrive", "%1 files are too big for the space left in OneDrive", m_sync->tooBigCount());
        } else if (reason == QLatin1String("blocked")) {
            attention = i18np("1 change can't upload", "%1 changes can't upload", m_sync->blockedCount());
        } else if (reason == QLatin1String("not-updated")) {
            attention = m_sync->notUpdated();
        }
    }

    // What Issues lists that needs the user, for a folder there is.
    if (m_account->serviceAvailable() && m_sync->serviceAvailable() && !m_sync->rootPath().isEmpty()) {
        QStringList issues;
        if (m_sync->quotaFull()) {
            issues << i18np("OneDrive is full: 1 file waits for space", "OneDrive is full: %1 files wait for space", m_sync->spaceWaitingCount());
        }
        if (m_sync->tooBigCount() > 0) {
            issues << i18np("1 file is too big for the space left in OneDrive", "%1 files are too big for the space left in OneDrive", m_sync->tooBigCount());
        }
        if (m_sync->blockedCount() > 0) {
            issues << i18np("1 change can't upload", "%1 changes can't upload", m_sync->blockedCount());
        }
        if (m_sync->conflictCount() > 0) {
            issues << i18np("Kept both versions of 1 file", "Kept both versions of %1 files", m_sync->conflictCount());
        }
        issuesLine = issues.join(QStringLiteral(" · "));
    }

    // The check of the folder on disk at the start of its sync, while it runs. Once it ended
    // there is nothing to say: every change made in the folder goes through its mount, which
    // the daemon sees as it is made.
    QString scanLine;
    if (m_sync->scanState() == QLatin1String("running")) {
        scanLine = i18nc("@info local scan: when it started, why",
                         "Checking local files — started %1, %2",
                         ago(m_sync->scanStarted()),
                         scanReasonText(m_sync->scanReason()));
        ages = true;
    }

    // The timer runs only while the text says how long ago.
    if (ages && !m_tick->isActive()) {
        m_tick->start();
    } else if (!ages && m_tick->isActive()) {
        m_tick->stop();
    }

    if (state == m_state && text == m_text && attention == m_attention && scanLine == m_scanLine && headline == m_headline && detail == m_detail
        && action == m_action && troubleLine == m_troubleLine && issuesLine == m_issuesLine) {
        return;
    }
    m_state = state;
    m_text = text;
    m_attention = attention;
    m_scanLine = scanLine;
    m_headline = headline;
    m_detail = detail;
    m_action = action;
    m_troubleLine = troubleLine;
    m_issuesLine = issuesLine;
    Q_EMIT changed();
}
