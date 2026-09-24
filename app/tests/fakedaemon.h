#pragma once

// A stand-in for konedrived on the tests' private session bus: Account1 and
// Sync1 on one object, as the daemon has them. Each interface is an adaptor,
// since one D-Bus path holds one object. What the controllers ask is logged
// in `calls`; properties change through set(), which also emits
// PropertiesChanged the way the daemon does.

#include "accountcontroller.h"
#include "synccontroller.h"
#include "synctypes.h"

#include <QDBusAbstractAdaptor>
#include <QDBusConnection>
#include <QDBusMessage>
#include <QStringList>
#include <QVariantMap>

#include <memory>

namespace fake
{
inline const QString BusName = QStringLiteral("fake-daemon");

/// The fake's own connection, so the controllers under test (on the
/// default session connection) talk to it over the bus.
inline QDBusConnection bus()
{
    return QDBusConnection::connectToBus(QDBusConnection::SessionBus, BusName);
}

inline void propertiesChanged(QDBusConnection connection, const QString &interfaceName, const QVariantMap &changes)
{
    auto signal = QDBusMessage::createSignal(SyncController::ObjectPath, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("PropertiesChanged"));
    signal << interfaceName << changes << QStringList();
    connection.send(signal);
}
}

class FakeAccount1 : public QDBusAbstractAdaptor
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.Account1")
    Q_PROPERTY(QString State READ state)
    Q_PROPERTY(QString LastError READ lastError)
    Q_PROPERTY(QString ClientId READ clientId)
    Q_PROPERTY(QString DisplayName READ displayName)
    Q_PROPERTY(QString Email READ email)
    Q_PROPERTY(qulonglong QuotaUsed READ quotaUsed)
    Q_PROPERTY(qulonglong QuotaTotal READ quotaTotal)

public:
    FakeAccount1(QObject *parent, const QDBusConnection &bus)
        : QDBusAbstractAdaptor(parent)
        , m_bus(bus)
    {
    }

    QString state() const { return m_properties.value(QStringLiteral("State")).toString(); }
    QString lastError() const { return m_properties.value(QStringLiteral("LastError")).toString(); }
    QString clientId() const { return m_properties.value(QStringLiteral("ClientId")).toString(); }
    QString displayName() const { return m_properties.value(QStringLiteral("DisplayName")).toString(); }
    QString email() const { return m_properties.value(QStringLiteral("Email")).toString(); }
    qulonglong quotaUsed() const { return m_properties.value(QStringLiteral("QuotaUsed")).toULongLong(); }
    qulonglong quotaTotal() const { return m_properties.value(QStringLiteral("QuotaTotal")).toULongLong(); }

    void set(const QVariantMap &changes)
    {
        for (auto it = changes.cbegin(); it != changes.cend(); ++it) {
            m_properties.insert(it.key(), it.value());
        }
        fake::propertiesChanged(m_bus, AccountController::InterfaceName, changes);
    }

    QStringList calls;

public Q_SLOTS:
    void SignOut()
    {
        calls << QStringLiteral("SignOut");
        set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
    }
    void RefreshAccountInfo() { calls << QStringLiteral("RefreshAccountInfo"); }

private:
    QDBusConnection m_bus;
    QVariantMap m_properties{
        {QStringLiteral("State"), QStringLiteral("signed-out")},
        {QStringLiteral("LastError"), QString()},
        {QStringLiteral("ClientId"), QString()},
        {QStringLiteral("DisplayName"), QString()},
        {QStringLiteral("Email"), QString()},
        {QStringLiteral("QuotaUsed"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("QuotaTotal"), QVariant::fromValue<qulonglong>(0)},
    };
};

class FakeSync1 : public QDBusAbstractAdaptor
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.Sync1")
    Q_PROPERTY(QString RootPath READ rootPath)
    Q_PROPERTY(QString RootState READ rootState)
    Q_PROPERTY(QString RootSource READ rootSource)
    Q_PROPERTY(QString LastError READ lastError)
    Q_PROPERTY(QString HelperState READ helperState)
    Q_PROPERTY(qulonglong ItemsListed READ itemsListed)
    Q_PROPERTY(qulonglong ItemsPlaced READ itemsPlaced)
    Q_PROPERTY(qulonglong SkippedCount READ skippedCount)
    Q_PROPERTY(qlonglong LastChecked READ lastChecked)
    Q_PROPERTY(qulonglong LocalBytes READ localBytes)
    Q_PROPERTY(uint ConflictCount READ conflictCount)
    Q_PROPERTY(uint PinnedCount READ pinnedCount)
    Q_PROPERTY(KonedriveTransferList Transfers READ transfers)

