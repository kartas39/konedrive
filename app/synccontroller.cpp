#include "synccontroller.h"

#include "sync1interface.h"

#include <QDBusArgument>
#include <QDBusError>
#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusServiceWatcher>
#include <QDesktopServices>
#include <QLocale>

#include <KIO/OpenFileManagerWindowJob>
#include <KLocalizedString>

#include <algorithm>
#include <limits>

namespace
{
/// Same sentences as konedrivectl's skip-reason text, each through i18n().
QString whyText(const QString &reason)
{
    if (reason == QLatin1String("name-too-long")) {
        return i18n("The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two).");
    }
    if (reason == QLatin1String("personal-vault")) {
        return i18n("The Personal Vault is locked separately and is not synced.");
    }
    if (reason == QLatin1String("shared")) {
        return i18n("A shared folder added to your OneDrive; shared folders are not synced yet.");
    }
    if (reason == QLatin1String("onenote")) {
        return i18n("A OneNote notebook, which is not a file.");
    }
    if (reason == QLatin1String("reserved-name")) {
        return i18n("The name begins with .konedrive-, which konedrive keeps for itself.");
    }
    return i18n("It is neither a file nor a folder konedrive can show.");
}
}

const QString SyncController::ServiceName = QStringLiteral("org.konedrive.Daemon");
const QString SyncController::ObjectPath = QStringLiteral("/org/konedrive/Daemon");
const QString SyncController::InterfaceName = QStringLiteral("org.konedrive.Sync1");

SyncController::SyncController(QObject *parent)
    : SyncController(QDBusConnection::sessionBus(), parent)
{
}

SyncController::SyncController(const QDBusConnection &bus, QObject *parent)
    : QObject(parent)
    , m_bus(bus)
    , m_iface(new OrgKonedriveSync1Interface(ServiceName, ObjectPath, bus, this))
    , m_watcher(new QDBusServiceWatcher(ServiceName, bus, QDBusServiceWatcher::WatchForOwnerChange, this))
    , m_transfers(new TransferModel(this))
    , m_activity(new ActivityModel(this))
    , m_conflicts(new ConflictModel(this))
{
    registerKonedriveSyncTypes();
    m_bus.connect(ServiceName,
                  ObjectPath,
                  QStringLiteral("org.freedesktop.DBus.Properties"),
                  QStringLiteral("PropertiesChanged"),
                  this,
                  SLOT(onPropertiesChanged(QString, QVariantMap, QStringList)));
    m_bus.connect(ServiceName, ObjectPath, InterfaceName, QStringLiteral("ActivityAdded"), this, SLOT(onActivityAdded(qlonglong, QString, QString, QString)));
    connect(m_watcher, &QDBusServiceWatcher::serviceOwnerChanged, this, [this](const QString &, const QString &, const QString &newOwner) {
        if (newOwner.isEmpty()) {
            setServiceAvailable(false);
            // M8: nothing will update these again until the daemon is back;
            // a stale row would otherwise look like a download still going.
            m_transfers->setTransfers({});
        } else {
            fetchAll();
        }
    });
    fetchAll();
}

void SyncController::fetchAll()
{
    auto message = QDBusMessage::createMethodCall(ServiceName, ObjectPath, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("GetAll"));
    message << InterfaceName;
    auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(message), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        const QDBusPendingReply<QVariantMap> reply = *w;
        if (reply.isError()) {
            setServiceAvailable(false);
            return;
        }
        applyProperties(reply.value());
        setServiceAvailable(true);
        loadActivity();
        loadConflicts();
    });
}

void SyncController::onPropertiesChanged(const QString &interfaceName, const QVariantMap &changed, const QStringList &invalidated)
{
    if (interfaceName != InterfaceName) {
        return;
    }
    applyProperties(changed);
    if (!invalidated.isEmpty()) {
        fetchAll();
    }
}

void SyncController::applyProperties(const QVariantMap &p)
{
    const auto text = [&p](const char *key, QString &field) {
        if (const auto it = p.constFind(QLatin1String(key)); it != p.constEnd()) {
            field = it->toString();
        }
    };
    const auto number = [&p](const char *key, qulonglong &field) {
        if (const auto it = p.constFind(QLatin1String(key)); it != p.constEnd()) {
            field = it->toULongLong();
        }
    };
    const QString previousRoot = m_rootPath;
    const uint previousConflicts = m_conflictCount;
    text("RootPath", m_rootPath);
    text("RootState", m_rootState);
    text("RootSource", m_rootSource);
    text("LastError", m_lastError);
    text("HelperState", m_helperState);
    number("ItemsListed", m_itemsListed);
    number("ItemsPlaced", m_itemsPlaced);
    number("SkippedCount", m_skippedCount);
    number("LocalBytes", m_localBytes);
    if (const auto it = p.constFind(QLatin1String("LastChecked")); it != p.constEnd()) {
        m_lastChecked = it->toLongLong();
    }
    if (const auto it = p.constFind(QLatin1String("ConflictCount")); it != p.constEnd()) {
        m_conflictCount = it->toUInt();
    }
    if (const auto it = p.constFind(QLatin1String("PinnedCount")); it != p.constEnd()) {
        m_pinnedCount = it->toUInt();
    }
    if (const auto it = p.constFind(QLatin1String("Transfers")); it != p.constEnd()) {
        // A structured value inside a{sv} arrives as a QDBusArgument.
        m_transfers->setTransfers(it->canConvert<QDBusArgument>() ? qdbus_cast<KonedriveTransferList>(it->value<QDBusArgument>())
                                                                  : it->value<KonedriveTransferList>());
    }
    Q_EMIT syncChanged();

    // GetAll's own answer loads the lists (fetchAll); a change on the way loads them again.
    if (!m_serviceAvailable) {
        return;
    }
    if (m_rootPath != previousRoot) {
        // Registered or forgotten: the daemon's lists start over.
        loadActivity();
        loadConflicts();
    } else if (m_conflictCount != previousConflicts) {
        loadConflicts();
    }
}

