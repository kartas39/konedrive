#include "accountstatus.h"

#include "accountcontroller.h"
#include "daemoncontroller.h"
#include "synccontroller.h"
#include "transfermodel.h"

#include <KLocalizedString>

#include <QDateTime>
#include <QLocale>
#include <QRegularExpression>
#include <QTimer>

namespace
{
const QString Separator = QStringLiteral(". ");

/// What a `no-interception` root's LastError always starts with
/// (crates/konedrived/src/sync/mod.rs, NO_INTERCEPTION_WARNING), mirrored
/// here since the app has no other way to read a Rust constant. It carries
/// no ". " of its own, so stripping it (and the separator that follows when
/// there is trouble after it) leaves exactly the trouble, if any (I2).
const QString NoInterceptionWarning = QStringLiteral(
    "this folder is registered WITHOUT interception: nothing fills a placeholder when it is "
    "opened, so files in this folder read as zeros until they are explicitly hydrated");

/// Where, in LastError ("every current problem, joined with '. '"), the
/// failed-update note begins: "N file(s) changed in OneDrive could not be
/// updated here yet: <reason>" (dbus/org.konedrive.Folder.xml). The daemon
/// puts it last; -1 when there is none.
qsizetype updateNoteStart(const QString &lastError)
{
    const qsizetype key = lastError.indexOf(QLatin1String("could not be updated"), 0, Qt::CaseInsensitive);
    if (key < 0) {
        return -1;
    }
    const qsizetype separator = lastError.lastIndexOf(Separator, key);
    return separator < 0 ? 0 : separator + Separator.size();
}

/// The failed-update note, or empty.
QString updateNote(const QString &lastError)
{
    const qsizetype start = updateNoteStart(lastError);
    return start < 0 ? QString() : lastError.mid(start);
}

/// What else LastError says while the folder stays ready: trouble that does
/// not stop it (no network, no helper), without the failed-update note and
/// without the rescue note, which conflicts already stand for.
QString troubleIn(const QString &lastError)
{
    static const QRegularExpression rescueNote(QStringLiteral("\\d+ file\\(s\\) changed here were moved to .*? because the cloud changed or removed them"));
    const qsizetype noteStart = updateNoteStart(lastError);
    QString rest = noteStart < 0 ? lastError : lastError.left(noteStart);
    rest.remove(rescueNote);
    QStringList parts = rest.split(Separator, Qt::SkipEmptyParts);
    for (QString &part : parts) {
        part = part.trimmed();
    }
    parts.removeAll(QString());
    QString trouble = parts.join(Separator);
    if (!trouble.isEmpty()) {
        trouble[0] = trouble.at(0).toUpper();
    }
    return trouble;
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

/// A no-interception root's LastError with the fixed warning (and the
/// separator after it, if there is trouble) stripped, so `troubleIn` sees
/// only the trouble, exactly as it does for a `ready` root (I2).
QString withoutNoInterceptionWarning(QString lastError)
{
    if (!lastError.startsWith(NoInterceptionWarning)) {
        // An unexpected shape (a future daemon change): safer to show it all
        // as trouble than to silently drop it, which is the bug I2 fixes.
        return lastError;
    }
    lastError.remove(0, NoInterceptionWarning.size());
    if (lastError.startsWith(Separator)) {
        lastError.remove(0, Separator.size());
    }
    return lastError;
}
}

AccountStatus::AccountStatus(AccountController *account, SyncController *sync, DaemonController *daemon, Clock clock, QObject *parent)
    : QObject(parent)
    , m_account(account)
    , m_sync(sync)
    , m_daemon(daemon)
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
    connect(m_daemon, &DaemonController::changed, this, &AccountStatus::update);
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

    const QString rootState = m_sync->rootState();
    const int downloads = m_sync->transfers()->count();
    const qint64 checked = m_sync->lastChecked();
    const QString checkedText = checked > 0 ? i18nc("@info status, %1 is a time like '20 s ago'", "checked %1", ago(checked)) : QString();

