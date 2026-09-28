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
#include <QTimer>

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
const QString SyncController::InterfaceName = QStringLiteral("org.konedrive.Sync1");

SyncController::SyncController(const QString &path, QObject *parent)
    : SyncController(QDBusConnection::sessionBus(), path, parent)
{
}

SyncController::SyncController(const QDBusConnection &bus, const QString &path, QObject *parent)
    : QObject(parent)
    , m_bus(bus)
    , m_path(path)
    , m_iface(new OrgKonedriveSync1Interface(ServiceName, path, bus, this))
    , m_watcher(new QDBusServiceWatcher(ServiceName, bus, QDBusServiceWatcher::WatchForOwnerChange, this))
    , m_transfers(new TransferModel(this))
    , m_activity(new ActivityModel(this))
    , m_conflicts(new ConflictModel(this))
    , m_uploads(new TransferModel(this))
    , m_sampler(new QTimer(this))
    , m_notUploadedSoon(new QTimer(this))
{
    registerKonedriveSyncTypes();
    // The counts are coalesced to a few changes a second (up to 4, during a
    // bulk upload): what is kept back is read at most once a second, so its
    // refresh never piles up behind the daemon's signals.
    m_notUploadedSoon->setSingleShot(true);
    connect(m_notUploadedSoon, &QTimer::timeout, this, &SyncController::loadNotUploaded);
    // The charts' history: one sample a second, whether or not the daemon said anything,
    // so that an idle line decays to 0.
    for (auto &history : m_history) {
        history.reserve(HistoryLength);
        for (int i = 0; i < HistoryLength; ++i) {
            history.append(0.0);
        }
    }
    m_sampler->setInterval(1000);
    connect(m_sampler, &QTimer::timeout, this, &SyncController::sampleHistory);
    m_sampler->start();
    m_bus.connect(ServiceName,
                  m_path,
                  QStringLiteral("org.freedesktop.DBus.Properties"),
                  QStringLiteral("PropertiesChanged"),
                  this,
                  SLOT(onPropertiesChanged(QString, QVariantMap, QStringList)));
    m_bus.connect(ServiceName, m_path, InterfaceName, QStringLiteral("ActivityAdded"), this, SLOT(onActivityAdded(qlonglong, QString, QString, QString)));
    connect(m_watcher, &QDBusServiceWatcher::serviceOwnerChanged, this, [this](const QString &, const QString &, const QString &newOwner) {
        if (newOwner.isEmpty()) {
            setServiceAvailable(false);
            // M8: nothing will update these again until the daemon is back;
            // a stale row would otherwise look like a download still going.
            m_transfers->setTransfers({});
            m_downloadSpeed = m_uploadSpeed = 0;
            m_activeDownloads = m_activeUploads = 0;
            m_largeTransfers = m_retryAfter = 0;
            m_uploads->setTransfers({});
            m_notUploadedKnown = false;
        } else {
            fetchAll();
        }
    });
    fetchAll();
}

