#include "outboxmodel.h"

#include <KLocalizedString>

#include <QFileInfo>

QString uploadReasonText(const QString &reason)
{
    if (reason == QLatin1String("name-characters")) {
        return i18n("A name OneDrive refuses (it holds one of \" * : < > ? \\ |): rename it to upload it.");
    }
    if (reason == QLatin1String("name-spaces")) {
        return i18n("A name that starts or ends with a space, which OneDrive refuses: rename it to upload it.");
    }
    if (reason == QLatin1String("name-reserved")) {
        return i18n("A name OneDrive reserves: rename it to upload it.");
    }
    if (reason == QLatin1String("name-not-utf8")) {
        return i18n("A name that is not valid UTF-8: rename it to upload it.");
    }
    if (reason == QLatin1String("too-large")) {
        return i18n("Larger than OneDrive takes (250 GB).");
    }
    if (reason == QLatin1String("quota-exceeded")) {
        return i18n("OneDrive is full: free some space in OneDrive.");
    }
    if (reason == QLatin1String("forbidden")) {
        return i18n("This sign-in does not allow uploads: sign in again.");
    }
    if (reason == QLatin1String("open-for-writing")) {
        return i18n("Open for writing in another program: it goes up once closed.");
    }
    if (reason == QLatin1String("mass-delete")) {
        return i18n("Part of a large delete: delete it in OneDrive too, or restore it, on the Status page.");
    }
    if (reason == QLatin1String("symlink")) {
        return i18n("A symbolic link: never uploaded.");
    }
    if (reason == QLatin1String("fifo") || reason == QLatin1String("socket") || reason == QLatin1String("device")) {
        return i18n("Not a file or a folder: never uploaded.");
    }
    if (reason == QLatin1String("reserved-name")) {
        return i18n("A .konedrive- name, which KOneDrive keeps for itself: never uploaded.");
    }
    if (reason == QLatin1String("not-downloaded")) {
        return i18n("A file from another OneDrive folder that is not downloaded here.");
    }
    if (reason == QLatin1String("other-device")) {
        return i18n("On another filesystem mounted inside the folder: never uploaded.");
    }
    if (reason == QLatin1String("hard-link")) {
        return i18n("A file with other hard links: not uploaded.");
    }
    if (reason == QLatin1String("locked")) {
        return i18n("Locked in OneDrive (open for co-authoring): tried again later.");
    }
    if (reason.startsWith(QLatin1String("refused: "))) {
        return i18n("OneDrive refused it: %1", reason.mid(9));
    }
    return reason;
}

OutboxModel::OutboxModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int OutboxModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : count();
}

QString OutboxModel::stateText(const QString &state, const QString &kind)
{
    QString what;
    if (kind == QLatin1String("create")) {
        what = i18nc("@info outbox kind", "New");
    } else if (kind == QLatin1String("update")) {
        what = i18nc("@info outbox kind", "Changed");
    } else if (kind == QLatin1String("mkdir")) {
        what = i18nc("@info outbox kind", "New folder");
    } else if (kind == QLatin1String("move")) {
        what = i18nc("@info outbox kind", "Moved or renamed");
    } else if (kind == QLatin1String("delete")) {
        what = i18nc("@info outbox kind", "Deleted");
    } else if (kind == QLatin1String("move-out")) {
        what = i18nc("@info outbox kind", "Moved out of the folder");
    } else {
        what = kind;
    }
    QString how;
    if (state == QLatin1String("waiting")) {
        how = i18nc("@info outbox state", "waiting until it is closed");
    } else if (state == QLatin1String("ready")) {
        how = i18nc("@info outbox state", "waiting to upload");
    } else if (state == QLatin1String("running")) {
        how = i18nc("@info outbox state", "uploading now");
    } else if (state == QLatin1String("retry")) {
        how = i18nc("@info outbox state", "will be tried again");
    } else if (state == QLatin1String("blocked")) {
        how = i18nc("@info outbox state", "cannot be uploaded");
    } else if (state == QLatin1String("held")) {
        how = i18nc("@info outbox state", "held until you decide");
    } else {
        how = state;
    }
    return i18nc("@info outbox row: what changed · where it stands", "%1 · %2", what, how);
}

QVariant OutboxModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid | CheckIndexOption::ParentIsInvalid)) {
        return {};
    }
    const KonedriveOutboxRow &row = m_rows.at(index.row());
    switch (role) {
    case Qt::DisplayRole:
    case NameRole: {
        const QString name = QFileInfo(row.path).fileName();
        return name.isEmpty() ? row.path : name;
    }
    case SeqRole:
        return row.seq;
    case KindRole:
        return row.kind;
    case PathRole:
        return row.path;
    case FolderRole:
        return QFileInfo(row.path).path();
    case StateRole:
        return row.state;
    case StateTextRole:
        return stateText(row.state, row.kind);
    case DoneRole:
        return row.done;
    case TotalRole:
        return row.total;
    case FractionRole:
        return row.total > 0 ? qMin(1.0, double(row.done) / double(row.total)) : 0.0;
    case ReasonRole:
        return row.reason;
    case WhyRole:
        return row.reason.isEmpty() ? QString() : uploadReasonText(row.reason);
    case NextTryRole:
        return row.nextTry;
    case IconRole:
        if (row.state == QLatin1String("blocked")) {
            return QStringLiteral("dialog-warning");
        }
        if (row.state == QLatin1String("held") || row.kind == QLatin1String("delete")) {
            return QStringLiteral("edit-delete");
        }
        return QStringLiteral("cloud-upload");
    }
    return {};
}

QHash<int, QByteArray> OutboxModel::roleNames() const
{
    return {
        {SeqRole, "seq"},
        {KindRole, "kind"},
        {PathRole, "path"},
        {NameRole, "name"},
        {FolderRole, "folder"},
        {StateRole, "state"},
        {StateTextRole, "stateText"},
        {DoneRole, "done"},
        {TotalRole, "total"},
        {FractionRole, "fraction"},
        {ReasonRole, "reason"},
        {WhyRole, "why"},
        {NextTryRole, "nextTry"},
        {IconRole, "iconName"},
    };
}

void OutboxModel::setRows(const KonedriveOutboxList &rows)
{
    int held = 0;
    for (const KonedriveOutboxRow &row : rows) {
        if (row.state == QLatin1String("held")) {
            ++held;
        }
    }
    beginResetModel();
    m_rows = rows.mid(0, DisplayLimit);
    endResetModel();
    m_total = int(rows.size());
    m_held = held;
    Q_EMIT changed();
}
