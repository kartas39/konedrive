#pragma once

#include <QHash>
#include <QList>
#include <QObject>
#include <QString>

#include <functional>

class AccountController;
class SyncController;
class QTimer;

/// A button on a notification, beyond "Show in Folder".
struct NoticeAction {
    QString label;
    std::function<void()> run;
};

/// One desktop notification, as the Notifier decides it.
struct Notice {
    /// The event in konedrive.notifyrc: signedOut, diskFull, downloadFailed,
    /// updateFailed, uploadFailed, conflict, massDelete.
    QString event;
    QString title;
    QString text;
    /// When set, the notification offers "Show in Folder" for this path.
    QString showPath;
    /// 1 for a single event; for a summary, how many events it stands for.
    int count = 1;
    /// The account's object path: a click on the notification opens the window on it.
    QString account = QString();
    /// Buttons, in order.
    QList<NoticeAction> actions = {};
    /// When set, a click on the notification itself runs this rather than
    /// opening the window: massDelete's safe choice, RestoreDeletes.
    std::function<void()> defaultAction = {};
};

/// Where notices go. The app sends them through KNotification
/// (KNotificationSink); tests record them.
class NotificationSink
{
public:
    virtual ~NotificationSink() = default;
    virtual void send(const Notice &notice) = 0;
};

/// Sends each notice as a KNotification of component "konedrive", so each
/// event can be configured in System Settings.
class KNotificationSink : public NotificationSink
{
public:
    /// `openWindow` runs when the user clicks the notification itself, with
    /// the notice's account.
    explicit KNotificationSink(std::function<void(const QString &account)> openWindow = {});
    /// The same, given once the window exists (the sink is made before the
    /// accounts, whose Notifiers point at it).
    void setOpenWindow(std::function<void(const QString &account)> openWindow) { m_openWindow = std::move(openWindow); }
    void send(const Notice &notice) override;

private:
    std::function<void(const QString &account)> m_openWindow;
};

/// Turns what one account's daemon objects report into notifications:
/// signing out after being signed in, a full disk, a failed download, a
/// failed update, a change that cannot be uploaded, a conflict (a rescue or a
/// copy), and removals the mass-delete guard holds. Nothing else notifies. Each event kind sends at
/// most one notification per 10 s window; what arrives inside a window is
/// counted and sent as one summary when the window ends. Each account has its
/// own Notifier, so windows and summaries are per account and kind.
class Notifier : public QObject
{
    Q_OBJECT

public:
    /// Milliseconds, monotonic.
    using Clock = std::function<qint64()>;
    static constexpr qint64 WindowMs = 10000;
    /// How long a sign-out waits before it is announced: an account being
    /// removed is signed out first and gone within it, taking this Notifier
    /// (a child of its AccountItem) and the notice with it.
    static constexpr int SignOutDelayMs = 2000;

    Notifier(AccountController *account, SyncController *sync, NotificationSink *sink, Clock clock = {}, QObject *parent = nullptr);

    /// The timer that ends the windows (tests check that it is armed).
    QTimer *windowTimer() const { return m_timer; }

    /// The account's name for the titles ("Download failed — Family"),
    /// asked each time; empty (the default) leaves titles as they are —
    /// with one account there is nothing to tell apart.
    void setAccountName(std::function<QString()> name);

    /// Tests shorten SignOutDelayMs.
    void setSignOutDelay(int ms);

public Q_SLOTS:
    /// Ends every window whose time is up: sends its summary, if it counted
    /// anything, and opens a new window for it. The timer calls this.
    void flushDue();

private:
    struct Window {
        qint64 endsAt = 0;
        int pending = 0;
        Notice last;
    };

    void onActivity(qint64 time, const QString &kind, const QString &path, const QString &detail);
    void onAccountChanged();
    /// Held removals appearing (HeldCount from 0): massDelete, once each time they appear.
    void onSyncChanged();
    /// The sign-out, once SignOutDelayMs has passed with the account still signed out.
    void announceSignOut();
    /// `count` > 1 (a capped cycle's own "and N more" conflict event, M3)
    /// folds straight into the window's count rather than as one more event.
    void post(const Notice &notice, int count = 1);
    Notice summary(const Window &window) const;
    void arm();

    AccountController *m_account;
    SyncController *m_sync;
    NotificationSink *m_sink;
    Clock m_clock;
    std::function<QString()> m_accountName;
    QTimer *m_timer;
    QTimer *m_signOutTimer;
    int m_signOutDelayMs = SignOutDelayMs;
    QHash<QString, Window> m_windows;
    QString m_accountState;
    /// The user signed out from this window: the sign-out that follows is theirs, not news.
    bool m_signOutAsked = false;
    /// HeldCount as last seen; -1 while the daemon is away. The daemon's first
    /// answer only sets it (a restart replays nothing).
    int m_held = -1;
};