void SyncController::fetchAll()
{
    auto message = QDBusMessage::createMethodCall(ServiceName, m_path, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("GetAll"));
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
    // A structured value inside a{sv} arrives as a QDBusArgument.
    const auto transfers = [](const QVariant &value) {
        return value.canConvert<QDBusArgument>() ? qdbus_cast<KonedriveTransferList>(value.value<QDBusArgument>()) : value.value<KonedriveTransferList>();
    };
    if (const auto it = p.constFind(QLatin1String("Transfers")); it != p.constEnd()) {
        m_transfers->setTransfers(transfers(*it));
    }
    if (const auto it = p.constFind(QLatin1String("Uploads")); it != p.constEnd()) {
        m_uploads->setTransfers(transfers(*it));
    }
    if (const auto it = p.constFind(QLatin1String("PendingCount")); it != p.constEnd()) {
        m_pendingCount = it->toUInt();
    }
    number("PendingBytes", m_pendingBytes);
    if (const auto it = p.constFind(QLatin1String("BlockedCount")); it != p.constEnd()) {
        m_blockedCount = it->toUInt();
    }
    if (const auto it = p.constFind(QLatin1String("HeldCount")); it != p.constEnd()) {
        m_heldCount = it->toUInt();
    }
    if (const auto it = p.constFind(QLatin1String("Paused")); it != p.constEnd()) {
        m_paused = it->toBool();
    }
    if (const auto it = p.constFind(QLatin1String("PausedUntil")); it != p.constEnd()) {
        m_pausedUntil = it->toLongLong();
    }
    if (const auto it = p.constFind(QLatin1String("IgnorePatterns")); it != p.constEnd()) {
        m_ignorePatterns = it->toStringList();
    }
    text("MachineName", m_machineName);
    number("DownloadSpeed", m_downloadSpeed);
    number("UploadSpeed", m_uploadSpeed);
    const auto count = [&p](const char *key, uint &field) {
        if (const auto it = p.constFind(QLatin1String(key)); it != p.constEnd()) {
            field = it->toUInt();
        }
    };
    count("ActiveDownloads", m_activeDownloads);
    count("ActiveUploads", m_activeUploads);
    count("PoolSize", m_poolSize);
    count("PoolCeiling", m_poolCeiling);
    count("LargeTransfers", m_largeTransfers);
    count("LargeLimit", m_largeLimit);
    count("RetryAfter", m_retryAfter);
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

void SyncController::sampleHistory()
{
    const double now[4] = {double(m_downloadSpeed), double(m_uploadSpeed), double(m_activeDownloads), double(m_activeUploads)};
    bool changed = false;
    for (int i = 0; i < 4; ++i) {
        // An idle history that stays idle is not news.
        const auto nonZero = [](const QVariant &sample) {
            return sample.toDouble() != 0.0;
        };
        if (now[i] != 0.0 || std::any_of(m_history[i].cbegin(), m_history[i].cend(), nonZero)) {
            changed = true;
        }
    }
    if (!changed) {
        return;
    }
    for (int i = 0; i < 4; ++i) {
        m_history[i].removeFirst();
        m_history[i].append(now[i]);
    }
    Q_EMIT historyChanged();
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
    auto message = QDBusMessage::createMethodCall(ServiceName, m_path, InterfaceName, QStringLiteral("RegisterRoot"));
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
    const auto message = QDBusMessage::createMethodCall(ServiceName, m_path, InterfaceName, QStringLiteral("UnregisterRoot"));
    call(m_bus.asyncCall(message, std::numeric_limits<int>::max()), {}, [this](const QDBusError &error) {
        if (error.name() != QLatin1String("org.konedrive.Error.PendingUploads")) {
            return false;
        }
        setActionError(i18n("The folder was not forgotten: changes made on this computer have not been uploaded yet, and forgetting it now would lose them. Wait until they are uploaded, or turn uploading off for this account and choose not to upload them; then forget it."));
        return true;
    });
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
    const auto message = QDBusMessage::createMethodCall(ServiceName, m_path, InterfaceName, QStringLiteral("FreeUpSpace"));
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

void SyncController::pause(uint seconds)
{
    call(m_iface->Pause(seconds));
}

void SyncController::resume()
{
    call(m_iface->Resume());
}

void SyncController::setIgnorePatterns(const QStringList &patterns)
{
    // It runs a scan of the whole folder before it answers: no timeout.
    auto message = QDBusMessage::createMethodCall(ServiceName, m_path, InterfaceName, QStringLiteral("SetIgnorePatterns"));
    message << patterns;
    call(m_bus.asyncCall(message, std::numeric_limits<int>::max()));
}

void SyncController::addIgnorePattern(const QString &pattern)
{
    const QString trimmed = pattern.trimmed();
    if (trimmed.isEmpty() || m_ignorePatterns.contains(trimmed)) {
        return;
    }
    setIgnorePatterns(m_ignorePatterns + QStringList{trimmed});
}

void SyncController::removeIgnorePattern(const QString &pattern)
{
    QStringList patterns = m_ignorePatterns;
    if (patterns.removeAll(pattern) > 0) {
        setIgnorePatterns(patterns);
    }
}

void SyncController::confirmDeletes()
{
    call(m_iface->ConfirmDeletes());
}

void SyncController::restoreDeletes()
{
    call(m_iface->RestoreDeletes());
}

void SyncController::loadNotUploaded()
{
    if (!m_serviceAvailable || m_notUploadedSoon->isActive()) {
        return;
    }
    // With thousands of changes kept back the daemon reads its whole outbox
    // for this: once a second at most, however often the counts move.
    if (m_notUploadedLast.isValid() && m_notUploadedLast.elapsed() < 1000) {
        m_notUploadedSoon->start(int(1000 - m_notUploadedLast.elapsed()));
        return;
    }
    m_notUploadedLast.start();
    // Refused Unsupported for a folder not connected to OneDrive, and
    // UnknownMethod by an older daemon: nothing is shown as kept back.
    quietly(m_iface->NotUploadedSummary(), [this](const QDBusPendingCall &pending) {
        const QDBusPendingReply<KonedriveKeptBackList> reply = pending;
        m_notUploadedSummary.clear();
        m_blockedBytes = 0;
        QSet<QString> reasons;
        for (const KonedriveKeptBack &row : reply.value()) {
            reasons.insert(row.reason);
            if (row.group == QLatin1String("one-action") || row.group == QLatin1String("per-file")) {
                m_blockedBytes += row.bytes;
            }
            m_notUploadedSummary << QVariantMap{{QStringLiteral("group"), row.group},
                                                {QStringLiteral("reason"), row.reason},
                                                {QStringLiteral("count"), row.count},
                                                {QStringLiteral("bytes"), row.bytes},
                                                {QStringLiteral("why"), uploadReasonText(row.reason)}};
        }
        m_notUploadedKnown = true;
        Q_EMIT notUploadedChanged();
        // A reason gone from the summary has no files left to show.
        bool dropped = false;
        for (auto it = m_notUploadedFiles.begin(); it != m_notUploadedFiles.end();) {
            if (reasons.contains(it.key())) {
                ++it;
            } else {
                it = m_notUploadedFiles.erase(it);
                dropped = true;
            }
        }
        if (dropped) {
            Q_EMIT notUploadedFilesChanged();
        }
    });
    for (const QString &reason : std::as_const(m_filesShown)) {
        loadNotUploadedFiles(reason);
    }
}

void SyncController::setNotUploadedFilesShown(const QString &reason, bool shown)
{
    if (!shown) {
        m_filesShown.remove(reason);
        return;
    }
    m_filesShown.insert(reason);
    loadNotUploadedFiles(reason);
}

void SyncController::loadNotUploadedFiles(const QString &reason)
{
    if (!m_serviceAvailable) {
        return;
    }
    quietly(m_iface->NotUploadedFiles(reason, PerFileCap), [this, reason](const QDBusPendingCall &pending) {
        const QDBusPendingReply<KonedriveSkippedList, uint> reply = pending;
        QVariantList items;
        for (const KonedriveSkippedItem &item : reply.argumentAt<0>()) {
            items << QVariantMap{{QStringLiteral("path"), item.path}, {QStringLiteral("reason"), item.reason}, {QStringLiteral("why"), uploadReasonText(item.reason)}};
        }
        m_notUploadedFiles.insert(reason, QVariantMap{{QStringLiteral("items"), items}, {QStringLiteral("total"), reply.argumentAt<1>()}});
        Q_EMIT notUploadedFilesChanged();
    });
}

void SyncController::showBoth(const QString &first, const QString &second)
{
    KIO::highlightInFileManager({QUrl::fromLocalFile(first), QUrl::fromLocalFile(second)});
}

