#pragma once

#include <QHash>
#include <QObject>
#include <QString>

#include <functional>

class AccountController;
class SyncController;
class QTimer;

/// One desktop notification, as the Notifier decides it.
struct Notice {
    /// The event in konedrive.notifyrc: signedOut, diskFull, downloadFailed, updateFailed, conflict.
    QString event;
    QString title;
    QString text;
    /// When set, the notification offers "Show in Folder" for this path.
    QString showPath;
    /// 1 for a single event; for a summary, how many events it stands for.
    int count = 1;
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
    /// `openWindow` runs when the user clicks the notification itself.
    explicit KNotificationSink(std::function<void()> openWindow = {});
    void send(const Notice &notice) override;

private:
    std::function<void()> m_openWindow;
};

/// Turns what the daemon reports into notifications:
/// signing out after being signed in, a full disk, a failed download, a
/// failed update, a conflict. Nothing else notifies. Each event kind sends at
/// most one notification per 10 s window; what arrives inside a window is
/// counted and sent as one summary when the window ends.
class Notifier : public QObject
{
    Q_OBJECT

public:
    /// Milliseconds, monotonic.
    using Clock = std::function<qint64()>;
    static constexpr qint64 WindowMs = 10000;

    Notifier(AccountController *account, SyncController *sync, NotificationSink *sink, Clock clock = {}, QObject *parent = nullptr);

    /// The timer that ends the windows (tests check that it is armed).
    QTimer *windowTimer() const { return m_timer; }

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
    /// `count` > 1 (a capped cycle's own "and N more" conflict event, M3)
    /// folds straight into the window's count rather than as one more event.
    void post(const Notice &notice, int count = 1);
    Notice summary(const Window &window) const;
    void arm();

    AccountController *m_account;
    SyncController *m_sync;
    NotificationSink *m_sink;
    Clock m_clock;
    QTimer *m_timer;
    QHash<QString, Window> m_windows;
    QString m_accountState;
    /// The user signed out from this window: the sign-out that follows is theirs, not news.
    bool m_signOutAsked = false;
};
