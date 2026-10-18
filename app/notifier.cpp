#include "notifier.h"

#include "accountcontroller.h"
#include "activitymodel.h"
#include "uploadreasons.h"
#include "synccontroller.h"

#include <KIO/OpenFileManagerWindowJob>
#include <KLocalizedString>
#include <KNotification>
#include <KWindowSystem>

#include <QElapsedTimer>
#include <QFileInfo>
#include <QTimer>
#include <QUrl>

#include <limits>
#include <memory>

namespace
{
QString fileName(const QString &path)
{
    const QString name = QFileInfo(path).fileName();
    return name.isEmpty() ? path : name;
}

/// The title of a conflict that kept a copy beside the file.
QString copyTitle()
{
    return i18nc("@title notification", "Changed on both sides");
}
}

KNotificationSink::KNotificationSink(std::function<void(const QString &account)> openWindow)
    : m_openWindow(std::move(openWindow))
{
}

void KNotificationSink::send(const Notice &notice)
{
    auto *notification = new KNotification(notice.event);
    notification->setComponentName(QStringLiteral("konedrive"));
    notification->setTitle(notice.title);
    notification->setText(notice.text);
    if (!notice.showPath.isEmpty()) {
        const QString path = notice.showPath;
        KNotificationAction *show = notification->addAction(i18nc("@action:button", "Show in Folder"));
        QObject::connect(show, &KNotificationAction::activated, notification, [notification, path] {
            // The file manager's window gets the click's activation token.
            KIO::highlightInFileManager({QUrl::fromLocalFile(path)}, notification->xdgActivationToken().toUtf8());
        });
    }
    if (m_openWindow) {
        KNotificationAction *open = notification->addDefaultAction(i18nc("@action", "Open KOneDrive"));
        const std::function<void(const QString &)> openWindow = m_openWindow;
        const QString account = notice.account;
        QObject::connect(open, &KNotificationAction::activated, notification, [notification, openWindow, account] {
            // The click hands over the right to raise a window (xdg-activation on Wayland).
            if (const QString token = notification->xdgActivationToken(); !token.isEmpty()) {
                KWindowSystem::setCurrentXdgActivationToken(token);
            }
            openWindow(account);
        });
    }
    notification->sendEvent();
}

Notifier::Notifier(AccountController *account, SyncController *sync, NotificationSink *sink, Clock clock, QObject *parent)
    : QObject(parent)
    , m_account(account)
    , m_sync(sync)
    , m_sink(sink)
    , m_clock(std::move(clock))
    , m_timer(new QTimer(this))
    , m_signOutTimer(new QTimer(this))
    , m_accountState(account->state())
{
    m_signOutTimer->setSingleShot(true);
    connect(m_signOutTimer, &QTimer::timeout, this, &Notifier::announceSignOut);
    if (!m_clock) {
        auto elapsed = std::make_shared<QElapsedTimer>();
        elapsed->start();
        m_clock = [elapsed] {
            return elapsed->elapsed();
        };
    }
    m_timer->setSingleShot(true);
    connect(m_timer, &QTimer::timeout, this, &Notifier::flushDue);
    connect(m_sync, &SyncController::activityAdded, this, &Notifier::onActivity);
    connect(m_sync, &SyncController::syncChanged, this, &Notifier::onSyncChanged);
    connect(m_sync, &SyncController::serviceAvailableChanged, this, [this] {
        m_full = m_sync->serviceAvailable() ? int(m_sync->quotaFull()) : -1;
        m_waitRefusal = m_sync->serviceAvailable() ? m_sync->retryAfterRefusal() : QString();
    });
    if (m_sync->serviceAvailable()) {
        m_full = int(m_sync->quotaFull());
        m_waitRefusal = m_sync->retryAfterRefusal();
    }
    connect(m_account, &AccountController::accountChanged, this, &Notifier::onAccountChanged);
    connect(m_account, &AccountController::signOutRequested, this, [this] {
        m_signOutAsked = true;
    });
}