void SyncController::onActivityAdded(qlonglong time, const QString &kind, const QString &path, const QString &detail)
{
    const KonedriveActivity event{time, kind, path, detail};
    // M9: the daemon stores an event before it signals it, so a RecentActivity()
    // reply already on its way back can already hold this one (loadActivity's
    // merge only guards the opposite order). Prepending it again would list
    // it twice.
    if (!m_activity->contains(event)) {
        m_activity->prepend(event);
    }
    if (m_activityLoads > 0) {
        // RecentActivity() is on its way and may not have this one.
        m_liveDuringLoad << event;
    }
    Q_EMIT activityAdded(time, kind, path, detail);
}

void SyncController::setServiceAvailable(bool available)
{
    if (m_serviceAvailable == available) {
        return;
    }
    m_serviceAvailable = available;
    Q_EMIT serviceAvailableChanged();
}

void SyncController::setActionError(const QString &message)
{
    if (m_actionError == message) {
        return;
    }
    m_actionError = message;
    Q_EMIT actionErrorChanged();
}

void SyncController::setPendingFolder(const QString &folder)
{
    if (m_pendingFolder == folder) {
        return;
    }
    m_pendingFolder = folder;
    Q_EMIT pendingFolderChanged();
}

void SyncController::call(const QDBusPendingCall &pending,
                          std::function<void(const QDBusPendingCall &)> onSuccess,
                          std::function<bool(const QDBusError &)> onError,
                          bool resetError)
{
    if (resetError) {
        setActionError(QString());
    }
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, onSuccess, onError](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        if (w->isError()) {
            if (!onError || !onError(w->error())) {
                setActionError(w->error().message());
            }
            return;
        }
        if (onSuccess) {
            onSuccess(*w);
        }
    });
}

void SyncController::quietly(const QDBusPendingCall &pending, std::function<void(const QDBusPendingCall &)> onSuccess)
{
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [onSuccess](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        // A daemon without these lists (older than) answers
        // UnknownMethod: the window shows them empty, not an error.
        if (!w->isError()) {
            onSuccess(*w);
        }
    });
}

void SyncController::chooseFolder(const QUrl &folder)
{
    const QString path = folder.toLocalFile();
    // M6: no timeout, as FreeUpSpace has — RegisterRoot can take a while
    // (the initial listing starts under it), so it bypasses the generated
    // proxy (whose timeout is shared with every other call on m_iface).
    auto message = QDBusMessage::createMethodCall(ServiceName, ObjectPath, InterfaceName, QStringLiteral("RegisterRoot"));
    message << path;
    call(
        m_bus.asyncCall(message, std::numeric_limits<int>::max()),
        [this](const QDBusPendingCall &) {
            // A no-op the first time (pendingFolder is already empty); on a
            // retry that now succeeds, this is what closes the prompt.
            setPendingFolder(QString());
        },
        [this, path](const QDBusError &error) {
            if (error.name() == QLatin1String("org.konedrive.Error.NoHelper")) {
                setPendingFolder(path);
                return true;
            }
            return false;
        });
}

void SyncController::retryRegistration()
{
    if (m_pendingFolder.isEmpty()) {
        return;
    }
    // Retries RegisterRoot(path); on the same NoHelper refusal, pendingFolder
    // simply stays as it was (I3: the window never falls back to
    // RegisterRootWithoutInterception on its own).
    chooseFolder(QUrl::fromLocalFile(m_pendingFolder));
}

void SyncController::cancelPending()
{
    setPendingFolder(QString());
}

void SyncController::forget()
{
    // M6: no timeout, as RegisterRoot and FreeUpSpace have.
    const auto message = QDBusMessage::createMethodCall(ServiceName, ObjectPath, InterfaceName, QStringLiteral("UnregisterRoot"));
    call(m_bus.asyncCall(message, std::numeric_limits<int>::max()));
}

void SyncController::refresh()
{
    call(m_iface->Refresh());
}

