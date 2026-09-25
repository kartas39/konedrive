#include "placescontroller.h"

#include "accountsmodel.h"
#include "daemoncontroller.h"
#include "placessettings.h"

#include <KBookmark>
#include <KFilePlacesModel>
#include <KLocalizedString>

#include <QIcon>
#include <QLatin1String>
#include <QSet>

#include <algorithm>

namespace
{
/// The single-account versions' tag: `konedrive` = `1`.
const QString OldKey = QStringLiteral("konedrive");
const QString OldValue = QStringLiteral("1");

struct Wanted {
    QString id;
    QString text;
    QUrl url;
};
}

const QString PlacesController::AccountKey = QStringLiteral("konedrive-account");

PlacesController::PlacesController(AccountsModel *accounts, PlacesSettings *settings, QObject *parent)
    : QObject(parent)
    , m_accounts(accounts)
    , m_settings(settings)
    , m_model(new KFilePlacesModel(this))
{
    connect(m_accounts, &QAbstractItemModel::dataChanged, this, &PlacesController::reconcile);
    connect(m_accounts, &QAbstractItemModel::rowsInserted, this, &PlacesController::reconcile);
    connect(m_accounts, &QAbstractItemModel::rowsRemoved, this, &PlacesController::reconcile);
    connect(m_accounts, &QAbstractItemModel::modelReset, this, &PlacesController::reconcile);
    connect(m_accounts->daemon(), &DaemonController::serviceAvailableChanged, this, &PlacesController::reconcile);
    connect(m_settings, &PlacesSettings::enabledChanged, this, &PlacesController::reconcile);
    reconcile();
}

QString PlacesController::iconName()
{
    return QIcon::hasThemeIcon(QStringLiteral("folder-cloud")) ? QStringLiteral("folder-cloud") : QStringLiteral("cloudstatus");
}

bool PlacesController::known() const
{
    if (!m_accounts->daemon()->serviceAvailable()) {
        return false;
    }
    for (const AccountItem *item : m_accounts->items()) {
        if (!item->account()->serviceAvailable() || !item->sync()->serviceAvailable()) {
            return false;
        }
    }
    return true;
}

QModelIndex PlacesController::findTagged(const QString &key, const QString &value) const
{
    for (int row = 0; row < m_model->rowCount(); ++row) {
        const QModelIndex idx = m_model->index(row, 0);
        if (m_model->bookmarkForIndex(idx).metaDataItem(key) == value) {
            return idx;
        }
    }
    return QModelIndex();
}

QModelIndex PlacesController::findOurs() const
{
    for (int row = 0; row < m_model->rowCount(); ++row) {
        const KBookmark bookmark = m_model->bookmarkForIndex(m_model->index(row, 0));
        if (!bookmark.metaDataItem(AccountKey).isEmpty() || bookmark.metaDataItem(OldKey) == OldValue) {
            return m_model->index(row, 0);
        }
    }
    return QModelIndex();
}

void PlacesController::tag(const QModelIndex &index, const QString &id, const QString &text, const QUrl &url)
{
    KBookmark bookmark = m_model->bookmarkForIndex(index);
    bookmark.setMetaDataItem(AccountKey, id);
    bookmark.setMetaDataItem(OldKey, QString());
    // editPlace saves the place only when its text, url or icon change;
    // refresh() saves the file in any case, so the tag set above on the
    // shared bookmark element reaches the disk, not just this process.
    m_model->editPlace(index, text, url, iconName(), QString());
    m_model->refresh();
}

void PlacesController::addEntry(const QString &text, const QUrl &url, const QString &id)
{
    m_model->addPlace(text, url, iconName(), QString());

    // addPlace does not hand back the new row, and a user could already
    // have a place at this exact url, so take the last match rather than the
    // first: addPlace appends, so the last matching row is the one just made
    // (docs/limitations-and-workarounds.md, A12).
    for (int row = m_model->rowCount() - 1; row >= 0; --row) {
        const QModelIndex idx = m_model->index(row, 0);
        if (m_model->url(idx) == url) {
            // addPlace's own save can already have written the file before
            // the tag is set; tag() saves again with it.
            tag(idx, id, text, url);
            return;
        }
    }
}

void PlacesController::reconcile()
{
    // "Show in Places" off: every entry of ours goes, whatever the accounts
    // say, and even while the daemon is not running.
    if (!m_settings->enabled()) {
        for (QModelIndex idx = findOurs(); idx.isValid(); idx = findOurs()) {
            m_model->removePlace(idx);
        }
        return;
    }
    // Until every account has said what its folder is, an entry that looks
    // unwanted may only be unknown yet: nothing is touched.
    if (!known()) {
        return;
    }

    QList<Wanted> wanted;
    if (m_settings->enabled()) {
        for (const AccountItem *item : m_accounts->items()) {
            const QString root = item->sync()->rootPath();
            if (!root.isEmpty()) {
                wanted.append({item->id(),
                               i18nc("@item a Places entry, %1 is the account's name", "OneDrive — %1", item->account()->label()),
                               QUrl::fromLocalFile(root)});
            }
        }
    }

    // The single-account versions' entry: the account whose folder it shows
    // takes it over, in its place in the panel; otherwise it goes.
    for (QModelIndex old = findTagged(OldKey, OldValue); old.isValid(); old = findTagged(OldKey, OldValue)) {
        const QUrl url = m_model->url(old);
        const auto heir = std::find_if(wanted.cbegin(), wanted.cend(), [this, &url](const Wanted &w) {
            return w.url == url && !findTagged(AccountKey, w.id).isValid();
        });
        if (heir != wanted.cend()) {
            tag(old, heir->id, heir->text, heir->url);
        } else {
            m_model->removePlace(old);
        }
    }

    // Entries no account wants any more (removed, forgotten, switched off),
    // and any second entry of one account; one at a time, since a removal
    // shifts the rows.
    const auto unwanted = [this, &wanted] {
        QSet<QString> seen;
        for (int row = 0; row < m_model->rowCount(); ++row) {
            const QModelIndex idx = m_model->index(row, 0);
            const QString id = m_model->bookmarkForIndex(idx).metaDataItem(AccountKey);
            if (id.isEmpty()) {
                continue;
            }
            const bool isWanted = std::any_of(wanted.cbegin(), wanted.cend(), [&id](const Wanted &w) {
                return w.id == id;
            });
            if (!isWanted || seen.contains(id)) {
                return idx;
            }
            seen.insert(id);
        }
        return QModelIndex();
    };
    for (QModelIndex idx = unwanted(); idx.isValid(); idx = unwanted()) {
        m_model->removePlace(idx);
    }

    // Each account's entry, updated in place, or added.
    for (const Wanted &w : std::as_const(wanted)) {
        const QModelIndex idx = findTagged(AccountKey, w.id);
        if (!idx.isValid()) {
            addEntry(w.text, w.url, w.id);
        } else if (m_model->url(idx) != w.url || m_model->text(idx) != w.text) {
            m_model->editPlace(idx, w.text, w.url, iconName(), QString());
        }
    }
}
