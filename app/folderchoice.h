#pragma once

#include <QObject>
#include <QPointer>
#include <QString>
#include <QStringList>
#include <QUrl>

class AccountItem;
class AccountsModel;
class CurrentAccount;

/// What a folder picked for an account is, as its folder.
enum class FolderCheck {
    /// Not there yet, or an empty directory: it can be the account's folder.
    Free,
    /// Something is in it, or it is a file or a link, not a directory.
    NotEmpty,
    /// It is another account's folder.
    AnotherAccountsFolder,
    /// It is inside another account's folder.
    InsideAnotherAccountsFolder,
};

/// What `path` is as the folder of an account, given the folders of the other accounts
/// (`taken`): another account's folder, then a folder inside one, then what is there.
FolderCheck checkFolder(const QString &path, const QStringList &taken);

/// The folder picked in the dialog for an account, given to it once checkFolder() finds it
/// free: a folder that stopped forgotten first (Unregister, as Use Another Folder does), then
/// the folder registered, made first when it is not there. A folder that cannot be the
/// account's is not given, and `refusal` says why while that account is shown.
class FolderChoice : public QObject
{
    Q_OBJECT
    /// Why the last folder picked for the account shown cannot be its folder; empty when
    /// none was refused.
    Q_PROPERTY(QString refusal READ refusal NOTIFY refusalChanged)

public:
    FolderChoice(AccountsModel *accounts, CurrentAccount *current, QObject *parent = nullptr);

    QString refusal() const;

    /// Gives `folder` to `item` when checkFolder() finds it free, and answers true; answers
    /// false, with the refusal, when it does not.
    Q_INVOKABLE bool choose(AccountItem *item, const QUrl &folder);

Q_SIGNALS:
    void refusalChanged();

private:
    void setRefusal(AccountItem *item, const QString &text);

    AccountsModel *m_accounts;
    CurrentAccount *m_current;
    /// The account the refusal is about, and the refusal.
    QPointer<AccountItem> m_refused;
    QString m_refusal;
};
