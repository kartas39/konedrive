#pragma once

#include <QObject>
#include <QUrl>

class AccountsModel;
class KFilePlacesModel;
class PlacesSettings;
class QModelIndex;

/// Keeps one entry per account folder in KDE's Places panel (Dolphin's left
/// sidebar, and file dialogs everywhere), always named "OneDrive — <label>",
/// with `folder-cloud` where the icon theme has it, `cloudstatus`
/// otherwise. Added with an empty appName, so it shows in every
/// application, and never touching the user's own places.
///
/// Each entry is found again by a bookmark metadata item,
/// `konedrive-account` = the account's id (KFilePlacesModel::bookmarkForIndex,
/// KBookmark::setMetaDataItem), not by its URL, since the folder can change.
/// Reconciled whenever the accounts or PlacesSettings change. With the
/// setting off, every entry of konedrive's goes at once. With it on, only
/// once the daemon and every account have answered — until then nothing is
/// touched, so an entry keeps its place in the panel across a start:
/// - an account with a folder: its entry exists, points at the folder and
///   carries the account's name, all updated in place;
/// - an account without a folder, or a removed account: its entry (if any)
///   is removed;
/// - an entry of the single-account versions (`konedrive` = `1`) whose URL is
///   an account's folder is re-tagged to that account and renamed, keeping
///   its place; any other such entry is removed.
/// Every other entry in the file is left exactly as it is.
class PlacesController : public QObject
{
    Q_OBJECT

public:
    PlacesController(AccountsModel *accounts, PlacesSettings *settings, QObject *parent = nullptr);

    /// The icon name the entries use: `folder-cloud` if the current icon
    /// theme has it, `cloudstatus` (present in every Breeze release) otherwise.
    static QString iconName();
    /// The bookmark metadata key that names an entry's account.
    static const QString AccountKey;

    /// Exposed for tests; the model this controller owns and keeps in sync.
    KFilePlacesModel *model() const { return m_model; }

public Q_SLOTS:
    void reconcile();

private:
    /// Every account's state is known: the daemon answered, and each account's objects did.
    bool known() const;
    QModelIndex findTagged(const QString &key, const QString &value) const;
    /// The first entry konedrive made, of any account or of the single-account versions.
    QModelIndex findOurs() const;
    void addEntry(const QString &text, const QUrl &url, const QString &id);
    void tag(const QModelIndex &index, const QString &id, const QString &text, const QUrl &url);

    AccountsModel *m_accounts;
    PlacesSettings *m_settings;
    KFilePlacesModel *m_model;
};
