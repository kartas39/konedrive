#include "accountsmodel.h"

#include "accountcontroller.h"
#include "daemoncontroller.h"
#include "synccontroller.h"

#include <KLocalizedString>

#include <QRegularExpression>

AccountItem::AccountItem(const QDBusConnection &bus, const QString &path, DaemonController *daemon, const AccountStatus::Clock &clock, QObject *parent)
    : QObject(parent)
    , m_path(path)
    , m_account(new AccountController(bus, path, this))
    , m_sync(new SyncController(bus, path, this))
    , m_status(new AccountStatus(m_account, m_sync, daemon, clock, this))
{
}

QString AccountItem::id() const
{
    return m_account->id();
}

AccountsModel::AccountsModel(DaemonController *daemon, AccountStatus::Clock clock, QObject *parent)
    : QAbstractListModel(parent)
    , m_daemon(daemon)
    , m_clock(std::move(clock))
{
    connect(m_daemon, &DaemonController::accountsChanged, this, [this] {
        follow(m_daemon->accounts());
    });
    follow(m_daemon->accounts());
}

int AccountsModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : int(m_items.size());
}

QVariant AccountsModel::data(const QModelIndex &index, int role) const
{
    const AccountItem *item = index.isValid() ? at(index.row()) : nullptr;
    if (!item) {
        return {};
    }
    switch (role) {
    case Qt::DisplayRole:
    case LabelRole:
        return item->account()->label();
    case PathRole:
        return item->path();
    case IdRole:
        return item->id();
    case EmailRole:
        return item->account()->email();
    case AccountStateRole:
        return item->account()->state();
    case StatusStateRole:
        return item->status()->state();
    case IconNameRole:
        return item->status()->iconName();
    case StatusTextRole:
        return item->status()->text();
    case RootPathRole:
        return item->sync()->rootPath();
    case ItemRole:
        return QVariant::fromValue(const_cast<AccountItem *>(item));
    default:
        return {};
    }
}

QHash<int, QByteArray> AccountsModel::roleNames() const
{
    return {
        {PathRole, "path"},
        {IdRole, "accountId"},
        {LabelRole, "label"},
        {EmailRole, "email"},
        {AccountStateRole, "accountState"},
        {StatusStateRole, "statusState"},
        {IconNameRole, "iconName"},
        {StatusTextRole, "statusText"},
        {RootPathRole, "rootPath"},
        {ItemRole, "item"},
    };
}

bool AccountsModel::anySignedIn() const
{
    for (const AccountItem *item : m_items) {
        if (item->account()->state() != QLatin1String("signed-out")) {
            return true;
        }
    }
    return false;
}

AccountItem *AccountsModel::find(const QString &path) const
{
    return at(indexOf(path));
}

int AccountsModel::indexOf(const QString &path) const
{
    for (int row = 0; row < m_items.size(); ++row) {
        if (m_items.at(row)->path() == path) {
            return row;
        }
    }
    return -1;
}

void AccountsModel::onEachAccount(std::function<void(AccountItem *)> setUp)
{
    for (AccountItem *item : std::as_const(m_items)) {
        setUp(item);
    }
    m_setUps.push_back(std::move(setUp));
}

void AccountsModel::follow(const QStringList &paths)
{
    for (int row = int(m_items.size()) - 1; row >= 0; --row) {
        if (!paths.contains(m_items.at(row)->path())) {
            removeAt(row);
        }
    }
    for (int i = 0; i < paths.size(); ++i) {
        const int row = indexOf(paths.at(i));
        if (row == i) {
            continue;
        }
        if (row < 0) {
            insert(i, paths.at(i));
            continue;
        }
        // The daemon never reorders its list; followed all the same.
        beginMoveRows(QModelIndex(), row, row, QModelIndex(), i);
        m_items.move(row, i);
        endMoveRows();
    }
}

