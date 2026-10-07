#include "trayicon.h"

#include "accountsmodel.h"
#include "appstatus.h"
#include "currentaccount.h"
#include "trayitem.h"
#include "traysettings.h"

#include <KWindowSystem>

#include <QDBusConnection>
#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusServiceWatcher>
#include <QDBusVariant>
#include <QEvent>
#include <QWindow>

Q_LOGGING_CATEGORY(KONEDRIVE_APP, "konedrive.app", QtInfoMsg)

namespace
{
const QString WatcherService = QStringLiteral("org.kde.StatusNotifierWatcher");
const QString WatcherPath = QStringLiteral("/StatusNotifierWatcher");
}

TrayIcon::TrayIcon(AppStatus *status, TraySettings *settings, QObject *parent)
    : QObject(parent)
    , m_status(status)
    , m_settings(settings)
{
    // Before any icon's own connections to the model: an account that went
    // loses its icon before that icon hears of the model's change.
    AccountsModel *accounts = m_status->accounts();
    connect(accounts, &QAbstractItemModel::rowsInserted, this, &TrayIcon::reconcile);
    connect(accounts, &QAbstractItemModel::rowsRemoved, this, &TrayIcon::reconcile);
    connect(accounts, &QAbstractItemModel::rowsMoved, this, &TrayIcon::reconcile);
    connect(accounts, &QAbstractItemModel::modelReset, this, &TrayIcon::reconcile);
    if (m_settings) {
        connect(m_settings, &TraySettings::perAccountChanged, this, &TrayIcon::reconcile);
    }
    reconcile();

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

void TrayIcon::setCurrentAccount(const CurrentAccount *current)
{
    m_current = current;
}

void TrayIcon::reconcile()
{
    const QList<AccountItem *> &items = m_status->accounts()->items();
    // One account or none: one icon says it all, and there is always an icon
    // to come back from.
    const bool perAccount = m_settings && m_settings->perAccount() && items.size() > 1;
    if (!perAccount) {
        qDeleteAll(m_perAccount);
        m_perAccount.clear();
        if (!m_one) {
            m_one = add(nullptr);
        }
        return;
    }

    delete m_one;
    m_one = nullptr;
    QList<TrayItem *> kept;
    for (AccountItem *item : items) {
        TrayItem *icon = nullptr;
        for (TrayItem *have : std::as_const(m_perAccount)) {
            if (have->account() == item) {
                icon = have;
                break;
            }
        }
        kept.append(icon ? icon : add(item));
    }
    for (TrayItem *have : std::as_const(m_perAccount)) {
        if (!kept.contains(have)) {
            delete have;
        }
    }
    m_perAccount = kept;
}

TrayItem *TrayIcon::add(AccountItem *only)
{
    auto *icon = new TrayItem(m_status, only, this);
    connect(icon, &TrayItem::activated, this, [this, icon] {
        toggleWindow(icon);
    });
    connect(icon, &TrayItem::openWindowRequested, this, [this, icon] {
        openWindowWithToken(icon);
    });
    connect(icon, &TrayItem::quitRequested, this, &TrayIcon::quitRequested);
    return icon;
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

void TrayIcon::toggleWindow(TrayItem *clicked)
{
    if (!m_window) {
        return;
    }
    // An account's own icon shows that account; the icon of them all, the one
    // account needing attention when exactly one does.
    const AccountItem *toShow = clicked->account() ? clicked->account() : m_status->onlyAccountNeedingAttention();
    if (m_window->isVisible() && m_window->isActive()) {
        if (clicked->account() && m_current && m_current->path() != toShow->path()) {
            qCDebug(KONEDRIVE_APP) << "turning the window to another account";
            Q_EMIT accountToShow(toShow->path());
            return;
        }
        qCDebug(KONEDRIVE_APP) << "hiding the window";
        m_window->hide();
        return;
    }
    // The click hands over the right to raise a window (xdg-activation on Wayland).
    if (const QString token = clicked->token(); !token.isEmpty()) {
        KWindowSystem::setCurrentXdgActivationToken(token);
    }
    if (toShow) {
        Q_EMIT accountToShow(toShow->path());
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

void TrayIcon::openWindowWithToken(TrayItem *clicked)
{
    if (const QString token = clicked->token(); !token.isEmpty()) {
        KWindowSystem::setCurrentXdgActivationToken(token);
    }
    if (clicked->account()) {
        Q_EMIT accountToShow(clicked->account()->path());
    }
    showWindow();
}
