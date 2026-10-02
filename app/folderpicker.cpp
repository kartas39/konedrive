#include "folderpicker.h"

#include "synctypes.h"

#include <KLocalizedString>

#include <QDBusArgument>
#include <QDBusError>
#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QSet>

#include <algorithm>
#include <limits>

namespace
{
const QString Service = QStringLiteral("org.konedrive.Daemon");
const QString FolderInterface = QStringLiteral("org.konedrive.Folder");

/// `path` lies below `above` (both relative to the root, "/" between names).
bool below(const QString &path, const QString &above)
{
    return path.size() > above.size() && path.startsWith(above) && path.at(above.size()) == QLatin1Char('/');
}
}

FolderPicker::FolderPicker(const QDBusConnection &bus, const QString &path, QObject *parent)
    : QAbstractListModel(parent)
    , m_bus(bus)
    , m_path(path)
{
    registerKonedriveSyncTypes();
}

int FolderPicker::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : int(m_rows.size());
}

QVariant FolderPicker::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }
    const Node &node = *m_nodes.constFind(m_rows.at(index.row()));
    switch (role) {
    case IdRole:
        return node.id;
    case Qt::DisplayRole:
    case NameRole:
        return node.name;
    case DepthRole:
        return node.depth;
    case ExpandableRole:
        return node.expandable;
    case ExpandedRole:
        return node.expanded;
    case CheckRole:
        return int(stateOf(node, m_now));
    case LoadingRole:
        return node.loading;
    }
    return {};
}

QHash<int, QByteArray> FolderPicker::roleNames() const
{
    return {{IdRole, "id"},
            {NameRole, "name"},
            {DepthRole, "depth"},
            {ExpandableRole, "expandable"},
            {ExpandedRole, "expanded"},
            {CheckRole, "check"},
            {LoadingRole, "loading"}};
}

QString FolderPicker::pathOf(const Chosen &chosen) const
{
    const auto node = m_nodes.constFind(chosen.id);
    return node != m_nodes.constEnd() ? node->path : chosen.path;
}

Qt::CheckState FolderPicker::stateOf(const QString &id, const QString &path, const Selection &selection) const
{
    if (selection.everything) {
        return Qt::Checked;
    }
    bool partial = false;
    for (const Chosen &chosen : selection.chosen) {
        if (chosen.id == id) {
            return Qt::Checked;
        }
        const QString at = pathOf(chosen);
        // A chosen folder the daemon's list does not hold has no place in the tree.
        if (at.isEmpty()) {
            continue;
        }
        if (at == path || below(path, at)) {
            return Qt::Checked;
        }
        partial = partial || below(at, path);
    }
    return partial ? Qt::PartiallyChecked : Qt::Unchecked;
}

Qt::CheckState FolderPicker::stateOf(const Node &node, const Selection &selection) const
{
    return stateOf(node.id, node.path, selection);
}

QStringList FolderPicker::chosenIds() const
{
    QStringList ids;
    for (const Chosen &chosen : m_now.chosen) {
        ids << chosen.id;
    }
    return ids;
}

bool FolderPicker::modified() const
{
    if (m_now.everything != m_initial.everything) {
        return true;
    }
    if (m_now.everything) {
        return false;
    }
    const auto ids = [](const Selection &selection) {
        QSet<QString> set;
        for (const Chosen &chosen : selection.chosen) {
            set.insert(chosen.id);
        }
        return set;
    };
    return m_now.rootFiles != m_initial.rootFiles || ids(m_now) != ids(m_initial);
}

