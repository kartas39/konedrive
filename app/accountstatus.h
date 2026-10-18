#pragma once

#include <QObject>
#include <QString>

#include <functional>

class AccountController;
class SyncController;
class QTimer;

/// One summary of one account and its folder, for the head of the window's Overview,
/// the account switcher and the tray. The state is the daemon's (Folder.Overall);
/// the reason it gives chooses the words, the one thing to do, and what needs
/// attention. The one state decided here is "the service is not running". It reads
/// only what the controllers already hold: the "checked N s ago" text is refreshed
/// from a clock every 10 s without any D-Bus traffic.
class AccountStatus : public QObject
{
    Q_OBJECT
    /// "offline", "warning", "paused", "syncing" or "ok", as the daemon decided it; "offline"
    /// too while the service is not running, and for a state this build does not know.
    Q_PROPERTY(QString state READ state NOTIFY changed)
    /// The icon for the state: state-offline, state-warning, media-playback-pause, state-sync, state-ok.
    Q_PROPERTY(QString iconName READ iconName NOTIFY changed)
    /// The status line: "Up to date · checked 20 s ago" ("Up to date · live" while changes
    /// arrive through the notification socket), "Paused: metered connection" while the account
    /// holds back by itself, "Listing your OneDrive: N items so far", "OneDrive asked to wait: 4 min left", the error…
    Q_PROPERTY(QString text READ text NOTIFY changed)
    /// Why the state is "warning" when the status line does not say it (a conflict, a failed
    /// update), by the daemon's reason; else empty.
    Q_PROPERTY(QString attention READ attention NOTIFY changed)
    /// The check of the folder on disk at the start of its sync, one line while it runs:
    /// "Checking local files — started 2 min ago, as syncing started"; empty otherwise.
    Q_PROPERTY(QString scanLine READ scanLine NOTIFY changed)
    /// The head of the Overview: what the folder is doing or what stops it, in a few words
    /// ("Up to date", "Paused until 18:00", "Syncing stopped", "Needs your attention"…).
    Q_PROPERTY(QString headline READ headline NOTIFY changed)
    /// The sentence under the headline: why, or how fresh ("Checked 20 s ago"); may be empty.
    Q_PROPERTY(QString detail READ detail NOTIFY changed)
    /// The one thing to do about the headline.
    Q_PROPERTY(Action action READ action NOTIFY changed)
    /// The trouble that does not stop the folder (Folder.Trouble), when the headline and its
    /// sentence do not say it already: under a pause, say. Empty otherwise.
    Q_PROPERTY(QString troubleLine READ troubleLine NOTIFY changed)

public:
    /// Unix seconds.
    using Clock = std::function<qint64()>;
    static constexpr int TickMs = 10000;

    /// The one thing to do about the headline, as the Overview's button offers it.
    enum class Action {
        /// Nothing: the folder syncs, or waits by itself.
        None,
        /// The service is not running: start it.
        StartService,
        /// A sign-in waits in the browser: it can be given up.
        CancelSignIn,
        /// Signed out: sign in again.
        SignIn,
        /// No folder yet: choose where the OneDrive appears.
        ChooseFolder,
        /// The folder's syncing stopped on an error: a Refresh brings it up again.
        TryAgain,
        /// The user paused it: resume.
        Resume,
        /// It holds back by itself (metered, battery): sync anyway.
        SyncAnyway,
        /// Something needs the user, listed on the Issues page.
        Review,
    };
    Q_ENUM(Action)

    AccountStatus(AccountController *account, SyncController *sync, Clock clock = {}, QObject *parent = nullptr);

    QString state() const { return m_state; }
    QString iconName() const;
    QString text() const { return m_text; }
    QString attention() const { return m_attention; }
    QString scanLine() const { return m_scanLine; }
    QString headline() const { return m_headline; }
    QString detail() const { return m_detail; }
    Action action() const { return m_action; }
    QString troubleLine() const { return m_troubleLine; }

    /// The icon for a state name.
    static QString iconFor(const QString &state);
    /// Why the account holds back by itself (HeldBack), as the Overview and the tray say it:
    /// "Paused: metered connection", "Paused: on battery"…
    static QString heldBackText(const QString &reason);

    /// "20 s ago", "3 min ago"… for a unix time, against the clock.
    Q_INVOKABLE QString ago(qint64 unixSeconds) const;
    /// A unix time to come: the time of day when it is today by the clock, else a short date and time.
    Q_INVOKABLE QString until(qint64 unixSeconds) const;

public Q_SLOTS:
    /// Re-reads the clock; the 10 s timer calls it.
    void tick();

Q_SIGNALS:
    void changed();

private:
    void update();

    AccountController *m_account;
    SyncController *m_sync;
    Clock m_clock;
    QTimer *m_tick;
    QString m_state = QStringLiteral("offline");
    QString m_text;
    QString m_attention;
    QString m_scanLine;
    QString m_headline;
    QString m_detail;
    Action m_action = Action::StartService;
    QString m_troubleLine;
};