void Notifier::onActivity(qint64, const QString &kind, const QString &path, const QString &detail)
{
    const QString name = fileName(path);
    if (kind == QLatin1String("not-removed-here")) {
        // The path is the directory; rmdir itself could only say "not empty". Nothing to press.
        post({QStringLiteral("notRemovedHere"),
              i18nc("@title notification", "Folder not removed"),
              i18n("%1 cannot be removed here: it holds items that can't be shown in the folder. Remove it in OneDrive.", name),
              {}});
        return;
    }
    if (kind == QLatin1String("conflict")) {
        // A file changed on both sides: the daemon's detail is the copy kept beside it.
        post({QStringLiteral("conflict"), copyTitle(), i18n("%1 changed here and in OneDrive. Kept both versions: yours is %2.", name, fileName(detail)), detail});
        return;
    }
    switch (classifyFailure(kind, detail)) {
    case ActivityFailure::None:
        return;
    case ActivityFailure::DiskFull:
        post({QStringLiteral("diskFull"), i18nc("@title notification", "Not enough disk space"), i18n("%1 could not be downloaded: the disk is full.", name), {}});
        return;
    case ActivityFailure::Update:
        post({QStringLiteral("updateFailed"),
              i18nc("@title notification", "A file could not be updated"),
              i18n("%1 changed in OneDrive but could not be updated here: %2", name, detail),
              {}});
        return;
    case ActivityFailure::Upload:
        // Blocked names and a full OneDrive are the usual ones; the detail is a reason code.
        if (detail == QLatin1String("quota-exceeded")) {
            post({QStringLiteral("uploadFailed"),
                  i18nc("@title notification", "OneDrive is full"),
                  i18n("%1 can't upload until there is space in OneDrive.", name),
                  path});
            return;
        }
        post({QStringLiteral("uploadFailed"), i18nc("@title notification", "Can't upload"), i18n("%1 can't upload. %2", name, uploadReasonText(detail)), path});
        return;
    case ActivityFailure::Download:
        post({QStringLiteral("downloadFailed"),
              i18nc("@title notification", "Download failed"),
              detail.isEmpty() ? i18n("%1 could not be downloaded.", name) : i18n("%1 could not be downloaded: %2", name, detail),
              {}});
        return;
    }
}

void Notifier::onSyncChanged()
{
    if (m_full < 0) {
        // The daemon's first answer sets the baseline (serviceAvailableChanged).
        return;
    }
    // OneDrive turned full: said once for the account, never per file (issue #2).
    const int full = int(m_sync->quotaFull());
    if (m_full == 0 && full == 1) {
        post({QStringLiteral("uploadFailed"),
              i18nc("@title notification", "OneDrive is full"),
              i18n("Nothing more is uploaded until there is space in OneDrive. Free up space there, then choose Sync Now in KOneDrive."),
              {}});
    }
    m_full = full;

    // OneDrive asked the account to wait, and a program was refused a file meanwhile: said
    // once for the wait, with the first file refused, never per read.
    const QString refused = m_sync->retryAfterRefusal();
    if (m_waitRefusal.isEmpty() && !refused.isEmpty()) {
        const QString name = fileName(refused);
        const int minutes = int((m_sync->retryAfter() + 59) / 60);
        post({QStringLiteral("throttled"),
              i18nc("@title notification", "OneDrive asked to wait"),
              minutes > 0 ? i18np("OneDrive asked this device to wait about 1 minute. %2 opens after that.",
                                  "OneDrive asked this device to wait about %1 minutes. %2 opens after that.",
                                  minutes,
                                  name)
                          : i18n("OneDrive asked this device to wait. %1 opens after that.", name),
              {}});
    }
    m_waitRefusal = refused;
}