QString FolderPicker::summary() const
{
    if (m_loading || !modified()) {
        return {};
    }
    // Folders that were on this computer and no longer are, each named once,
    // by the highest folder that leaves whole; and folders that stay with
    // only their chosen sub-folders, whose own files leave.
    QStringList leaving;
    QStringList opened;
    const auto leaves = [this](const Node &node) {
        return stateOf(node, m_initial) == Qt::Checked && stateOf(node, m_now) == Qt::Unchecked;
    };
    for (const Node &node : std::as_const(m_nodes)) {
        if (node.id.isEmpty()) {
            continue;
        }
        if (leaves(node)) {
            const auto parent = m_nodes.constFind(node.parent);
            if (node.parent.isEmpty() || parent == m_nodes.constEnd() || !leaves(*parent)) {
                leaving << node.path;
            }
        } else if (stateOf(node, m_initial) == Qt::Checked && stateOf(node, m_now) == Qt::PartiallyChecked) {
            opened << node.path;
        }
    }
    // A chosen folder whose branch was never opened, below a folder unchecked.
    for (const Chosen &chosen : m_initial.chosen) {
        if (m_nodes.contains(chosen.id) || chosen.path.isEmpty()) {
            continue;
        }
        if (stateOf(chosen.id, chosen.path, m_now) == Qt::Unchecked) {
            leaving << chosen.path;
        }
    }
    leaving.sort(Qt::CaseInsensitive);
    leaving.removeDuplicates();
    opened.sort(Qt::CaseInsensitive);

    QStringList lines;
    if (!leaving.isEmpty()) {
        lines << i18np("Removed from this computer (it stays in OneDrive): %2",
                       "Removed from this computer (they stay in OneDrive): %2",
                       leaving.size(),
                       leaving.join(QStringLiteral(", ")));
    }
    if (!opened.isEmpty()) {
        lines << i18np("Only the chosen sub-folders of %2 are synced: the files directly in it are removed from this computer too.",
                       "Only the chosen sub-folders of %2 are synced: the files directly in them are removed from this computer too.",
                       opened.size(),
                       opened.join(QStringLiteral(", ")));
    }
    if (m_initial.rootFiles && !rootFiles()) {
        lines << i18n("The files directly in the root of OneDrive are removed from this computer; they stay in OneDrive.");
    }
    return lines.join(QLatin1Char('\n'));
}

void FolderPicker::touched()
{
    if (!m_rows.isEmpty()) {
        Q_EMIT dataChanged(index(0, 0), index(int(m_rows.size()) - 1, 0), {CheckRole});
    }
    Q_EMIT changed();
}

void FolderPicker::fail(const QString &message)
{
    m_problem = message;
    Q_EMIT changed();
}

void FolderPicker::setEverything(bool on)
{
    if (m_loading || m_applying || on == m_now.everything || !m_nodes.contains(QString())) {
        return;
    }
    if (on) {
        m_saved = m_now;
        m_now.everything = true;
    } else if (m_saved) {
        m_now = *m_saved;
        m_saved.reset();
    } else {
        // Every folder of the root and the root's files: nothing leaves this
        // computer until something is unchecked.
        m_now.everything = false;
        m_now.rootFiles = true;
        m_now.chosen.clear();
        const Node &root = *m_nodes.constFind(QString());
        for (const QString &id : root.children) {
            const Node &child = *m_nodes.constFind(id);
            m_now.chosen << Chosen{child.id, child.path};
        }
    }
    m_problem.clear();
    touched();
}

void FolderPicker::setRootFiles(bool on)
{
    if (m_loading || m_applying || m_now.everything || on == m_now.rootFiles) {
        return;
    }
    m_now.rootFiles = on;
    m_problem.clear();
    touched();
}

void FolderPicker::open()
{
    const int generation = ++m_generation;
    beginResetModel();
    m_nodes.clear();
    m_rows.clear();
    m_nodes.insert(QString(), Node{});
    m_now = Selection{};
    m_initial = Selection{};
    m_saved.reset();
    m_loading = true;
    m_applying = false;
    m_incomplete = false;
    m_problem.clear();
    endResetModel();
    Q_EMIT changed();

    auto message = QDBusMessage::createMethodCall(Service, m_path, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("GetAll"));
    message << FolderInterface;
    auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(message), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, generation](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        if (generation != m_generation) {
            return;
        }
        const QDBusPendingReply<QVariantMap> reply = *w;
        if (reply.isError()) {
            m_loading = false;
            fail(reply.error().message());
            return;
        }
        const QVariantMap properties = reply.value();
        // A daemon without the selection (older) syncs everything.
        m_initial.everything = properties.value(QStringLiteral("SyncsEverything"), true).toBool();
        m_initial.rootFiles = properties.value(QStringLiteral("RootFiles"), true).toBool();
        const QVariant folders = properties.value(QStringLiteral("SelectedFolders"));
        const KonedriveSkippedList list = folders.canConvert<QDBusArgument>() ? qdbus_cast<KonedriveSkippedList>(folders.value<QDBusArgument>())
                                                                               : folders.value<KonedriveSkippedList>();
        // (item id, path in OneDrive): the type's first field is the id.
        for (const KonedriveSkippedItem &folder : list) {
            m_initial.chosen << Chosen{folder.path, folder.reason};
        }
        m_now = m_initial;
        m_incomplete = properties.value(QStringLiteral("State")).toString() == QLatin1String("listing");
        load(QString(), generation);
    });
}

