#include "trayitem.h"

#include "accountsmodel.h"
#include "appstatus.h"

#include <KIO/OpenUrlJob>
#include <KLocalizedString>
#include <KStatusNotifierItem>
#include <KWindowSystem>

#include <QAction>
#include <QMenu>
#include <QUrl>

namespace
{
/// The tray keeps what the user chose for an icon (shown, hidden) under its id:
/// the icon of them all keeps the id it always had, an account's carries the account's.
QString itemId(const AccountItem *only)
{
    return only ? QStringLiteral("konedrive-") + only->id() : QStringLiteral("konedrive");
}
}

TrayItem::TrayItem(AppStatus *status, AccountItem *only, QObject *parent)
    : QObject(parent)
    , m_status(status)
    , m_single(only != nullptr)
    , m_only(only)
    , m_item(new KStatusNotifierItem(itemId(only), this))
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
    // associated window, so a click comes to activated().
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
            for (AccountItem *item : accounts()) {
                if (!item->sync()->rootPath().isEmpty() && item->sync()->rootSource() == QLatin1String("onedrive") && !item->sync()->paused()) {
                    item->sync()->pause(forSeconds);
                }
            }
        });
    }
    m_pauseMenuAction = m_menu->addMenu(m_pauseMenu);
    m_resume = m_menu->addAction(QIcon::fromTheme(QStringLiteral("media-playback-start")), i18nc("@action:inmenu", "Resume Syncing"));
    connect(m_resume, &QAction::triggered, this, [this] {
        for (AccountItem *item : accounts()) {
            if (item->sync()->paused()) {
                item->sync()->resume();
            }
        }
    });
    // The account's own hold (metered connection, battery). The icon of every account
    // lifts every account's (writes.md §11), as `sync anyway --all`; an account's icon
    // lifts that account's, as its Status page does.
    m_syncAnyway = m_menu->addAction(QIcon::fromTheme(QStringLiteral("media-playback-start")), i18nc("@action:inmenu", "Sync Anyway"));
    connect(m_syncAnyway, &QAction::triggered, this, [this] {
        for (AccountItem *item : accounts()) {
            if (!item->sync()->heldBack().isEmpty() && !item->sync()->paused()) {
                item->sync()->syncAnyway();
            }
        }
    });
    m_menu->addSeparator();
    m_quit = m_menu->addAction(QIcon::fromTheme(QStringLiteral("application-exit")), i18nc("@action:inmenu", "Quit"));
    m_item->setContextMenu(m_menu); // the item owns the menu

    // M2: unlike a click on the item itself, these came straight from a
    // QAction::triggered, with no chance yet to hand over the menu click's
    // own xdg-activation token.
    connect(m_openFolder, &QAction::triggered, this, [this] {
        if (!m_folders.isEmpty()) {
            openFolder(m_folders.constFirst().second);
        }
    });
    connect(m_openWindow, &QAction::triggered, this, &TrayItem::openWindowRequested);
    connect(m_refresh, &QAction::triggered, this, [this] {
        for (AccountItem *item : accounts()) {
            if (!item->sync()->rootPath().isEmpty() && item->sync()->rootSource() == QLatin1String("onedrive")) {
                item->sync()->refresh();
            }
        }
    });
    connect(m_quit, &QAction::triggered, this, &TrayItem::quitRequested);
    connect(m_item, &KStatusNotifierItem::activateRequested, this, &TrayItem::activated);

    connect(m_status, &AppStatus::changed, this, &TrayItem::update);
    // A folder or a label can change without the state or the tooltip.
    AccountsModel *model = m_status->accounts();
    connect(model, &QAbstractItemModel::dataChanged, this, &TrayItem::update);
    connect(model, &QAbstractItemModel::rowsInserted, this, &TrayItem::update);
    connect(model, &QAbstractItemModel::rowsRemoved, this, &TrayItem::update);
    connect(model, &QAbstractItemModel::rowsMoved, this, &TrayItem::update);
    update();
}

TrayItem::~TrayItem() = default;

QString TrayItem::token() const
{
    return m_item->providedToken();
}

QList<AccountItem *> TrayItem::accounts() const
{
    if (!m_single) {
        return m_status->accounts()->items();
    }
    return m_only ? QList<AccountItem *>{m_only.data()} : QList<AccountItem *>{};
}

void TrayItem::update()
{
    const QList<AccountItem *> items = accounts();
    if (m_single) {
        if (items.isEmpty()) {
            // The account went; TrayIcon is about to take the icon away.
            return;
        }
        const AccountItem *only = items.constFirst();
        const AccountStatus *status = only->status();
        QString toolTip = status->text();
        if (!status->attention().isEmpty()) {
            toolTip += QLatin1Char('\n') + status->attention();
        }
        const QString label = only->account()->label();
        m_item->setIconByName(AccountStatus::iconFor(status->state()));
        // The title is what the tray's own settings list the icon under.
        m_item->setTitle(i18nc("@title a tray icon: the account's label", "KOneDrive — %1", label));
        m_item->setToolTipTitle(label);
        m_item->setToolTipSubTitle(toolTip);
    } else {
        m_item->setIconByName(m_status->iconName());
        m_item->setToolTipSubTitle(m_status->toolTip());
    }

    QList<QPair<QString, QString>> folders;
    bool refreshable = false;
    bool pausable = false;
    bool paused = false;
    bool held = false;
    for (const AccountItem *item : items) {
        paused = paused || item->sync()->paused();
        held = held || (!item->sync()->heldBack().isEmpty() && !item->sync()->paused());
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
    m_syncAnyway->setVisible(held);

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

void TrayItem::openFolder(const QString &path)
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
