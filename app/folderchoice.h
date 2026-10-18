#pragma once

#include <QObject>
#include <QPointer>
#include <QString>
#include <QStringList>
#include <QUrl>

#include <functional>

class AccountItem;
class AccountsModel;
class CurrentAccount;

/// The folder offered to an account called `accountName` (its email, else its label) under
/// `home`: ~/OneDrive, else ~/OneDrive-<the name up to its @>, else that with -2, -3… up to
/// -99; the first `isFree` holds for. Empty when none is.
QString suggestFolder(const QString &home, const QString &accountName, const std::function<bool(const QString &)> &isFree);

/// Whether `path` can be offered as a new folder: no account's folder (`taken`), and nothing
/// there yet, or an empty directory.
bool isFreeFolder(const QString &path, const QStringList &taken);

/// The folder the account shown gets while it has none, and Continue, which gives it that
/// folder: the one suggestFolder() offers, until another is chosen in the picker. Another
/// account shown, or a folder taken meanwhile, offers again.
class FolderChoice : public QObject
{
    Q_OBJECT
    /// The folder, absolute; empty when none is offered and none was chosen.
    Q_PROPERTY(QString folder READ folder NOTIFY folderChanged)
    /// The folder as the window writes it: ~/OneDrive.
    Q_PROPERTY(QString folderText READ folderText NOTIFY folderChanged)

public:
    FolderChoice(AccountsModel *accounts, CurrentAccount *current, QObject *parent = nullptr);

    QString folder() const { return m_folder; }
    QString folderText() const;

    /// Choose Another…: the folder the picker gave.
    Q_INVOKABLE void setFolder(const QUrl &folder);
    /// Continue: the account turned two-way first when `twoWay` (its sign-in in the browser),
    /// read-only when not, and then the folder registered, made first when it is not there.
    /// A switch to two-way that ends without it registers nothing: the account says why.
    Q_INVOKABLE void choose(bool twoWay);

Q_SIGNALS:
    void folderChanged();

private:
    /// The folder offered again, unless one was chosen in the picker for this account.
    void offer();
    void setChosen(const QString &folder);
    /// Registers `folder` for `item` once its switch to two-way has ended so, and gives
    /// it up once that switch has ended otherwise.
    void registerWhenTwoWay();

    AccountsModel *m_accounts;
    CurrentAccount *m_current;
    QString m_folder;
    /// The account `m_folder` was chosen for in the picker; null while it is the one offered.
    QPointer<AccountItem> m_picked;
    /// The account waiting for its switch to two-way, and the folder it then registers.
    QPointer<AccountItem> m_twoWayFor;
    QString m_twoWayFolder;
};
