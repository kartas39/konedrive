#pragma once

#include "accountcontroller.h"
#include "accountstatus.h"
#include "synccontroller.h"

#include <QAbstractListModel>
#include <QDBusConnection>
#include <QHash>
#include <QList>
#include <QObject>
#include <QString>
#include <QStringList>

#include <functional>
#include <optional>
#include <vector>

class DaemonController;

/// One account: its object path and the controllers that present it.
/// AccountsModel owns it; what else hangs on an account (its notifier, its
/// download progress) is parented to it and goes when the account goes.
class AccountItem : public QObject
{
    Q_OBJECT
    Q_PROPERTY(QString path READ path CONSTANT)
    Q_PROPERTY(QString id READ id CONSTANT)
    Q_PROPERTY(AccountController *account READ account CONSTANT)
    Q_PROPERTY(SyncController *sync READ sync CONSTANT)
    Q_PROPERTY(AccountStatus *status READ status CONSTANT)

public:
    AccountItem(const QDBusConnection &bus, const QString &path, DaemonController *daemon, const AccountStatus::Clock &clock, QObject *parent);

    QString path() const { return m_path; }
    QString id() const;
    AccountController *account() const { return m_account; }
    SyncController *sync() const { return m_sync; }
    AccountStatus *status() const { return m_status; }

private:
    QString m_path;
    AccountController *m_account;
    SyncController *m_sync;
    AccountStatus *m_status;
};

/// The accounts, in the order they were added (Accounts.Accounts): one row
/// per account, each with its own AccountController, SyncController and
/// AccountStatus. Rows follow the daemon's list; while the daemon is away the
/// last list stays, each account's controllers saying the service is gone.
class AccountsModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ count NOTIFY countChanged)
    /// An account is signed in or signing in: the client id cannot change then.
    Q_PROPERTY(bool anySignedIn READ anySignedIn NOTIFY summaryChanged)
    /// Sign In is under way: SetClientId, Accounts.SignIn, and the wait for
    /// how that sign-in ends (Accounts.SignInFinished).
    Q_PROPERTY(bool adding READ adding NOTIFY addingChanged)
    /// Why the last Sign In failed; empty when it did not.
    Q_PROPERTY(QString addError READ addError NOTIFY addingChanged)

public:
    enum Roles {
        PathRole = Qt::UserRole + 1,
        IdRole,
        LabelRole,
        EmailRole,
        AccountStateRole,
        StatusStateRole,
        IconNameRole,
        StatusTextRole,
        RootPathRole,
        ItemRole,
    };
    Q_ENUM(Roles)

    explicit AccountsModel(DaemonController *daemon, AccountStatus::Clock clock = {}, QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    QHash<int, QByteArray> roleNames() const override;

    int count() const { return int(m_items.size()); }
    bool anySignedIn() const;
    bool adding() const { return m_adding; }
    QString addError() const { return m_addError; }

    DaemonController *daemon() const { return m_daemon; }
    const QList<AccountItem *> &items() const { return m_items; }
    AccountItem *at(int row) const { return row >= 0 && row < m_items.size() ? m_items.at(row) : nullptr; }
    Q_INVOKABLE AccountItem *find(const QString &path) const;
    Q_INVOKABLE int indexOf(const QString &path) const;

    /// Runs `setUp` for every account there is and every one that comes.
    void onEachAccount(std::function<void(AccountItem *)> setUp);

    /// Why `label` cannot name an account (Account.SetLabel's rules: trimmed,
    /// 1–40 characters, no "/" or control character, not 12 hexadecimal
    /// digits in any case (an account id's look), unique regardless of case
    /// among the accounts other than `exceptPath`); empty when it can. "@"
    /// is allowed: an account's label is commonly its email (A14).
    Q_INVOKABLE QString labelProblem(const QString &label, const QString &exceptPath = QString()) const;

    /// Sign In: SetClientId(clientId) when it is given and new, then
    /// Accounts.SignIn, whose URL comes out of openUrlRequested. The daemon
    /// makes the account only once the sign-in has succeeded, signed in and
    /// named (by its email); accountAdded(path) then, once its row is here,
    /// and the window opens the folder picker. Cancelled, failed, or already
    /// added as another account: no account was made, and addError says why
    /// unless it was cancelled. A daemon that goes away in the middle ends
    /// the adding too, with an error that says so.
    Q_INVOKABLE void addAccount(const QString &clientId = QString());
    /// Gives up the sign-in under way, if any (Accounts.CancelSignIn).
    Q_INVOKABLE void cancelAdd();
    /// Forgets the last Sign In's failure (the dialog opens clean).
    Q_INVOKABLE void clearAddError();
    /// Accounts.Remove; a refusal lands in DaemonController::actionError.
    Q_INVOKABLE void removeAccount(const QString &path);
    /// "Try Again" when the service was not running: every controller re-reads.
    Q_INVOKABLE void retry();

Q_SIGNALS:
    void countChanged();
    void summaryChanged();
    void addingChanged();
    /// The account Sign In added is signed in, named, and shown; the window
    /// selects it and opens the folder picker.
    void accountAdded(const QString &path);
    /// A row is gone; `item` is deleted later.
    void accountRemoved(AccountItem *item);
    /// Some account's sign-in has its URL; the window opens it in the browser.
    void openUrlRequested(const QString &url);

private:
    void follow(const QStringList &paths);
    void handleSignInFinished(uint number, const QString &outcome, const QString &message, const QString &account);
    void handleServiceChanged();
    void signInEnded(const QString &outcome, const QString &message, const QString &account);
    void cancelSignIn();
    void showAdded();
    AccountItem *insert(int row, const QString &path);
    void removeAt(int row);
    void rowChanged(AccountItem *item);
    void finishAdding(const QString &error);

    DaemonController *m_daemon;
    AccountStatus::Clock m_clock;
    QList<AccountItem *> m_items;
    std::vector<std::function<void(AccountItem *)>> m_setUps;
    bool m_adding = false;
    QString m_addError;
    /// The number of this Sign In's sign-in, from SignIn's answer until it has ended.
    std::optional<uint> m_signIn;
    /// Which Sign In the answers of SetClientId and SignIn belong to: one that
    /// comes after its adding has ended is dropped.
    quint64 m_attempt = 0;
    /// cancelAdd() was called for this adding. Before SignIn's answer, the
    /// sign-in is cancelled once it has one; a refusal or a daemon that stops
    /// then ends the adding with no error.
    bool m_cancelRequested = false;
    /// The account the sign-in ended "signed-in" with, while it is not in
    /// Accounts.List as this model has it yet; empty otherwise.
    QString m_awaitedPath;
    /// SignInFinished signals that came before SignIn's answer said which
    /// sign-in is this window's: by number; outcome, message and account.
    QHash<uint, QStringList> m_early;
};