void FolderPicker::load(const QString &id, int generation)
{
    m_nodes[id].loading = true;
    auto message = QDBusMessage::createMethodCall(Service, m_path, FolderInterface, QStringLiteral("FolderChildren"));
    message << id;
    auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(message), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, id, generation](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        if (generation != m_generation || !m_nodes.contains(id)) {
            return;
        }
        const QDBusPendingReply<KonedriveFolderChildList> reply = *w;
        Node &node = m_nodes[id];
        node.loading = false;
        const int row = int(m_rows.indexOf(id));
        if (reply.isError()) {
            node.expanded = false;
            if (id.isEmpty()) {
                m_loading = false;
            } else if (row >= 0) {
                Q_EMIT dataChanged(index(row, 0), index(row, 0), {ExpandedRole, LoadingRole});
            }
            fail(reply.error().message());
            return;
        }
        node.loaded = true;
        node.children.clear();
        const QString parentPath = node.path;
        const int depth = id.isEmpty() ? 0 : node.depth + 1;
        const KonedriveFolderChildList children = reply.value();
        QStringList ids;
        for (const KonedriveFolderChild &child : children) {
            Node entry;
            entry.id = child.id;
            entry.name = child.name;
            entry.path = parentPath.isEmpty() ? child.name : parentPath + QLatin1Char('/') + child.name;
            entry.parent = id;
            entry.depth = depth;
            entry.expandable = child.hasSubfolders;
            m_nodes.insert(child.id, entry);
            ids << child.id;
        }
        // `node` may dangle after the inserts above.
        m_nodes[id].children = ids;
        if (id.isEmpty()) {
            m_loading = false;
            insertChildren(id);
            Q_EMIT changed();
            return;
        }
        if (row >= 0) {
            if (m_nodes[id].expanded) {
                insertChildren(id);
            }
            Q_EMIT dataChanged(index(row, 0), index(row, 0), {ExpandedRole, LoadingRole});
        }
    });
}

void FolderPicker::insertChildren(const QString &id)
{
    const QStringList children = m_nodes.value(id).children;
    if (children.isEmpty()) {
        return;
    }
    const int at = id.isEmpty() ? 0 : int(m_rows.indexOf(id)) + 1;
    beginInsertRows({}, at, at + int(children.size()) - 1);
    for (int i = 0; i < children.size(); ++i) {
        m_rows.insert(at + i, children.at(i));
    }
    endInsertRows();
}

void FolderPicker::toggle(int row)
{
    if (row < 0 || row >= m_rows.size()) {
        return;
    }
    const QString id = m_rows.at(row);
    Node &node = m_nodes[id];
    if (!node.expandable || node.loading) {
        return;
    }
    if (!node.expanded) {
        node.expanded = true;
        if (node.loaded) {
            insertChildren(id);
        } else {
            load(id, m_generation);
        }
    } else {
        node.expanded = false;
        // Everything deeper that follows it is in the branch.
        int last = row;
        while (last + 1 < m_rows.size() && m_nodes.value(m_rows.at(last + 1)).depth > node.depth) {
            ++last;
        }
        if (last > row) {
            beginRemoveRows({}, row + 1, last);
            for (int i = last; i > row; --i) {
                m_nodes[m_rows.at(i)].expanded = false;
                m_rows.removeAt(i);
            }
            endRemoveRows();
        }
    }
    Q_EMIT dataChanged(index(row, 0), index(row, 0), {ExpandedRole, LoadingRole});
}

