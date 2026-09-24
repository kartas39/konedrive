#pragma once

#include <QObject>
#include <QUrl>

class KFilePlacesModel;
class PlacesSettings;
class SyncController;
class QModelIndex;

/// Keeps one entry for the registered folder in KDE's Places panel (Dolphin's
/// left sidebar, and file dialogs everywhere): "OneDrive", `folder-cloud`
/// where Breeze has it, `cloudstatus` otherwise. Added with an empty appName,
/// so it shows in every application, and never the user's own places.
///
/// The entry is found again by a bookmark metadata item (`konedrive` = `1`,
/// KFilePlacesModel::bookmarkForIndex / KBookmark::setMetaDataItem), not by
/// its URL, since the folder can change. Reconciled on construction and
/// whenever SyncController::syncChanged or PlacesSettings::enabledChanged
/// fires:
/// - a registered folder with the setting on: the entry exists and points at
///   it, updated in place if the folder changed;
/// - no folder, or the setting off: the entry (if any) is removed.
/// Every other entry in the file is left exactly as it is.
class PlacesController : public QObject
{
    Q_OBJECT

public:
    PlacesController(SyncController *sync, PlacesSettings *settings, QObject *parent = nullptr);

    /// The icon name this entry uses: `folder-cloud` if the current icon
    /// theme has it, `cloudstatus` (present in every Breeze release) otherwise.
    static QString iconName();

    /// Exposed for tests; the model this controller owns and keeps in sync.
    KFilePlacesModel *model() const { return m_model; }

public Q_SLOTS:
    void reconcile();

private:
    QModelIndex findOurEntry() const;
    void addEntry(const QUrl &url);

    SyncController *m_sync;
    PlacesSettings *m_settings;
    KFilePlacesModel *m_model;
};