public:
    FakeSync1(QObject *parent, const QDBusConnection &bus)
        : QDBusAbstractAdaptor(parent)
        , m_bus(bus)
    {
    }

    QString rootPath() const { return m_properties.value(QStringLiteral("RootPath")).toString(); }
    QString rootState() const { return m_properties.value(QStringLiteral("RootState")).toString(); }
    QString rootSource() const { return m_properties.value(QStringLiteral("RootSource")).toString(); }
    QString lastError() const { return m_properties.value(QStringLiteral("LastError")).toString(); }
    QString helperState() const { return m_properties.value(QStringLiteral("HelperState")).toString(); }
    qulonglong itemsListed() const { return m_properties.value(QStringLiteral("ItemsListed")).toULongLong(); }
    qulonglong itemsPlaced() const { return m_properties.value(QStringLiteral("ItemsPlaced")).toULongLong(); }
    qulonglong skippedCount() const { return m_properties.value(QStringLiteral("SkippedCount")).toULongLong(); }
    qlonglong lastChecked() const { return m_properties.value(QStringLiteral("LastChecked")).toLongLong(); }
    qulonglong localBytes() const { return m_properties.value(QStringLiteral("LocalBytes")).toULongLong(); }
    uint conflictCount() const { return m_properties.value(QStringLiteral("ConflictCount")).toUInt(); }
    uint pinnedCount() const { return m_properties.value(QStringLiteral("PinnedCount")).toUInt(); }
    KonedriveTransferList transfers() const { return m_transfers; }

    void set(const QVariantMap &changes)
    {
        for (auto it = changes.cbegin(); it != changes.cend(); ++it) {
            m_properties.insert(it.key(), it.value());
        }
        fake::propertiesChanged(m_bus, SyncController::InterfaceName, changes);
    }

    void setTransfers(const KonedriveTransferList &transfers)
    {
        m_transfers = transfers;
        fake::propertiesChanged(m_bus, SyncController::InterfaceName, {{QStringLiteral("Transfers"), QVariant::fromValue(transfers)}});
    }

    /// Records an event in RecentActivity() and emits ActivityAdded, as the daemon does.
    void activity(qint64 time, const QString &kind, const QString &path, const QString &detail)
    {
        log.prepend({time, kind, path, detail});
        signalOnly(time, kind, path, detail);
    }

    /// Emits ActivityAdded without storing the event: a daemon that signals
    /// before its store has it.
    void signalOnly(qint64 time, const QString &kind, const QString &path, const QString &detail)
    {
        auto signal = QDBusMessage::createSignal(SyncController::ObjectPath, SyncController::InterfaceName, QStringLiteral("ActivityAdded"));
        signal << time << kind << path << detail;
        m_bus.send(signal);
    }

    /// Answers the held RecentActivity() call with the log as it is now.
    void releaseActivity()
    {
        m_bus.send(m_heldActivity.createReply(QVariant::fromValue(log.mid(0, int(m_heldLimit)))));
        m_heldActivity = QDBusMessage();
    }

    /// Answers the held FreeUpSpace() call.
    void finishFreeUp()
    {
        m_bus.send(m_heldFreeUp.createReply({QVariant::fromValue(freedFiles), QVariant::fromValue(freedBytes), QVariant::fromValue(busyFiles)}));
        m_heldFreeUp = QDBusMessage();
    }

    QStringList calls;
    bool helperMissing = false;
    /// Hold these calls unanswered (until releaseActivity/finishFreeUp; Refresh for ever).
    bool holdActivity = false;
    bool holdFreeUp = false;
    bool holdRefresh = false;
    /// RecentActivity(), newest first.
    KonedriveActivityList log;
    KonedriveConflictList conflictList;
    uint freedFiles = 0;
    qulonglong freedBytes = 0;
    uint busyFiles = 0;

public Q_SLOTS:
    void RegisterRoot(const QString &path, const QDBusMessage &message)
    {
        calls << QStringLiteral("RegisterRoot:") + path;
        if (helperMissing) {
            message.setDelayedReply(true);
            m_bus.send(message.createErrorReply(QStringLiteral("org.konedrive.Error.NoHelper"), QStringLiteral("the konedrive helper is not connected")));
            return;
        }
        set({{QStringLiteral("RootPath"), path}, {QStringLiteral("RootState"), QStringLiteral("listing")}, {QStringLiteral("RootSource"), QStringLiteral("onedrive")}});
    }
    void RegisterRootWithoutInterception(const QString &path)
    {
        calls << QStringLiteral("RegisterRootWithoutInterception:") + path;
        set({{QStringLiteral("RootPath"), path}, {QStringLiteral("RootState"), QStringLiteral("no-interception")}, {QStringLiteral("RootSource"), QStringLiteral("onedrive")}});
    }
    void UnregisterRoot()
    {
        calls << QStringLiteral("UnregisterRoot");
        set({{QStringLiteral("RootPath"), QString()}, {QStringLiteral("RootState"), QStringLiteral("none")}, {QStringLiteral("RootSource"), QString()}});
    }
    void Refresh(const QDBusMessage &message)
    {
        calls << QStringLiteral("Refresh");
        if (holdRefresh) {
            message.setDelayedReply(true); // never answered
        }
    }
    KonedriveSkippedList Skipped() { return {{QStringLiteral("/home/u/OneDrive/Personal Vault"), QStringLiteral("personal-vault")}}; }
    KonedriveActivityList RecentActivity(uint limit, const QDBusMessage &message)
    {
        calls << QStringLiteral("RecentActivity:") + QString::number(limit);
        if (holdActivity) {
            message.setDelayedReply(true);
            m_heldActivity = message;
            m_heldLimit = limit;
            return {};
        }
        return log.mid(0, int(limit));
    }
    KonedriveConflictList Conflicts()
    {
        calls << QStringLiteral("Conflicts");
        return conflictList;
    }
    /// Removes the row and answers; it does not emit ConflictCount, so a
    /// test sees whether the window asks again on its own.
    void DismissConflict(const QString &rescued, const QDBusMessage &message)
    {
        calls << QStringLiteral("DismissConflict:") + rescued;
        const auto before = conflictList.size();
        conflictList.removeIf([&rescued](const KonedriveConflict &c) {
            return c.rescued == rescued;
        });
        if (conflictList.size() == before) {
            message.setDelayedReply(true);
            m_bus.send(message.createErrorReply(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("no conflict at ") + rescued));
        }
    }
    /// Answers (u files, t bytes, u busy) by hand, so that it can be held.
    void FreeUpSpace(const QDBusMessage &message)
    {
        calls << QStringLiteral("FreeUpSpace");
        message.setDelayedReply(true);
        m_heldFreeUp = message;
        if (!holdFreeUp) {
            finishFreeUp();
        }
    }

