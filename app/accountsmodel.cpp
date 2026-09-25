#include "accountsmodel.h"

#include "accountcontroller.h"
#include "daemoncontroller.h"
#include "synccontroller.h"

#include <KLocalizedString>

#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QRegularExpression>

const QString AccountsModel::DraftLabel = QStringLiteral("Signing in…");

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
    const QSet<QString> known(paths.cbegin(), paths.cend());
    m_hiddenDrafts.intersect(known);
    m_cleared.intersect(known);

    // A path is shown once a probe (or Sign In itself) has said its label is
    // not DraftLabel; until then it is left out, neither a row nor removed.
    QStringList visible;
    for (const QString &path : paths) {
        if (m_hiddenDrafts.contains(path)) {
            continue;
        }
        if (m_cleared.contains(path)) {
            visible << path;
            continue;
        }
        if (!m_probing.contains(path)) {
            probe(path);
        }
    }

    for (int row = int(m_items.size()) - 1; row >= 0; --row) {
        if (!visible.contains(m_items.at(row)->path())) {
            removeAt(row);
        }
    }
    for (int i = 0; i < visible.size(); ++i) {
        const int row = indexOf(visible.at(i));
        if (row == i) {
            continue;
        }
        if (row < 0) {
            insert(i, visible.at(i));
            continue;
        }
        // The daemon never reorders its list; followed all the same.
        beginMoveRows(QModelIndex(), row, row, QModelIndex(), i);
        m_items.move(row, i);
        endMoveRows();
    }
}

void AccountsModel::probe(const QString &path)
{
    m_probing.insert(path);
    auto message =
        QDBusMessage::createMethodCall(AccountController::ServiceName, path, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("GetAll"));
    message << AccountController::InterfaceName;
    auto *watcher = new QDBusPendingCallWatcher(m_daemon->bus().asyncCall(message), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, path](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        m_probing.remove(path);
        const QDBusPendingReply<QVariantMap> reply = *w;
        const QString label = reply.isError() ? QString() : reply.value().value(QStringLiteral("Label")).toString();
        if (label == DraftLabel) {
            m_hiddenDrafts.insert(path);
            if (path == m_draftPath) {
                // Already Sign In's own draft; nothing to do.
            } else if (m_adding && m_draftPath.isEmpty()) {
                // Sign In's own draft, found here before its Add() answer.
                claimDraft(path);
            } else {
                // Nobody here is managing it: an earlier run's draft, left
                // behind by a crash (A15).
                m_daemon->remove(path);
            }
        } else {
            m_cleared.insert(path);
        }
        follow(m_daemon->accounts());
    });
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
    if (name.contains(QLatin1Char('/'))) {
        return i18n("A name cannot contain “/”.");
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

void AccountsModel::addAccount(const QString &clientId)
{
    if (m_adding) {
        return;
    }
    m_adding = true;
    m_cancelRequested = false;
    m_addError.clear();
    Q_EMIT addingChanged();

    const auto failed = [this](const QString &error) {
        finishAdding(error.isEmpty() ? i18n("The account could not be added.") : error);
    };
    const auto add = [this, failed] {
        m_daemon->add(
            DraftLabel,
            [this](const QString &path) {
                // A probe (follow(), racing Add's own answer: the daemon
                // announces the new Accounts list before it replies here)
                // may have claimed it first; claimDraft() is a no-op then.
                claimDraft(path);
                if (m_cancelRequested) {
                    m_cancelRequested = false;
                    abandonDraft(QString());
                }
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

void AccountsModel::claimDraft(const QString &path)
{
    if (m_draftPath == path) {
        return;
    }
    m_draftPath = path;
    m_draftStartedSignIn = false;
    m_renamingDraft = false;
    m_hiddenDrafts.insert(path);
    m_draft = new AccountController(m_daemon->bus(), path, this);
    connect(m_draft, &AccountController::openUrlRequested, this, &AccountsModel::openUrlRequested);
    connect(m_draft, &AccountController::accountChanged, this, &AccountsModel::handleDraftChanged);
    connect(m_draft, &AccountController::actionErrorChanged, this, [this] {
        if (m_draft && !m_draft->actionError().isEmpty()) {
            abandonDraft(m_draft->actionError());
        }
    });
    m_draft->signIn();
}

void AccountsModel::handleDraftChanged()
{
    if (!m_draft) {
        return;
    }
    const QString state = m_draft->state();
    if (state == QLatin1String("signing-in")) {
        m_draftStartedSignIn = true;
        return;
    }
    if (state != QLatin1String("signed-in")) {
        // "signed-out": either the initial snapshot, before BeginSignIn's
        // answer has taken effect (ignored), or — once signing-in has been
        // seen — cancelled, refused, or a failed sign-in; LastError says why.
        if (m_draftStartedSignIn) {
            abandonDraft(m_draft->lastError());
        }
        return;
    }
    const QString email = m_draft->email();
    if (email.isEmpty()) {
        return; // GetAll's properties can arrive in more than one message.
    }
    if (m_draft->label() == email) {
        // SetLabel has taken effect: show it, choose it, and let the window
        // open the folder picker (accountAdded, as ever).
        const QString path = m_draftPath;
        m_draftPath.clear();
        m_renamingDraft = false;
        m_hiddenDrafts.remove(path);
        m_cleared.insert(path);
        m_draft->deleteLater();
        m_draft = nullptr;
        follow(m_daemon->accounts());
        finishAdding(QString());
        Q_EMIT accountAdded(path);
        return;
    }
    if (m_renamingDraft) {
        return; // SetLabel already sent; wait for it to land.
    }
    if (emailAlreadyUsed(email)) {
        abandonDraft(i18n("This account is already added."));
        return;
    }
    m_renamingDraft = true;
    m_draft->setLabel(email);
}

void AccountsModel::abandonDraft(const QString &error)
{
    const QString path = m_draftPath;
    m_draftPath.clear();
    m_draftStartedSignIn = false;
    m_renamingDraft = false;
    if (m_draft) {
        m_draft->deleteLater();
        m_draft = nullptr;
    }
    if (!path.isEmpty()) {
        m_daemon->remove(path);
    }
    finishAdding(error);
}

void AccountsModel::cancelAdd()
{
    if (!m_adding) {
        return;
    }
    if (m_draftPath.isEmpty()) {
        // Add (or SetClientId before it) is still in flight: abandon it as
        // soon as it answers.
        m_cancelRequested = true;
        return;
    }
    abandonDraft(QString());
}

bool AccountsModel::emailAlreadyUsed(const QString &email) const
{
    for (const AccountItem *item : m_items) {
        if (item->account()->label().compare(email, Qt::CaseInsensitive) == 0) {
            return true;
        }
    }
    return false;
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