AccountItem *AccountsModel::insert(int row, const QString &path)
{
    auto *item = new AccountItem(m_daemon->bus(), path, m_daemon, m_clock, this);
    const auto changed = [this, item] {
        rowChanged(item);
    };
    connect(item->account(), &AccountController::accountChanged, this, changed);
    connect(item->account(), &AccountController::serviceAvailableChanged, this, changed);
    connect(item->sync(), &SyncController::syncChanged, this, changed);
    connect(item->sync(), &SyncController::serviceAvailableChanged, this, changed);
    connect(item->status(), &AccountStatus::changed, this, changed);
    connect(item->account(), &AccountController::openUrlRequested, this, &AccountsModel::openUrlRequested);

    beginInsertRows(QModelIndex(), row, row);
    m_items.insert(row, item);
    endInsertRows();
    Q_EMIT countChanged();
    Q_EMIT summaryChanged();
    for (const auto &setUp : m_setUps) {
        setUp(item);
    }
    return item;
}

void AccountsModel::removeAt(int row)
{
    AccountItem *item = m_items.at(row);
    beginRemoveRows(QModelIndex(), row, row);
    m_items.removeAt(row);
    endRemoveRows();
    Q_EMIT countChanged();
    Q_EMIT summaryChanged();
    Q_EMIT accountRemoved(item);
    // Later: QML may still hold it for the rest of this event.
    item->deleteLater();
}

void AccountsModel::rowChanged(AccountItem *item)
{
    const int row = indexOf(item->path());
    if (row < 0) {
        return;
    }
    Q_EMIT dataChanged(index(row), index(row));
    Q_EMIT summaryChanged();
}

QString AccountsModel::labelProblem(const QString &label, const QString &exceptPath) const
{
    const QString name = label.trimmed();
    if (name.isEmpty()) {
        return i18n("Enter a name for the account.");
    }
    // The daemon counts characters, not UTF-16 units.
    if (name.toUcs4().size() > 40) {
        return i18n("A name can be at most 40 characters long.");
    }
    if (name.contains(QLatin1Char('/')) || name.contains(QLatin1Char('@'))) {
        return i18n("A name cannot contain “/” or “@”.");
    }
    for (const QChar c : name) {
        if (c.category() == QChar::Other_Control) {
            return i18n("A name cannot contain control characters.");
        }
    }
    // What an account id looks like, so `--account` could not tell them apart.
    static const QRegularExpression idLike(QStringLiteral("^[0-9a-fA-F]{12}$"));
    if (idLike.match(name).hasMatch()) {
        return i18n("A name cannot be 12 hexadecimal digits: that is what an account ID looks like.");
    }
    for (const AccountItem *item : m_items) {
        if (item->path() != exceptPath && item->account()->label().compare(name, Qt::CaseInsensitive) == 0) {
            return i18n("Another account is already called %1.", item->account()->label());
        }
    }
    return QString();
}

QString AccountsModel::suggestedLabel() const
{
    const QString personal = i18nc("@item:intext the name suggested for an account", "Personal");
    return labelProblem(personal).isEmpty() ? personal : QString();
}

void AccountsModel::addAccount(const QString &label, const QString &clientId)
{
    if (m_adding) {
        return;
    }
    m_adding = true;
    m_addError.clear();
    Q_EMIT addingChanged();

    const QString name = label.trimmed();
    const auto failed = [this](const QString &error) {
        finishAdding(error.isEmpty() ? i18n("The account could not be added.") : error);
    };
    const auto add = [this, name, failed] {
        m_daemon->add(
            name,
            [this](const QString &path) {
                // The Accounts property may come before or after this answer.
                AccountItem *item = find(path);
                if (!item) {
                    item = insert(int(m_items.size()), path);
                }
                finishAdding(QString());
                Q_EMIT accountAdded(path);
                item->account()->signIn();
            },
            failed);
    };
    const QString id = clientId.trimmed();
    if (!id.isEmpty() && id != m_daemon->clientId()) {
        m_daemon->setClientId(id, add, failed);
    } else {
        add();
    }
}

void AccountsModel::finishAdding(const QString &error)
{
    m_adding = false;
    m_addError = error;
    Q_EMIT addingChanged();
}

void AccountsModel::clearAddError()
{
    if (m_adding || m_addError.isEmpty()) {
        return;
    }
    m_addError.clear();
    Q_EMIT addingChanged();
}

void AccountsModel::removeAccount(const QString &path)
{
    m_daemon->remove(path);
}

void AccountsModel::retry()
{
    m_daemon->retry();
    for (AccountItem *item : std::as_const(m_items)) {
        item->account()->retry();
        item->sync()->retry();
    }
}
