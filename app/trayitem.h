#pragma once

#include <QList>
#include <QObject>
#include <QPair>
#include <QPointer>

class AccountItem;
class AppStatus;
class KStatusNotifierItem;
class QAction;
class QMenu;

/// One icon in the tray, with its menu. It stands either for every account
/// — its icon is the worst state across them and its tooltip has a line per
/// account (AppStatus) — or for one account: that account's state, its label
/// and status line in the tooltip. The menu opens a folder or the window,
/// refreshes, pauses or resumes, lifts the account's own hold, or quits; it
/// acts on the accounts the icon stands for and on no other.
class TrayItem : public QObject
{
    Q_OBJECT

public:
    /// `only` null: the icon of every account. Otherwise that account's.
    TrayItem(AppStatus *status, AccountItem *only, QObject *parent = nullptr);
    ~TrayItem() override;

    KStatusNotifierItem *item() const { return m_item; }
    /// The one account this icon stands for; null for the icon of them all.
    AccountItem *account() const { return m_only; }
    /// The xdg-activation token of the last click on the icon or its menu.
    QString token() const;

    /// "Open OneDrive Folder", shown while the icon stands for at most one account.
    QAction *openFolderAction() const { return m_openFolder; }
    /// "Open Folder", shown with several accounts: a submenu with an entry
    /// for each account that has a folder.
    QAction *openFolderMenuAction() const { return m_openFolderMenuAction; }
    QMenu *openFolderMenu() const { return m_folderMenu; }
    QAction *openWindowAction() const { return m_openWindow; }
    /// "Refresh Now": every account of the icon whose folder shows OneDrive.
    QAction *refreshAction() const { return m_refresh; }
    /// "Pause Syncing": a submenu that pauses every account of the icon whose folder
    /// shows OneDrive and is not paused, for 2, 8 or 24 hours or until resumed.
    QAction *pauseMenuAction() const { return m_pauseMenuAction; }
    QMenu *pauseMenu() const { return m_pauseMenu; }
    /// "Resume Syncing": shown while any account of the icon is paused by the user;
    /// resumes each of them.
    QAction *resumeAction() const { return m_resume; }
    /// "Sync Anyway": shown while any account of the icon holds back by itself (HeldBack)
    /// and is not paused by the user; lifts the hold of each of them.
    QAction *syncAnywayAction() const { return m_syncAnyway; }
    QAction *quitAction() const { return m_quit; }

Q_SIGNALS:
    /// A click on the icon.
    void activated();
    /// "Open KOneDrive" in the menu.
    void openWindowRequested();
    /// "Quit" in the menu.
    void quitRequested();

private:
    /// The accounts the icon stands for.
    QList<AccountItem *> accounts() const;
    void update();
    /// Opens a folder with the menu click's token, passed to KIO::OpenUrlJob (M2).
    void openFolder(const QString &path);

    AppStatus *m_status;
    /// Whether the icon is one account's; that account may go before the icon does.
    bool m_single;
    QPointer<AccountItem> m_only;
    KStatusNotifierItem *m_item;
    QMenu *m_menu;
    QMenu *m_folderMenu;
    QAction *m_openFolder;
    QAction *m_openFolderMenuAction;
    QAction *m_openWindow;
    QAction *m_refresh;
    QMenu *m_pauseMenu;
    QAction *m_pauseMenuAction;
    QAction *m_resume;
    QAction *m_syncAnyway;
    QAction *m_quit;
    /// (label, folder) of each account that has a folder, in account order.
    QList<QPair<QString, QString>> m_folders;
};
