#include "trayicon.h"

#include "accountsmodel.h"
#include "appstatus.h"

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

TrayIcon::TrayIcon(AppStatus *status, QObject *parent)
    : QObject(parent)
    , m_status(status)
    , m_item(new KStatusNotifierItem(QStringLiteral("konedrive"), this))
    , m_menu(new QMenu)
    , m_folderMenu(new QMenu(i18nc("@action:inmenu", "Open Folder"), m_menu))
    , m_pauseMenu(new QMenu(i18nc("@action:inmenu", "Pause Syncing"), m_menu))
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
    m_folderMenu->setIcon(QIcon::fromTheme(QStringLiteral("folder-cloud")));
    m_openFolderMenuAction = m_menu->addMenu(m_folderMenu);
    m_openWindow = m_menu->addAction(QIcon::fromTheme(QStringLiteral("window")), i18nc("@action:inmenu", "Open KOneDrive"));
    m_refresh = m_menu->addAction(QIcon::fromTheme(QStringLiteral("view-refresh")), i18nc("@action:inmenu", "Refresh Now"));
    // As Windows offers it: 2, 8 or 24 hours, and here also until resumed.
    m_pauseMenu->setIcon(QIcon::fromTheme(QStringLiteral("media-playback-pause")));
    const QList<QPair<QString, uint>> pauses{
        {i18nc("@action:inmenu pause syncing", "For 2 Hours"), 2 * 3600},
        {i18nc("@action:inmenu pause syncing", "For 8 Hours"), 8 * 3600},
        {i18nc("@action:inmenu pause syncing", "For 24 Hours"), 24 * 3600},
        {i18nc("@action:inmenu pause syncing", "Until Resumed"), 0},
    };
    for (const auto &[text, seconds] : pauses) {
        const uint forSeconds = seconds;
        connect(m_pauseMenu->addAction(text), &QAction::triggered, this, [this, forSeconds] {
            for (AccountItem *item : m_status->accounts()->items()) {
                if (!item->sync()->rootPath().isEmpty() && item->sync()->rootSource() == QLatin1String("onedrive") && !item->sync()->paused()) {
                    item->sync()->pause(forSeconds);
                }
            }
        });
    }
    m_pauseMenuAction = m_menu->addMenu(m_pauseMenu);
    m_resume = m_menu->addAction(QIcon::fromTheme(QStringLiteral("media-playback-start")), i18nc("@action:inmenu", "Resume Syncing"));
    connect(m_resume, &QAction::triggered, this, [this] {
        for (AccountItem *item : m_status->accounts()->items()) {
            if (item->sync()->paused()) {
                item->sync()->resume();
            }
        }
    });
    m_menu->addSeparator();
    m_quit = m_menu->addAction(QIcon::fromTheme(QStringLiteral("application-exit")), i18nc("@action:inmenu", "Quit"));
    m_item->setContextMenu(m_menu); // the item owns the menu

    // M2: unlike a click on the item itself (toggleWindow), these came
    // straight from a QAction::triggered, with no chance yet to hand over
    // the menu click's own xdg-activation token.
    connect(m_openFolder, &QAction::triggered, this, [this] {
        if (!m_folders.isEmpty()) {
            openFolder(m_folders.constFirst().second);
        }
    });
    connect(m_openWindow, &QAction::triggered, this, &TrayIcon::openWindowWithToken);
    connect(m_refresh, &QAction::triggered, this, [this] {
        for (AccountItem *item : m_status->accounts()->items()) {
            if (!item->sync()->rootPath().isEmpty() && item->sync()->rootSource() == QLatin1String("onedrive")) {
                item->sync()->refresh();
            }
        }
    });
    connect(m_quit, &QAction::triggered, this, &TrayIcon::quitRequested);
    connect(m_item, &KStatusNotifierItem::activateRequested, this, &TrayIcon::toggleWindow);

    connect(m_status, &AppStatus::changed, this, &TrayIcon::update);
    // A folder or a label can change without the state or the tooltip.
    AccountsModel *accounts = m_status->accounts();
    connect(accounts, &QAbstractItemModel::dataChanged, this, &TrayIcon::update);
    connect(accounts, &QAbstractItemModel::rowsInserted, this, &TrayIcon::update);
    connect(accounts, &QAbstractItemModel::rowsRemoved, this, &TrayIcon::update);
    connect(accounts, &QAbstractItemModel::rowsMoved, this, &TrayIcon::update);
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
    m_item->setToolTipSubTitle(m_status->toolTip());

    const QList<AccountItem *> &items = m_status->accounts()->items();
    QList<QPair<QString, QString>> folders;
    bool refreshable = false;
    bool pausable = false;
    bool paused = false;
    for (const AccountItem *item : items) {
        paused = paused || item->sync()->paused();
        const QString root = item->sync()->rootPath();
        if (root.isEmpty()) {
            continue;
        }
        folders.append({item->account()->label(), root});
        const bool onedrive = item->sync()->rootSource() == QLatin1String("onedrive");
        refreshable = refreshable || onedrive;
        pausable = pausable || (onedrive && !item->sync()->paused());
    }

    // One account: "Open OneDrive Folder", as ever. Several: "Open Folder", a
    // submenu with the accounts that have a folder.
    const bool several = items.size() > 1;
    m_openFolder->setVisible(!several);
    m_openFolder->setEnabled(!several && !folders.isEmpty());
    m_openFolderMenuAction->setVisible(several);
    m_openFolderMenuAction->setEnabled(several && !folders.isEmpty());
    m_refresh->setEnabled(refreshable);
    m_pauseMenuAction->setVisible(pausable || !paused);
    m_pauseMenuAction->setEnabled(pausable);
    m_resume->setVisible(paused);

    if (folders == m_folders) {
        return;
    }
    m_folders = folders;
    m_folderMenu->clear();
    for (const auto &[label, root] : std::as_const(m_folders)) {
        // A label may hold "&", which a menu would take for a mnemonic.
        QAction *open = m_folderMenu->addAction(QIcon::fromTheme(QStringLiteral("folder-cloud")), QString(label).replace(QLatin1Char('&'), QStringLiteral("&&")));
        open->setToolTip(root);
        const QString path = root;
        connect(open, &QAction::triggered, this, [this, path] {
            openFolder(path);
        });
    }
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
    if (const AccountItem *item = m_status->onlyAccountNeedingAttention()) {
        Q_EMIT accountToShow(item->path());
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

void TrayIcon::openFolder(const QString &path)
{
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
