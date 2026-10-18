#include "folderchoice.h"

#include "accountsmodel.h"
#include "currentaccount.h"
#include "foldertext.h"

#include <QDir>
#include <QFileInfo>

namespace
{
const QString ReadWrite = QStringLiteral("read-write");
}

QString suggestFolder(const QString &home, const QString &accountName, const std::function<bool(const QString &)> &isFree)
{
    const QString base = home + QStringLiteral("/OneDrive");
    if (isFree(base)) {
        return base;
    }
    // The part of an email before its @ tells two accounts apart well enough.
    const QString name = accountName.section(QLatin1Char('@'), 0, 0).trimmed();
    const QString named = name.isEmpty() ? base : base + QLatin1Char('-') + name;
    if (named != base && isFree(named)) {
        return named;
    }
    for (int n = 2; n <= 99; ++n) {
        const QString numbered = named + QLatin1Char('-') + QString::number(n);
        if (isFree(numbered)) {
            return numbered;
        }
    }
    return QString();
}

bool isFreeFolder(const QString &path, const QStringList &taken)
{
    if (taken.contains(path)) {
        return false;
    }
    const QFileInfo info(path);
    if (!info.exists() && !info.isSymLink()) {
        return true;
    }
    return info.isDir() && !info.isSymLink() && QDir(path).isEmpty(QDir::AllEntries | QDir::NoDotAndDotDot | QDir::Hidden | QDir::System);
}

FolderChoice::FolderChoice(AccountsModel *accounts, CurrentAccount *current, QObject *parent)
    : QObject(parent)
    , m_accounts(accounts)
    , m_current(current)
{
    connect(m_current, &CurrentAccount::changed, this, &FolderChoice::offer);
    // A folder registered, forgotten or renamed: what is free changes. So does the account's
    // name, which the offer may carry.
    connect(m_accounts, &QAbstractItemModel::dataChanged, this, &FolderChoice::offer);
    connect(m_accounts, &QAbstractItemModel::rowsInserted, this, &FolderChoice::offer);
    connect(m_accounts, &QAbstractItemModel::rowsRemoved, this, &FolderChoice::offer);
    offer();
}

QString FolderChoice::folderText() const
{
    return ::folderText(m_folder);
}

void FolderChoice::offer()
{
    AccountItem *item = m_current->item();
    if (m_picked && m_picked == item) {
        return;
    }
    m_picked = nullptr;
    QString offered;
    if (item && item->sync()->rootPath().isEmpty()) {
        QStringList taken;
        for (const AccountItem *other : m_accounts->items()) {
            if (!other->sync()->rootPath().isEmpty()) {
                taken << other->sync()->rootPath();
            }
        }
        const QString email = item->account()->email();
        offered = suggestFolder(QDir::homePath(), email.isEmpty() ? item->account()->label() : email, [&taken](const QString &path) {
            return isFreeFolder(path, taken);
        });
    }
    setChosen(offered);
}

void FolderChoice::setFolder(const QUrl &folder)
{
    m_picked = m_current->item();
    setChosen(folder.toLocalFile());
}

void FolderChoice::setChosen(const QString &folder)
{
    if (folder == m_folder) {
        return;
    }
    m_folder = folder;
    Q_EMIT folderChanged();
}

void FolderChoice::choose(bool twoWay)
{
    AccountItem *item = m_current->item();
    if (!item || m_folder.isEmpty()) {
        return;
    }
    AccountController *account = item->account();
    const bool isTwoWay = account->mode() == ReadWrite;
    if (twoWay && !isTwoWay) {
        // The folder waits for the browser: a folder registered first would sync read-only
        // until then, and stay so if the sign-in is given up.
        if (m_twoWayFor) {
            disconnect(m_twoWayFor->account(), nullptr, this, nullptr);
        }
        m_twoWayFor = item;
        m_twoWayFolder = m_folder;
        connect(account, &AccountController::accountChanged, this, &FolderChoice::registerWhenTwoWay);
        connect(account, &AccountController::modeSwitchChanged, this, &FolderChoice::registerWhenTwoWay);
        account->setMode(ReadWrite, false);
        return;
    }
    if (!twoWay && isTwoWay) {
        // No folder, so nothing unsent: the switch is made at once.
        account->setMode(QStringLiteral("read-only"), false);
    }
    item->sync()->chooseFolder(QUrl::fromLocalFile(m_folder));
}

void FolderChoice::registerWhenTwoWay()
{
    if (!m_twoWayFor) {
        return;
    }
    AccountController *account = m_twoWayFor->account();
    const bool granted = account->mode() == ReadWrite;
    if (!granted && !account->switchingTo().isEmpty()) {
        return;
    }
    disconnect(account, nullptr, this, nullptr);
    AccountItem *item = m_twoWayFor;
    m_twoWayFor = nullptr;
    if (granted) {
        item->sync()->chooseFolder(QUrl::fromLocalFile(m_twoWayFolder));
    }
}
