#pragma once

#include <QList>
#include <QLoggingCategory>
#include <QObject>
#include <QPointer>

/// The app's own log: konedrive.app, debug off unless QT_LOGGING_RULES turns it on.
Q_DECLARE_LOGGING_CATEGORY(KONEDRIVE_APP)

class AccountItem;
class AppStatus;
class CurrentAccount;
class TrayItem;
class TraySettings;
class QWindow;

/// The app in the tray. With several accounts and TraySettings saying so,
/// each account has its own icon (TrayItem): its state, its tooltip and a
/// menu that acts on it alone; a click shows the window on that account.
/// Otherwise — one account or none, the setting off, or no settings given —
/// there is one icon for them all: the worst state across the accounts, a
/// tooltip line per account, a menu that acts on every account, and a click
/// that shows the window on the one account needing attention, when exactly
/// one does. A click on an icon whose window is in front hides the window.
class TrayIcon : public QObject
{
    Q_OBJECT

public:
    explicit TrayIcon(AppStatus *status, TraySettings *settings = nullptr, QObject *parent = nullptr);
    ~TrayIcon() override;

    void setWindow(QWindow *window);
    /// The account the window shows: a click on another account's icon then
    /// turns the window in front to that account instead of hiding it.
    void setCurrentAccount(const CurrentAccount *current);

    /// The icon of every account; null while each account has its own.
    TrayItem *one() const { return m_one; }
    /// The accounts' own icons, in account order; empty while one icon stands for them all.
    QList<TrayItem *> perAccount() const { return m_perAccount; }
    /// A system tray shows the icons (a StatusNotifierWatcher with a host).
    /// Without one, closing the window quits the app.
    bool trayAvailable() const { return m_trayAvailable; }

public Q_SLOTS:
    void showWindow();

Q_SIGNALS:
    /// "Quit" in a menu, or the window closed with no tray to return to;
    /// main() quits the application on it.
    void quitRequested();
    void trayAvailableChanged();
    /// A click is about to show the window on this account (its object path):
    /// the account of the icon clicked, or with one icon for them all the only
    /// account needing attention.
    void accountToShow(const QString &path);

protected:
    bool eventFilter(QObject *watched, QEvent *event) override;

private Q_SLOTS:
    /// Asks the StatusNotifierWatcher whether a host (a tray) is registered.
    void checkTrayHost();

private:
    /// Brings the icons in line with the setting and the accounts.
    void reconcile();
    TrayItem *add(AccountItem *only);
    /// A click on this icon: shows the window if it is hidden or behind
    /// others, hides it if it is in front.
    void toggleWindow(TrayItem *clicked);
    /// "Open KOneDrive": hands the menu click's activation token to the window (M2).
    void openWindowWithToken(TrayItem *clicked);
    void setTrayAvailable(bool available);

    AppStatus *m_status;
    TraySettings *m_settings;
    TrayItem *m_one = nullptr;
    QList<TrayItem *> m_perAccount;
    QPointer<QWindow> m_window;
    QPointer<const CurrentAccount> m_current;
    bool m_trayAvailable = false;
};