    if (!m_account->serviceAvailable() || !m_sync->serviceAvailable()) {
        state = QStringLiteral("offline");
        text = i18n("The KOneDrive service is not running");
    } else if (m_account->state() == QLatin1String("signing-in")) {
        state = QStringLiteral("offline");
        text = i18n("Signing in…");
    } else if (m_account->state() != QLatin1String("signed-in")) {
        state = QStringLiteral("offline");
        text = i18n("Signed out of OneDrive");
    } else if (m_sync->rootPath().isEmpty() || rootState == QLatin1String("none")) {
        state = QStringLiteral("offline");
        text = i18n("No OneDrive folder yet");
    } else if (rootState == QLatin1String("error")) {
        state = QStringLiteral("warning");
        text = m_sync->lastError().isEmpty() ? i18n("Syncing has stopped") : m_sync->lastError();
    } else {
        const QString note = updateNote(m_sync->lastError());
        // ready's LastError is read as-is; no-interception's starts with a
        // fixed warning that is never trouble by itself (I2). Any other
        // state (listing) has nothing of its own to read here.
        QString trouble;
        if (rootState == QLatin1String("ready")) {
            trouble = troubleIn(m_sync->lastError());
        } else if (rootState == QLatin1String("no-interception")) {
            trouble = troubleIn(withoutNoInterceptionWarning(m_sync->lastError()));
        }
        // The helper serves every account: its trouble counts against each
        // account whose folder it intercepts (design §6.3) — not one
        // registered without interception, where nothing downloads on open
        // whatever the helper does.
        const bool helperTrouble = m_daemon->helperTrouble() && rootState != QLatin1String("no-interception");
        const int uploads = m_sync->uploads()->count();
        const uint pending = m_sync->pendingCount();
        const bool paused = m_sync->paused();
        if (rootState == QLatin1String("listing")) {
            text = i18n("Listing your OneDrive: %1 items so far", m_sync->itemsListed());
        } else if (!trouble.isEmpty()) {
            text = trouble;
        } else if (paused) {
            text = m_sync->pausedUntil() > 0 ? i18n("Paused until %1", until(m_sync->pausedUntil())) : i18n("Paused");
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
        if (rootState != QLatin1String("listing") && !checkedText.isEmpty()) {
            text = i18nc("@info status: what, then when it last checked", "%1 · %2", text, checkedText);
            ages = true;
        }

        if (m_sync->heldCount() > 0) {
            // The mass-delete guard: nothing more urgent, since only the user can decide.
            state = QStringLiteral("warning");
            attention = i18np("1 item deleted here waits for you: delete it in OneDrive too, or restore it",
                              "%1 items deleted here wait for you: delete them in OneDrive too, or restore them",
                              m_sync->heldCount());
        } else if (m_sync->conflictCount() > 0) {
            state = QStringLiteral("warning");
            attention = i18np("1 changed file was moved out of the way", "%1 changed files were moved out of the way", m_sync->conflictCount());
        } else if (m_sync->quotaFull()) {
            state = QStringLiteral("warning");
            attention = i18np("OneDrive is full: 1 file waits for space", "OneDrive is full: %1 files wait for space", m_sync->spaceWaitingCount());
        } else if (m_sync->tooBigCount() > 0) {
            state = QStringLiteral("warning");
            attention = i18np("1 file is too big for the space left in OneDrive", "%1 files are too big for the space left in OneDrive", m_sync->tooBigCount());
        } else if (m_sync->blockedCount() > 0) {
            state = QStringLiteral("warning");
            attention = i18np("1 change cannot be uploaded", "%1 changes cannot be uploaded", m_sync->blockedCount());
        } else if (!note.isEmpty()) {
            state = QStringLiteral("warning");
            attention = note;
        } else if (helperTrouble) {
            // The tray still needs warning, not offline: the folder itself
            // may be fine, only the helper (and so hydration on open) is not.
            state = QStringLiteral("warning");
            attention = i18n("The konedrive helper is not available: files are not kept in step, and nothing downloads when it is opened.");
        } else if (paused) {
            state = QStringLiteral("paused");
        } else if (!trouble.isEmpty()) {
            // M4: only "cannot reach OneDrive" looks offline; any other
            // trouble that does not stop the folder is a warning instead.
            state = trouble.startsWith(QLatin1String("cannot reach onedrive"), Qt::CaseInsensitive) ? QStringLiteral("offline") : QStringLiteral("warning");
        } else if (rootState == QLatin1String("listing") || downloads > 0 || uploads > 0 || pending > 0) {
            state = QStringLiteral("syncing");
        } else {
            state = QStringLiteral("ok");
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
