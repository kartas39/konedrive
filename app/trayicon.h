#pragma once

#include <QList>
#include <QLoggingCategory>
#include <QObject>
#include <QPair>
#include <QPointer>

/// The app's own log: konedrive.app, debug off unless QT_LOGGING_RULES turns it on.
Q_DECLARE_LOGGING_CATEGORY(KONEDRIVE_APP)

class AppStatus;
class KStatusNotifierItem;
class QAction;
class QMenu;
class QWindow;

/// The tray icon: its icon is the worst state across the accounts and its
/// tooltip has a line per account (AppStatus); a click shows or hides the
/// window — on the one account needing attention, when exactly one does —
/// and its menu opens a folder or the window, refreshes, pauses or resumes
/// every account, or quits.
class TrayIcon : public QObject
{
    Q_OBJECT

public:
    explicit TrayIcon(AppStatus *status, QObject *parent = nullptr);
    ~TrayIcon() override;

    void setWindow(QWindow *window);

    KStatusNotifierItem *item() const { return m_item; }
    /// A system tray shows the icon (a StatusNotifierWatcher with a host).
    /// Without one, closing the window quits the app.
    bool trayAvailable() const { return m_trayAvailable; }
    /// "Open OneDrive Folder", shown while there is at most one account.
    QAction *openFolderAction() const { return m_openFolder; }
    /// "Open Folder", shown with several accounts: a submenu with an entry
    /// for each account that has a folder.
    QAction *openFolderMenuAction() const { return m_openFolderMenuAction; }
    QMenu *openFolderMenu() const { return m_folderMenu; }
    QAction *openWindowAction() const { return m_openWindow; }
    /// "Refresh Now": every account whose folder shows OneDrive.
    QAction *refreshAction() const { return m_refresh; }
    /// "Pause Syncing": a submenu that pauses every account whose folder shows
    /// OneDrive and is not paused, for 2, 8 or 24 hours or until resumed.
    QAction *pauseMenuAction() const { return m_pauseMenuAction; }
    QMenu *pauseMenu() const { return m_pauseMenu; }
    /// "Resume Syncing": shown while any account is paused; resumes each of them.
    QAction *resumeAction() const { return m_resume; }
    QAction *quitAction() const { return m_quit; }

public Q_SLOTS:
    /// Shows the window if it is hidden or behind others, hides it if it is in front.
    void toggleWindow();
    void showWindow();

Q_SIGNALS:
    /// "Quit" in the menu, or the window closed with no tray to return to;
    /// main() quits the application on it.
    void quitRequested();
    void trayAvailableChanged();
    /// A click is about to show the window, and this account (its object
    /// path) is the only one needing attention: the window shows it.
    void accountToShow(const QString &path);

protected:
    bool eventFilter(QObject *watched, QEvent *event) override;

private Q_SLOTS:
    /// Asks the StatusNotifierWatcher whether a host (a tray) is registered.
    void checkTrayHost();
    /// "Open KOneDrive": hands the menu click's activation token to the window (M2).
    void openWindowWithToken();

private:
    void update();
    void setTrayAvailable(bool available);
    /// Opens a folder with the menu click's token, passed to KIO::OpenUrlJob (M2).
    void openFolder(const QString &path);

    AppStatus *m_status;
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
    QAction *m_quit;
    /// (label, folder) of each account that has a folder, in account order.
    QList<QPair<QString, QString>> m_folders;
    QPointer<QWindow> m_window;
    bool m_trayAvailable = false;
};
