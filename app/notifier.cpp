#include "notifier.h"

#include "accountcontroller.h"
#include "activitymodel.h"
#include "synccontroller.h"

#include <KIO/OpenFileManagerWindowJob>
#include <KLocalizedString>
#include <KNotification>
#include <KWindowSystem>

#include <QElapsedTimer>
#include <QFileInfo>
#include <QRegularExpression>
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

/// A capped cycle's own summary event (activity::capped,
/// crates/konedrived/src/sync/listing.rs ~901): "and N more", nothing else.
const QRegularExpression cappedMore(QStringLiteral("^and (\\d+) more$"));
}

KNotificationSink::KNotificationSink(std::function<void()> openWindow)
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
        const std::function<void()> openWindow = m_openWindow;
        QObject::connect(open, &KNotificationAction::activated, notification, [notification, openWindow] {
            // The click hands over the right to raise a window (xdg-activation on Wayland).
            if (const QString token = notification->xdgActivationToken(); !token.isEmpty()) {
                KWindowSystem::setCurrentXdgActivationToken(token);
            }
            openWindow();
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
    , m_accountState(account->state())
{
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
    connect(m_account, &AccountController::accountChanged, this, &Notifier::onAccountChanged);
    connect(m_account, &AccountController::signOutRequested, this, [this] {
        m_signOutAsked = true;
    });
}

void Notifier::onActivity(qint64, const QString &kind, const QString &path, const QString &detail)
{
    const QString name = fileName(path);
    if (kind == QLatin1String("conflict")) {
        // M3: past 50 conflicts in one cycle, the daemon adds one more
        // `conflict` event whose path is the root and whose detail is
        // "and N more" — a count, not a rescued path. "Show in Folder" has
        // no single file to point at, so this one has none.
        if (path == m_sync->rootPath()) {
            if (const auto match = cappedMore.match(detail); match.hasMatch()) {
                post({QStringLiteral("conflict"), i18nc("@title notification", "Your changed version was moved"), QString(), {}}, match.captured(1).toInt());
                return;
            }
        }
        // The daemon's detail is where the local version went.
        post({QStringLiteral("conflict"),
              i18nc("@title notification", "Your changed version was moved"),
              i18n("Your changed version of %1 was moved to %2", name, detail),
              detail});
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
    case ActivityFailure::Download:
        post({QStringLiteral("downloadFailed"),
              i18nc("@title notification", "Download failed"),
              detail.isEmpty() ? i18n("%1 could not be downloaded.", name) : i18n("%1 could not be downloaded: %2", name, detail),
              {}});
        return;
    }
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
    post({QStringLiteral("signedOut"), i18nc("@title notification", "Signed out of OneDrive"), i18n("Sign in again to keep your OneDrive folder up to date."), {}});
}

void Notifier::post(const Notice &notice, int count)
{
    flushDue();
    const qint64 now = m_clock();
    auto it = m_windows.find(notice.event);
    if (it == m_windows.end()) {
        Window window{now + WindowMs, 0, notice};
        if (count <= 1) {
            m_sink->send(notice);
        } else {
            // A capped cycle's "and N more": nothing to count up from, so
            // the window opens already having said all of it.
            window.pending = count;
            m_sink->send(summary(window));
            window.pending = 0;
        }
        m_windows.insert(notice.event, window);
        arm();
        return;
    }
    it->pending += count;
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
        // "Show in Folder" stays on the last one moved.
        notice.text = i18np("1 more of your changed files was moved out of the way.", "%1 more of your changed files were moved out of the way.", n);
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