void SyncController::loadSkipped()
{
    // M7: an incidental background reload (the Skipped page reloads itself
    // whenever it is shown or skippedCount changes) must never wipe out an
    // actionError the user has not seen yet.
    call(
        m_iface->Skipped(),
        [this](const QDBusPendingCall &pending) {
            const QDBusPendingReply<KonedriveSkippedList> reply = pending;
            m_skipped.clear();
            for (const KonedriveSkippedItem &item : reply.value()) {
                m_skipped << QVariantMap{{QStringLiteral("path"), item.path}, {QStringLiteral("reason"), item.reason}, {QStringLiteral("why"), whyText(item.reason)}};
            }
            Q_EMIT skippedChanged();
        },
        {},
        false);
}

void SyncController::openFolder()
{
    if (!m_rootPath.isEmpty()) {
        QDesktopServices::openUrl(QUrl::fromLocalFile(m_rootPath));
    }
}

void SyncController::loadActivity()
{
    ++m_activityLoads;
    auto *watcher = new QDBusPendingCallWatcher(m_iface->RecentActivity(ActivityModel::Capacity), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        --m_activityLoads;
        const QDBusPendingReply<KonedriveActivityList> reply = *w;
        // A daemon without RecentActivity (older than) leaves the list as it is.
        if (!reply.isError()) {
            // The answer, plus what was signalled while it was on its way and
            // is not in it (signalled before stored), each once.
            KonedriveActivityList events = reply.value();
            for (const KonedriveActivity &live : std::as_const(m_liveDuringLoad)) {
                if (!events.contains(live)) {
                    events << live;
                }
            }
            std::stable_sort(events.begin(), events.end(), [](const KonedriveActivity &a, const KonedriveActivity &b) {
                return a.time > b.time;
            });
            m_activity->setEvents(events);
        }
        if (m_activityLoads == 0) {
            m_liveDuringLoad.clear();
        }
    });
}

void SyncController::loadConflicts()
{
    quietly(m_iface->Conflicts(), [this](const QDBusPendingCall &pending) {
        const QDBusPendingReply<KonedriveConflictList> reply = pending;
        m_conflicts->setConflicts(reply.value());
    });
}

void SyncController::dismissConflict(const QString &rescuedPath)
{
    call(m_iface->DismissConflict(rescuedPath), [this](const QDBusPendingCall &) {
        loadConflicts();
    });
}

void SyncController::freeUpSpace()
{
    if (m_freeingUp) {
        return;
    }
    m_freeingUp = true;
    m_freeUpResult.clear();
    Q_EMIT freeUpResultChanged();
    // Dehydrating a large folder can take minutes: no D-Bus timeout (INT_MAX
    // is libdbus's "infinite"), where the generated proxy would give up at 25 s.
    const auto message = QDBusMessage::createMethodCall(ServiceName, ObjectPath, InterfaceName, QStringLiteral("FreeUpSpace"));
    auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(message, std::numeric_limits<int>::max()), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        m_freeingUp = false;
        const QDBusPendingReply<uint, qulonglong, uint> reply = *w;
        if (reply.isError()) {
            m_freeUpResult = i18n("Could not free up space: %1", reply.error().message());
        } else {
            const uint files = reply.argumentAt<0>();
            const qulonglong bytes = reply.argumentAt<1>();
            const uint busy = reply.argumentAt<2>();
            m_freeUpResult = i18np("Freed 1 file (%2).", "Freed %1 files (%2).", files, QLocale().formattedDataSize(qint64(bytes)));
            if (busy > 0) {
                m_freeUpResult += QLatin1Char(' ') + i18np("1 file was in use and was kept.", "%1 files were in use and were kept.", busy);
            }
        }
        Q_EMIT freeUpResultChanged();
    });
}

void SyncController::clearFreeUpResult()
{
    if (m_freeUpResult.isEmpty()) {
        return;
    }
    m_freeUpResult.clear();
    Q_EMIT freeUpResultChanged();
}

void SyncController::setCallTimeout(int ms)
{
    m_iface->setTimeout(ms);
}

void SyncController::showInFolder(const QString &path)
{
    if (!path.isEmpty()) {
        KIO::highlightInFileManager({QUrl::fromLocalFile(path)});
    }
}

void SyncController::retry()
{
    fetchAll();
}

bool SyncController::helperTrouble() const
{
    return !m_helperState.isEmpty() && m_helperState != QLatin1String("connected");
}

QString SyncController::helperInstruction() const
{
    if (m_helperState == QLatin1String("not-installed")) {
        return i18n("Install the helper: sudo scripts/install-helper.sh (see README)");
    }
    if (m_helperState == QLatin1String("stopped")) {
        return i18n("sudo systemctl start konedrive-helper");
    }
    if (m_helperState == QLatin1String("failed")) {
        return i18n("systemctl status konedrive-helper shows why");
    }
    if (m_helperState == QLatin1String("unknown")) {
        return i18n("The daemon cannot reach the helper.");
    }
    return QString();
}
