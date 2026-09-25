#pragma once

#include "synctypes.h"

#include <QAbstractListModel>

/// What a failed activity event is about.
enum class ActivityFailure {
    None,
    DiskFull,
    Download,
    Update,
    /// A change made here that needs the user before it can go up.
    Upload,
};

/// Reads a Sync1 activity event by its kind: `failed` (a download) is
/// Download, `update-failed` (the replacement of a file changed in OneDrive)
/// is Update, and either with the exact detail "not enough disk space" is
/// DiskFull; `upload-failed` is Upload. Every other kind is None.
ActivityFailure classifyFailure(const QString &kind, const QString &detail);

/// Whether a conflict's other file is a copy kept beside the original (a file
/// changed on both sides, `docs/design/writes.md` §7) rather than a local version moved
/// out of the folder (a rescue, which goes to its own directory). Conflicts()
/// and the `conflict` event carry no kind, so the folder tells them apart.
bool isConflictCopy(const QString &original, const QString &other);

/// The window's "Recent" list: RecentActivity() plus every ActivityAdded
/// since, newest first, at most Capacity rows.
class ActivityModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ count NOTIFY countChanged)

public:
    static constexpr int Capacity = 50;

    enum Role {
        TimeRole = Qt::UserRole + 1,
        KindRole,
        PathRole,
        NameRole,
        FolderRole,
        DetailRole,
        TextRole,
        IconRole,
        /// The detail as shown: a reason in words for `upload-failed`, the
        /// detail itself otherwise.
        DetailTextRole,
    };
    Q_ENUM(Role)

    explicit ActivityModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    QHash<int, QByteArray> roleNames() const override;

    int count() const { return static_cast<int>(m_rows.size()); }

    /// Replaces every row with `newestFirst`, cut to Capacity.
    void setEvents(const KonedriveActivityList &newestFirst);
    /// Puts one live event on top; the oldest row goes past Capacity.
    void prepend(const KonedriveActivity &event);
    /// Whether `event` (every field equal) is already a row (M9).
    bool contains(const KonedriveActivity &event) const { return m_rows.contains(event); }

    /// What happened, in a few words: "Downloaded", "Moved in OneDrive"…
    static QString describe(const QString &kind, const QString &detail);

Q_SIGNALS:
    void countChanged();

private:
    QList<KonedriveActivity> m_rows;
};