void FolderPicker::removeBelow(const QString &path)
{
    m_now.chosen.removeIf([this, &path](const Chosen &chosen) {
        return below(pathOf(chosen), path);
    });
}

void FolderPicker::setChecked(int row, bool checked)
{
    if (row < 0 || row >= m_rows.size() || m_now.everything || m_loading || m_applying) {
        return;
    }
    const Node node = m_nodes.value(m_rows.at(row));
    const Qt::CheckState state = stateOf(node, m_now);
    if (checked) {
        if (state == Qt::Checked) {
            return;
        }
        removeBelow(node.path);
        m_now.chosen << Chosen{node.id, node.path};
    } else if (state == Qt::PartiallyChecked) {
        removeBelow(node.path);
    } else if (state == Qt::Checked) {
        const auto own = std::find_if(m_now.chosen.begin(), m_now.chosen.end(), [&node](const Chosen &chosen) {
            return chosen.id == node.id;
        });
        if (own != m_now.chosen.end()) {
            m_now.chosen.erase(own);
        } else {
            // Inside a chosen folder: that one gives way to its other
            // sub-folders, level by level down to this folder's siblings. Every
            // level is read, or this folder would not be in sight.
            QStringList chain{node.id};
            QString chosenAbove;
            for (QString up = node.parent; !up.isEmpty(); up = m_nodes.value(up).parent) {
                chain.prepend(up);
                const Node &ancestor = *m_nodes.constFind(up);
                const bool isChosen = std::any_of(m_now.chosen.cbegin(), m_now.chosen.cend(), [this, &ancestor](const Chosen &chosen) {
                    return chosen.id == ancestor.id || pathOf(chosen) == ancestor.path;
                });
                if (isChosen) {
                    chosenAbove = up;
                    break;
                }
            }
            if (chosenAbove.isEmpty()) {
                return;
            }
            const QString abovePath = m_nodes.value(chosenAbove).path;
            m_now.chosen.removeIf([this, &chosenAbove, &abovePath](const Chosen &chosen) {
                return chosen.id == chosenAbove || pathOf(chosen) == abovePath;
            });
            for (int level = 0; level + 1 < chain.size(); ++level) {
                const QStringList siblings = m_nodes.value(chain.at(level)).children;
                for (const QString &sibling : siblings) {
                    if (sibling != chain.at(level + 1)) {
                        m_now.chosen << Chosen{sibling, m_nodes.value(sibling).path};
                    }
                }
            }
        }
    } else {
        return;
    }
    m_problem.clear();
    touched();
}

void FolderPicker::apply()
{
    if (m_loading || m_applying) {
        return;
    }
    if (!modified()) {
        Q_EMIT applied();
        return;
    }
    m_applying = true;
    m_problem.clear();
    Q_EMIT changed();
    auto message = QDBusMessage::createMethodCall(Service, m_path, FolderInterface, m_now.everything ? QStringLiteral("SyncEverything") : QStringLiteral("SetSelection"));
    if (!m_now.everything) {
        message << chosenIds() << m_now.rootFiles;
    }
    const int generation = m_generation;
    // The daemon changes its whole list of OneDrive before it answers: no timeout.
    auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(message, std::numeric_limits<int>::max()), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, generation](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        if (generation != m_generation) {
            return;
        }
        m_applying = false;
        if (w->isError()) {
            const QDBusError error = w->error();
            if (error.name() == QLatin1String("org.konedrive.Error.LocalChanges")) {
                // The daemon's message is the list: a first line, then a path and why on each.
                fail(i18n("%1\nOnce these are uploaded, or moved out of the folders that would leave or deleted, choose again.", error.message()));
            } else {
                fail(error.message());
            }
            return;
        }
        m_initial = m_now;
        m_saved.reset();
        Q_EMIT changed();
        Q_EMIT applied();
    });
}