void Notifier::onAccountChanged()
{
    const QString previous = m_accountState;
    m_accountState = m_account->state();
    if (m_accountState != QLatin1String("signed-out") || previous != QLatin1String("signed-in")) {
        return;
    }
    if (m_signOutAsked) {
        m_signOutAsked = false;
        return;
    }
    // Accounts.Remove signs the account out, then unexports it and drops it
    // from Accounts: the row goes, and this Notifier with it, before the
    // notice is due. A sign-out that is news stays signed out until then.
    m_signOutTimer->start(m_signOutDelayMs);
}

void Notifier::setSignOutDelay(int ms)
{
    m_signOutDelayMs = ms;
}

void Notifier::announceSignOut()
{
    if (m_account->state() != QLatin1String("signed-out")) {
        return;
    }
    post({QStringLiteral("signedOut"), i18nc("@title notification", "Signed out of OneDrive"), i18n("Sign in again to keep your OneDrive folder up to date."), {}});
}

void Notifier::setAccountName(std::function<QString()> name)
{
    m_accountName = std::move(name);
}

void Notifier::post(const Notice &event)
{
    Notice notice = event;
    notice.account = m_account->path();
    if (const QString name = m_accountName ? m_accountName() : QString(); !name.isEmpty()) {
        notice.title = i18nc("@title notification: what happened — the account's name", "%1 — %2", event.title, name);
    }
    flushDue();
    const qint64 now = m_clock();
    auto it = m_windows.find(notice.event);
    if (it == m_windows.end()) {
        m_sink->send(notice);
        m_windows.insert(notice.event, Window{now + WindowMs, 0, notice});
        arm();
        return;
    }
    ++it->pending;
    it->last = notice;
}

Notice Notifier::summary(const Window &window) const
{
    const int n = window.pending;
    Notice notice = window.last;
    notice.count = n;
    if (notice.event == QLatin1String("diskFull")) {
        notice.text = i18np("1 more file could not be downloaded: the disk is full.", "%1 more files could not be downloaded: the disk is full.", n);
    } else if (notice.event == QLatin1String("downloadFailed")) {
        notice.text = i18np("1 more file could not be downloaded.", "%1 more files could not be downloaded.", n);
    } else if (notice.event == QLatin1String("updateFailed")) {
        notice.text = i18np("1 more file changed in OneDrive could not be updated here.", "%1 more files changed in OneDrive could not be updated here.", n);
    } else if (notice.event == QLatin1String("conflict")) {
        // "Show in Folder" stays on the last copy kept.
        notice.text = i18np("1 more file changed here and in OneDrive; kept both versions.", "%1 more files changed here and in OneDrive; kept both versions.", n);
    } else if (notice.event == QLatin1String("notRemovedHere")) {
        notice.text = i18np("1 more folder cannot be removed here.", "%1 more folders cannot be removed here.", n);
    } else if (notice.event == QLatin1String("throttled")) {
        notice.text = i18np("1 more file could not be opened: OneDrive asked to wait.", "%1 more files could not be opened: OneDrive asked to wait.", n);
    } else if (notice.event == QLatin1String("uploadFailed")) {
        notice.text = i18np("1 more change can't upload.", "%1 more changes can't upload.", n);
    }
    return notice;
}

void Notifier::flushDue()
{
    const qint64 now = m_clock();
    for (auto it = m_windows.begin(); it != m_windows.end();) {
        if (it->endsAt > now) {
            ++it;
            continue;
        }
        if (it->pending == 0) {
            it = m_windows.erase(it);
            continue;
        }
        m_sink->send(summary(*it));
        it->pending = 0;
        it->endsAt = now + WindowMs;
        ++it;
    }
    arm();
}

void Notifier::arm()
{
    if (m_windows.isEmpty()) {
        m_timer->stop();
        return;
    }
    qint64 next = std::numeric_limits<qint64>::max();
    for (const Window &window : std::as_const(m_windows)) {
        next = qMin(next, window.endsAt);
    }
    m_timer->start(int(qBound<qint64>(0, next - m_clock(), WindowMs)));
}