private:
    QDBusConnection m_bus;
    KonedriveTransferList m_transfers;
    QDBusMessage m_heldActivity;
    uint m_heldLimit = 0;
    QDBusMessage m_heldFreeUp;
    QVariantMap m_properties{
        {QStringLiteral("RootPath"), QString()},
        {QStringLiteral("RootState"), QStringLiteral("none")},
        {QStringLiteral("RootSource"), QString()},
        {QStringLiteral("LastError"), QString()},
        {QStringLiteral("HelperState"), QStringLiteral("connected")},
        {QStringLiteral("ItemsListed"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("ItemsPlaced"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("SkippedCount"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("LastChecked"), QVariant::fromValue<qlonglong>(0)},
        {QStringLiteral("LocalBytes"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(0)},
        {QStringLiteral("PinnedCount"), QVariant::fromValue<uint>(0)},
    };
};

/// The daemon's one object, carrying both interfaces.
class FakeDaemon : public QObject
{
    Q_OBJECT

public:
    FakeDaemon()
        : account(new FakeAccount1(this, fake::bus()))
        , sync(new FakeSync1(this, fake::bus()))
    {
        // Before anything is sent: Transfers is an a(stt).
        registerKonedriveSyncTypes();
    }

    /// Takes the daemon's name on the private bus. Returns false on failure.
    bool start()
    {
        auto connection = fake::bus();
        return connection.registerObject(SyncController::ObjectPath, this, QDBusConnection::ExportAdaptors)
            && connection.registerService(SyncController::ServiceName);
    }

    /// Gives the name up, as a daemon that exits does.
    void stop()
    {
        auto connection = fake::bus();
        connection.unregisterService(SyncController::ServiceName);
        connection.unregisterObject(SyncController::ObjectPath);
    }

    FakeAccount1 *account;
    FakeSync1 *sync;
};

/// A system tray's StatusNotifierWatcher with a host registered, as Plasma
/// runs one. Tray icons register with it; `items` records their names.
class FakeTrayWatcher : public QObject
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.kde.StatusNotifierWatcher")
    Q_PROPERTY(bool IsStatusNotifierHostRegistered READ hostRegistered)
    Q_PROPERTY(int ProtocolVersion READ protocolVersion)
    Q_PROPERTY(QStringList RegisteredStatusNotifierItems READ registeredItems)

public:
    static inline const QString Service = QStringLiteral("org.kde.StatusNotifierWatcher");
    static inline const QString Path = QStringLiteral("/StatusNotifierWatcher");

    ~FakeTrayWatcher() override
    {
        if (m_started) {
            stop();
        }
    }

    bool start(QDBusConnection connection)
    {
        m_connection = connection;
        m_started = true;
        return connection.registerObject(Path, this, QDBusConnection::ExportAllSlots | QDBusConnection::ExportAllProperties | QDBusConnection::ExportAllSignals)
            && connection.registerService(Service);
    }

    void stop()
    {
        m_connection.unregisterService(Service);
        m_connection.unregisterObject(Path);
        m_started = false;
    }

    bool hostRegistered() const { return true; }
    int protocolVersion() const { return 0; }
    QStringList registeredItems() const { return items; }

    QStringList items;

public Q_SLOTS:
    void RegisterStatusNotifierItem(const QString &service, const QDBusMessage &message)
    {
        // An item may pass its object path instead of a name; the sender is the name then.
        items << (service.startsWith(QLatin1Char('/')) ? message.service() : service);
        Q_EMIT StatusNotifierItemRegistered(items.constLast());
    }
    void RegisterStatusNotifierHost(const QString &) { }

Q_SIGNALS:
    void StatusNotifierItemRegistered(const QString &service);
    void StatusNotifierItemUnregistered(const QString &service);
    void StatusNotifierHostRegistered();
    void StatusNotifierHostUnregistered();

private:
    QDBusConnection m_connection{QString()};
    bool m_started = false;
};
