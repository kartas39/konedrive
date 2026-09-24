#include "trayicon.h"

#include "appstatus.h"
#include "synccontroller.h"

#include <KIO/OpenUrlJob>
#include <KLocalizedString>
#include <KStatusNotifierItem>
#include <KWindowSystem>

#include <QAction>
#include <QDBusConnection>
#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusServiceWatcher>
#include <QDBusVariant>
#include <QEvent>
#include <QMenu>
#include <QUrl>
#include <QWindow>

Q_LOGGING_CATEGORY(KONEDRIVE_APP, "konedrive.app", QtInfoMsg)

namespace
{
const QString WatcherService = QStringLiteral("org.kde.StatusNotifierWatcher");
const QString WatcherPath = QStringLiteral("/StatusNotifierWatcher");
}

TrayIcon::TrayIcon(AppStatus *status, SyncController *sync, QObject *parent)
    : QObject(parent)
    , m_status(status)
    , m_sync(sync)
    , m_item(new KStatusNotifierItem(QStringLiteral("konedrive"), this))
    , m_menu(new QMenu)
{
    m_item->setCategory(KStatusNotifierItem::ApplicationStatus);
    m_item->setStatus(KStatusNotifierItem::Active);
    m_item->setTitle(i18nc("@title", "KOneDrive"));
    m_item->setToolTipTitle(i18nc("@title", "KOneDrive"));
    m_item->setToolTipIconByName(QStringLiteral("folder-cloud"));
    // Our own "Quit" and no "Restore"/"Minimize": the window is not the item's
    // associated window, so a click comes to toggleWindow().
    m_item->setStandardActionsEnabled(false);

    m_openFolder = m_menu->addAction(QIcon::fromTheme(QStringLiteral("folder-cloud")), i18nc("@action:inmenu", "Open OneDrive Folder"));
    m_openWindow = m_menu->addAction(QIcon::fromTheme(QStringLiteral("window")), i18nc("@action:inmenu", "Open KOneDrive"));
    m_refresh = m_menu->addAction(QIcon::fromTheme(QStringLiteral("view-refresh")), i18nc("@action:inmenu", "Refresh Now"));
    m_menu->addSeparator();
    m_quit = m_menu->addAction(QIcon::fromTheme(QStringLiteral("application-exit")), i18nc("@action:inmenu", "Quit"));
    m_item->setContextMenu(m_menu); // the item owns the menu

    // M2: unlike a click on the item itself (toggleWindow), these came
    // straight from a QAction::triggered, with no chance yet to hand over
    // the menu click's own xdg-activation token.
    connect(m_openFolder, &QAction::triggered, this, &TrayIcon::openFolderWithToken);
    connect(m_openWindow, &QAction::triggered, this, &TrayIcon::openWindowWithToken);
    connect(m_refresh, &QAction::triggered, m_sync, &SyncController::refresh);
    connect(m_quit, &QAction::triggered, this, &TrayIcon::quitRequested);
    connect(m_item, &KStatusNotifierItem::activateRequested, this, &TrayIcon::toggleWindow);

    connect(m_status, &AppStatus::changed, this, &TrayIcon::update);
    connect(m_sync, &SyncController::syncChanged, this, &TrayIcon::update);
    update();

    // Is there a system tray to show the icon (and to come back from)?
    auto bus = QDBusConnection::sessionBus();
    auto *hostWatcher = new QDBusServiceWatcher(WatcherService, bus, QDBusServiceWatcher::WatchForOwnerChange, this);
    connect(hostWatcher, &QDBusServiceWatcher::serviceOwnerChanged, this, [this](const QString &, const QString &, const QString &newOwner) {
        if (newOwner.isEmpty()) {
            setTrayAvailable(false);
        } else {
            checkTrayHost();
        }
    });
    bus.connect(WatcherService, WatcherPath, WatcherService, QStringLiteral("StatusNotifierHostRegistered"), this, SLOT(checkTrayHost()));
    bus.connect(WatcherService, WatcherPath, WatcherService, QStringLiteral("StatusNotifierHostUnregistered"), this, SLOT(checkTrayHost()));
    checkTrayHost();
}

TrayIcon::~TrayIcon() = default;

void TrayIcon::setWindow(QWindow *window)
{
    if (m_window) {
        m_window->removeEventFilter(this);
    }
    m_window = window;
    if (m_window) {
        m_window->installEventFilter(this);
    }
}

void TrayIcon::update()
{
    m_item->setIconByName(m_status->iconName());
    QString tip = m_status->text();
    if (!m_status->attention().isEmpty()) {
        tip += QLatin1Char('\n') + m_status->attention();
    }
    m_item->setToolTipSubTitle(tip);

    const bool hasFolder = !m_sync->rootPath().isEmpty();
    m_openFolder->setEnabled(hasFolder);
    m_refresh->setEnabled(hasFolder && m_sync->rootSource() == QLatin1String("onedrive"));
}

bool TrayIcon::eventFilter(QObject *watched, QEvent *event)
{
    if (watched == m_window && event->type() == QEvent::Close && !m_trayAvailable) {
        // Nothing to come back from: no process should linger unseen.
        qCDebug(KONEDRIVE_APP) << "window closed with no system tray: quitting";
        Q_EMIT quitRequested();
    }
    return QObject::eventFilter(watched, event);
}

void TrayIcon::checkTrayHost()
{
    auto message = QDBusMessage::createMethodCall(WatcherService, WatcherPath, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("Get"));
    message << WatcherService << QStringLiteral("IsStatusNotifierHostRegistered");
    auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(message), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        const QDBusPendingReply<QDBusVariant> reply = *w;
        setTrayAvailable(!reply.isError() && reply.value().variant().toBool());
    });
}

void TrayIcon::setTrayAvailable(bool available)
{
    if (m_trayAvailable == available) {
        return;
    }
    m_trayAvailable = available;
    qCDebug(KONEDRIVE_APP) << "system tray" << (available ? "available" : "gone");
    Q_EMIT trayAvailableChanged();
}

void TrayIcon::toggleWindow()
{
    if (!m_window) {
        return;
    }
    if (m_window->isVisible() && m_window->isActive()) {
        qCDebug(KONEDRIVE_APP) << "hiding the window";
        m_window->hide();
        return;
    }
    // The click hands over the right to raise a window (xdg-activation on Wayland).
    if (const QString token = m_item->providedToken(); !token.isEmpty()) {
        KWindowSystem::setCurrentXdgActivationToken(token);
    }
    showWindow();
}

void TrayIcon::showWindow()
{
    if (!m_window) {
        return;
    }
    qCDebug(KONEDRIVE_APP) << "showing the window";
    m_window->show();
    m_window->raise();
    KWindowSystem::activateWindow(m_window);
    m_window->requestActivate();
}

void TrayIcon::openWindowWithToken()
{
    if (const QString token = m_item->providedToken(); !token.isEmpty()) {
        KWindowSystem::setCurrentXdgActivationToken(token);
    }
    showWindow();
}

void TrayIcon::openFolderWithToken()
{
    const QString path = m_sync->rootPath();
    if (path.isEmpty()) {
        return;
    }
    const QByteArray token = m_item->providedToken().toUtf8();
    if (!token.isEmpty()) {
        KWindowSystem::setCurrentXdgActivationToken(QString::fromUtf8(token));
    }
    auto *job = new KIO::OpenUrlJob(QUrl::fromLocalFile(path));
    job->setStartupId(token);
    job->start();
}
