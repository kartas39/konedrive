#include "folderchoice.h"

#include "accountsmodel.h"
#include "currentaccount.h"
#include "foldertext.h"

#include <KLocalizedString>

#include <QDir>
#include <QFileInfo>

namespace
{
/// Why `path` cannot be an account's folder, as the window says it: empty for a folder
/// that can.
QString refusalText(FolderCheck check, const QString &path)
{
    const QString folder = folderText(path);
    switch (check) {
    case FolderCheck::Free:
        return QString();
    case FolderCheck::NotEmpty:
        return i18n("%1 is not an empty folder. Choose an empty folder for your OneDrive.", folder);
    case FolderCheck::AnotherAccountsFolder:
        return i18n("%1 is already the folder of another OneDrive. Choose another folder.", folder);
    case FolderCheck::InsideAnotherAccountsFolder:
        return i18n("%1 is inside the folder of another OneDrive. Choose a folder outside it.", folder);
    }
    return QString();
}
}

FolderCheck checkFolder(const QString &path, const QStringList &taken)
{
    const QString folder = QDir::cleanPath(path);
    for (const QString &other : taken) {
        const QString otherFolder = QDir::cleanPath(other);
        if (folder == otherFolder) {
            return FolderCheck::AnotherAccountsFolder;
        }
        if (folder.startsWith(otherFolder + QLatin1Char('/'))) {
            return FolderCheck::InsideAnotherAccountsFolder;
        }
    }
    const QFileInfo info(folder);
    if (!info.exists() && !info.isSymLink()) {
        return FolderCheck::Free;
    }
    const bool isEmptyDirectory = info.isDir() && !info.isSymLink() && QDir(folder).isEmpty(QDir::AllEntries | QDir::NoDotAndDotDot | QDir::Hidden | QDir::System);
    return isEmptyDirectory ? FolderCheck::Free : FolderCheck::NotEmpty;
}

FolderChoice::FolderChoice(AccountsModel *accounts, CurrentAccount *current, QObject *parent)
    : QObject(parent)
    , m_accounts(accounts)
    , m_current(current)
{
    // Another account shown: the refusal is about the one picked for, and shows with it.
    connect(m_current, &CurrentAccount::changed, this, &FolderChoice::refusalChanged);
}

QString FolderChoice::refusal() const
{
    return m_refused && m_refused == m_current->item() ? m_refusal : QString();
}

void FolderChoice::setRefusal(AccountItem *item, const QString &text)
{
    if (m_refused == item && m_refusal == text) {
        return;
    }
    m_refused = item;
    m_refusal = text;
    Q_EMIT refusalChanged();
}

bool FolderChoice::choose(AccountItem *item, const QUrl &folder)
{
    if (!item) {
        return false;
    }
    const QString path = folder.toLocalFile();
    QStringList taken;
    for (const AccountItem *other : m_accounts->items()) {
        if (other != item && !other->sync()->rootPath().isEmpty()) {
            taken << other->sync()->rootPath();
        }
    }
    const QString refused = refusalText(checkFolder(path, taken), path);
    setRefusal(item, refused);
    if (!refused.isEmpty()) {
        return false;
    }
    if (item->sync()->rootPath().isEmpty()) {
        item->sync()->chooseFolder(folder);
        return true;
    }
    // A folder that stopped: forgotten first.
    const QPointer<AccountItem> forgotten = item;
    item->sync()->forgetThen([forgotten, folder] {
        if (forgotten) {
            forgotten->sync()->chooseFolder(folder);
        }
    });
    return true;
}
