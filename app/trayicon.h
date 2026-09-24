#pragma once

#include <QLoggingCategory>
#include <QObject>
#include <QPointer>

/// The app's own log: konedrive.app, debug off unless QT_LOGGING_RULES turns it on.
Q_DECLARE_LOGGING_CATEGORY(KONEDRIVE_APP)

class AppStatus;
class KStatusNotifierItem;
class QAction;
class QMenu;
class QWindow;
class SyncController;

/// The tray icon: its icon and tooltip follow AppStatus, a click
/// shows or hides the window, and its menu opens the folder or the window,
/// refreshes, or quits.
class TrayIcon : public QObject
{
    Q_OBJECT

public:
    TrayIcon(AppStatus *status, SyncController *sync, QObject *parent = nullptr);
    ~TrayIcon() override;

    void setWindow(QWindow *window);

    KStatusNotifierItem *item() const { return m_item; }
    /// A system tray shows the icon (a StatusNotifierWatcher with a host).
    /// Without one, closing the window quits the app.
    bool trayAvailable() const { return m_trayAvailable; }
    QAction *openFolderAction() const { return m_openFolder; }
    QAction *openWindowAction() const { return m_openWindow; }
    QAction *refreshAction() const { return m_refresh; }
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

protected:
    bool eventFilter(QObject *watched, QEvent *event) override;

private Q_SLOTS:
    /// Asks the StatusNotifierWatcher whether a host (a tray) is registered.
    void checkTrayHost();
    /// "Open KOneDrive": hands the menu click's activation token to the window (M2).
    void openWindowWithToken();
    /// "Open OneDrive Folder": the same token, passed to KIO::OpenUrlJob (M2).
    void openFolderWithToken();

private:
    void update();
    void setTrayAvailable(bool available);

    AppStatus *m_status;
    SyncController *m_sync;
    KStatusNotifierItem *m_item;
    QMenu *m_menu;
    QAction *m_openFolder;
    QAction *m_openWindow;
    QAction *m_refresh;
    QAction *m_quit;
    QPointer<QWindow> m_window;
    bool m_trayAvailable = false;
};
