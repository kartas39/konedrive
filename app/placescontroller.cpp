#include "placescontroller.h"

#include "placessettings.h"
#include "synccontroller.h"

#include <KBookmark>
#include <KFilePlacesModel>
#include <KLocalizedString>

#include <QIcon>
#include <QLatin1String>

namespace
{
const QString MetaDataKey = QStringLiteral("konedrive");
const QString MetaDataValue = QStringLiteral("1");
}

PlacesController::PlacesController(SyncController *sync, PlacesSettings *settings, QObject *parent)
    : QObject(parent)
    , m_sync(sync)
    , m_settings(settings)
    , m_model(new KFilePlacesModel(this))
{
    connect(m_sync, &SyncController::syncChanged, this, &PlacesController::reconcile);
    connect(m_settings, &PlacesSettings::enabledChanged, this, &PlacesController::reconcile);
    reconcile();
}

QString PlacesController::iconName()
{
    return QIcon::hasThemeIcon(QStringLiteral("folder-cloud")) ? QStringLiteral("folder-cloud") : QStringLiteral("cloudstatus");
}

QModelIndex PlacesController::findOurEntry() const
{
    for (int row = 0; row < m_model->rowCount(); ++row) {
        const QModelIndex idx = m_model->index(row, 0);
        if (m_model->bookmarkForIndex(idx).metaDataItem(MetaDataKey) == MetaDataValue) {
            return idx;
        }
    }
    return QModelIndex();
}

void PlacesController::addEntry(const QUrl &url)
{
    const QString text = i18n("OneDrive");
    const QString icon = iconName();
    m_model->addPlace(text, url, icon, QString());

    // addPlace does not hand back the new row, and a user could already
    // have a place at this exact url, so take the last match rather than the
    // first: addPlace appends, so the last matching row is the one just made
    // (docs/limitations-and-workarounds.md, A12).
    QModelIndex added;
    for (int row = m_model->rowCount() - 1; row >= 0; --row) {
        const QModelIndex idx = m_model->index(row, 0);
        if (m_model->url(idx) == url) {
            added = idx;
            break;
        }
    }
    if (!added.isValid()) {
        return;
    }
    KBookmark bookmark = m_model->bookmarkForIndex(added);
    bookmark.setMetaDataItem(MetaDataKey, MetaDataValue);
    // addPlace's own save can already have written the file before the tag
    // above was set on the shared bookmark element; editPlace's job is to
    // change and persist a place, so calling it with the same values again
    // forces the tag to disk too, not just into this process' memory.
    m_model->editPlace(added, text, url, icon, QString());
    m_model->refresh();
}

void PlacesController::reconcile()
{
    const bool wantEntry = m_settings->enabled() && !m_sync->rootPath().isEmpty();
    const QModelIndex existing = findOurEntry();

    if (!wantEntry) {
        if (existing.isValid()) {
            m_model->removePlace(existing);
        }
        return;
    }

    const QUrl url = QUrl::fromLocalFile(m_sync->rootPath());
    if (existing.isValid()) {
        if (m_model->url(existing) != url) {
            m_model->editPlace(existing, i18n("OneDrive"), url, iconName(), QString());
        }
        return;
    }

    addEntry(url);
}
