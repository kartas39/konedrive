#pragma once

// A stand-in for konedrived on the tests' private session bus, shaped as the
// daemon serves it: the manager at /org/konedrive/Accounts with Accounts1
// (dbus/org.konedrive.Accounts1.xml), and each account at
// /org/konedrive/Accounts/<id> with Account1 and Sync1. Each interface is an
// adaptor, since one D-Bus path holds one object. What the controllers ask is
// logged in `calls`; properties change through set(), which also emits
// PropertiesChanged the way the daemon does.

#include "accountcontroller.h"
#include "daemoncontroller.h"
#include "synccontroller.h"
#include "synctypes.h"

#include <QDBusAbstractAdaptor>
#include <QDBusConnection>
#include <QDBusMessage>
#include <QDBusObjectPath>
#include <QStringList>
#include <QVariantMap>

#include <memory>

namespace fake
{
inline const QString BusName = QStringLiteral("fake-daemon");
inline const QString ManagerPath = QStringLiteral("/org/konedrive/Accounts");

/// The n-th account's id (from 1): 12 hex characters, as the daemon's are.
inline QString idFor(int n)
{
    return QStringLiteral("%1").arg(n, 12, 16, QLatin1Char('0'));
}

inline QString accountPath(const QString &id)
{
    return ManagerPath + QLatin1Char('/') + id;
}

/// The path of FakeDaemon's first account.
inline const QString FirstAccount = accountPath(idFor(1));

/// The fake's own connection, so the controllers under test (on the
/// default session connection) talk to it over the bus.
inline QDBusConnection bus()
{
    return QDBusConnection::connectToBus(QDBusConnection::SessionBus, BusName);
}

inline void propertiesChanged(QDBusConnection connection, const QString &path, const QString &interfaceName, const QVariantMap &changes)
{
    auto signal = QDBusMessage::createSignal(path, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("PropertiesChanged"));
    signal << interfaceName << changes << QStringList();
    connection.send(signal);
}
}

class FakeAccount1 : public QDBusAbstractAdaptor
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.Account1")
    Q_PROPERTY(QString Id READ id)
    Q_PROPERTY(QString Label READ label)
    Q_PROPERTY(QString Mode READ mode)
    Q_PROPERTY(QString State READ state)
    Q_PROPERTY(QString LastError READ lastError)
    Q_PROPERTY(QString DisplayName READ displayName)
    Q_PROPERTY(QString Email READ email)
    Q_PROPERTY(qulonglong QuotaUsed READ quotaUsed)
    Q_PROPERTY(qulonglong QuotaTotal READ quotaTotal)

public:
    FakeAccount1(QObject *parent, const QDBusConnection &bus, const QString &path, const QString &id, const QString &label)
        : QDBusAbstractAdaptor(parent)
        , m_bus(bus)
        , m_path(path)
    {
        m_properties.insert(QStringLiteral("Id"), id);
        m_properties.insert(QStringLiteral("Label"), label);
    }

    QString id() const { return m_properties.value(QStringLiteral("Id")).toString(); }
    QString label() const { return m_properties.value(QStringLiteral("Label")).toString(); }
    QString mode() const { return m_properties.value(QStringLiteral("Mode")).toString(); }
    QString state() const { return m_properties.value(QStringLiteral("State")).toString(); }
    QString lastError() const { return m_properties.value(QStringLiteral("LastError")).toString(); }
    QString displayName() const { return m_properties.value(QStringLiteral("DisplayName")).toString(); }
    QString email() const { return m_properties.value(QStringLiteral("Email")).toString(); }
    qulonglong quotaUsed() const { return m_properties.value(QStringLiteral("QuotaUsed")).toULongLong(); }
    qulonglong quotaTotal() const { return m_properties.value(QStringLiteral("QuotaTotal")).toULongLong(); }

    void set(const QVariantMap &changes)
    {
        for (auto it = changes.cbegin(); it != changes.cend(); ++it) {
            m_properties.insert(it.key(), it.value());
        }
        fake::propertiesChanged(m_bus, m_path, AccountController::InterfaceName, changes);
    }

    /// The sign-in a switch to read-write waits for, granted.
    void grantReadWrite() { set({{QStringLiteral("Mode"), QStringLiteral("read-write")}}); }

    QStringList calls;
    /// SetMode("read-write") gets through the development gate, which refuses by default.
    bool gateOpen = false;
    /// Changes waiting to be uploaded, for SetMode("read-only").
    uint pendingUploads = 0;

public Q_SLOTS:
    QString BeginSignIn()
    {
        calls << QStringLiteral("BeginSignIn");
        set({{QStringLiteral("State"), QStringLiteral("signing-in")}});
        return QStringLiteral("https://login.example/authorize?account=") + id();
    }
    void CancelSignIn()
    {
        calls << QStringLiteral("CancelSignIn");
        // A switch to read-write's sign-in is given up with the account still signed in.
        if (state() == QLatin1String("signing-in")) {
            set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
        }
    }
    void SignOut()
    {
        calls << QStringLiteral("SignOut");
        set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
    }
    void RefreshAccountInfo() { calls << QStringLiteral("RefreshAccountInfo"); }
    void SetLabel(const QString &label, const QDBusMessage &message)
    {
        calls << QStringLiteral("SetLabel:") + label;
        if (label.trimmed().isEmpty() || label.contains(QLatin1Char('/'))) {
            message.setDelayedReply(true);
            m_bus.send(message.createErrorReply(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("not a label: ") + label));
            return;
        }
        set({{QStringLiteral("Label"), label.trimmed()}});
    }
    /// As the daemon's: the gate first, then the sign-in state, for read-write, which
    /// answers a sign-in URL (grantReadWrite() then ends it); PendingUploads for
    /// read-only while `pendingUploads` is not 0, unless forced, which drops them.
    QString SetMode(const QString &mode, bool force, const QDBusMessage &message)
    {
        calls << QStringLiteral("SetMode:%1:%2").arg(mode, force ? QStringLiteral("force") : QStringLiteral("no-force"));
        const auto refuse = [&](const QString &name, const QString &why) {
            message.setDelayedReply(true);
            m_bus.send(message.createErrorReply(name, why));
            return QString();
        };
        if (mode == QLatin1String("read-write")) {
            if (!gateOpen) {
                return refuse(QStringLiteral("org.konedrive.Error.WritesNotAllowed"), QStringLiteral("DAEMON-GATE-WORDS"));
            }
            if (state() != QLatin1String("signed-in")) {
                return refuse(QStringLiteral("org.konedrive.Error.NotSignedIn"), QStringLiteral("sign in first; then switch the account to read-write"));
            }
            set({{QStringLiteral("LastError"), QString()}});
            return QStringLiteral("https://login.example/authorize?scope=Files.ReadWrite&account=") + id();
        }
        if (mode == QLatin1String("read-only")) {
            if (pendingUploads > 0 && !force) {
                return refuse(QStringLiteral("org.konedrive.Error.PendingUploads"),
                              QStringLiteral("%1 changes made here have not been uploaded yet").arg(pendingUploads));
            }
            pendingUploads = 0;
            set({{QStringLiteral("Mode"), QStringLiteral("read-only")}});
            return QString();
        }
        return refuse(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("unknown mode ") + mode);
    }

private:
    QDBusConnection m_bus;
    QString m_path;
    QVariantMap m_properties{
        {QStringLiteral("Mode"), QStringLiteral("read-only")},
        {QStringLiteral("State"), QStringLiteral("signed-out")},
        {QStringLiteral("LastError"), QString()},
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
    Q_PROPERTY(qulonglong ItemsListed READ itemsListed)
    Q_PROPERTY(qulonglong ItemsPlaced READ itemsPlaced)
    Q_PROPERTY(qulonglong SkippedCount READ skippedCount)
    Q_PROPERTY(qlonglong LastChecked READ lastChecked)
    Q_PROPERTY(qulonglong LocalBytes READ localBytes)
    Q_PROPERTY(uint ConflictCount READ conflictCount)
    Q_PROPERTY(uint PinnedCount READ pinnedCount)
    Q_PROPERTY(KonedriveTransferList Transfers READ transfers)
    Q_PROPERTY(uint PendingCount READ pendingCount)
    Q_PROPERTY(qulonglong PendingBytes READ pendingBytes)
    Q_PROPERTY(uint BlockedCount READ blockedCount)
    Q_PROPERTY(uint HeldCount READ heldCount)
    Q_PROPERTY(KonedriveTransferList Uploads READ uploads)
    Q_PROPERTY(bool Paused READ paused)
    Q_PROPERTY(qlonglong PausedUntil READ pausedUntil)
    Q_PROPERTY(QStringList IgnorePatterns READ ignorePatterns)
    Q_PROPERTY(QString MachineName READ machineName)

public:
    FakeSync1(QObject *parent, const QDBusConnection &bus, const QString &path)
        : QDBusAbstractAdaptor(parent)
        , m_bus(bus)
        , m_path(path)
    {
    }

    QString rootPath() const { return m_properties.value(QStringLiteral("RootPath")).toString(); }
    QString rootState() const { return m_properties.value(QStringLiteral("RootState")).toString(); }
    QString rootSource() const { return m_properties.value(QStringLiteral("RootSource")).toString(); }
    QString lastError() const { return m_properties.value(QStringLiteral("LastError")).toString(); }
    qulonglong itemsListed() const { return m_properties.value(QStringLiteral("ItemsListed")).toULongLong(); }
    qulonglong itemsPlaced() const { return m_properties.value(QStringLiteral("ItemsPlaced")).toULongLong(); }
    qulonglong skippedCount() const { return m_properties.value(QStringLiteral("SkippedCount")).toULongLong(); }
    qlonglong lastChecked() const { return m_properties.value(QStringLiteral("LastChecked")).toLongLong(); }
    qulonglong localBytes() const { return m_properties.value(QStringLiteral("LocalBytes")).toULongLong(); }
    uint conflictCount() const { return m_properties.value(QStringLiteral("ConflictCount")).toUInt(); }
    uint pinnedCount() const { return m_properties.value(QStringLiteral("PinnedCount")).toUInt(); }
    KonedriveTransferList transfers() const { return m_transfers; }
    uint pendingCount() const { return m_properties.value(QStringLiteral("PendingCount")).toUInt(); }
    qulonglong pendingBytes() const { return m_properties.value(QStringLiteral("PendingBytes")).toULongLong(); }
    uint blockedCount() const { return m_properties.value(QStringLiteral("BlockedCount")).toUInt(); }
    uint heldCount() const { return m_properties.value(QStringLiteral("HeldCount")).toUInt(); }
    KonedriveTransferList uploads() const { return m_uploads; }
    bool paused() const { return m_properties.value(QStringLiteral("Paused")).toBool(); }
    qlonglong pausedUntil() const { return m_properties.value(QStringLiteral("PausedUntil")).toLongLong(); }
    QStringList ignorePatterns() const { return m_properties.value(QStringLiteral("IgnorePatterns")).toStringList(); }
    QString machineName() const { return m_properties.value(QStringLiteral("MachineName")).toString(); }

    void set(const QVariantMap &changes)
    {
        for (auto it = changes.cbegin(); it != changes.cend(); ++it) {
            m_properties.insert(it.key(), it.value());
        }
        fake::propertiesChanged(m_bus, m_path, SyncController::InterfaceName, changes);
    }

    void setTransfers(const KonedriveTransferList &transfers)
    {
        m_transfers = transfers;
        fake::propertiesChanged(m_bus, m_path, SyncController::InterfaceName, {{QStringLiteral("Transfers"), QVariant::fromValue(transfers)}});
    }

    /// The mass-delete guard trips: `count` removals of `path`'s kind are
    /// held, in the outbox and in HeldCount.
    void holdDeletes(const QString &path, uint count)
    {
        for (uint i = 0; i < count; ++i) {
            outboxRows << KonedriveOutboxRow{100 + i, QStringLiteral("delete"), path + QString::number(i), QStringLiteral("held"), 0, 0, QStringLiteral("mass-delete"), 0};
        }
        set({{QStringLiteral("HeldCount"), QVariant::fromValue<uint>(heldCount() + count)}});
    }

    void setUploads(const KonedriveTransferList &uploads)
    {
        m_uploads = uploads;
        fake::propertiesChanged(m_bus, m_path, SyncController::InterfaceName, {{QStringLiteral("Uploads"), QVariant::fromValue(uploads)}});
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
        auto signal = QDBusMessage::createSignal(m_path, SyncController::InterfaceName, QStringLiteral("ActivityAdded"));
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
    /// Outbox(), oldest first; Confirm/RestoreDeletes act on its "held" rows
    /// and set HeldCount to 0. holdDeletes() holds some, as the guard does.
    KonedriveOutboxList outboxRows;
    /// NotUploaded().
    KonedriveSkippedList notUploadedList;
    /// Pause(seconds) ends at pauseNow + seconds.
    qint64 pauseNow = 1758700000;

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
    KonedriveOutboxList Outbox(uint limit)
    {
        calls << QStringLiteral("Outbox");
        return limit == 0 ? outboxRows : outboxRows.mid(0, int(limit));
    }
    void Pause(uint seconds)
    {
        calls << QStringLiteral("Pause:") + QString::number(seconds);
        set({{QStringLiteral("Paused"), true}, {QStringLiteral("PausedUntil"), QVariant::fromValue<qlonglong>(seconds == 0 ? 0 : pauseNow + seconds)}});
    }
    void Resume()
    {
        calls << QStringLiteral("Resume");
        set({{QStringLiteral("Paused"), false}, {QStringLiteral("PausedUntil"), QVariant::fromValue<qlonglong>(0)}});
    }
    void SetIgnorePatterns(const QStringList &patterns, const QDBusMessage &message)
    {
        calls << QStringLiteral("SetIgnorePatterns:") + patterns.join(QLatin1Char(','));
        for (const QString &pattern : patterns) {
            if (pattern.isEmpty() || pattern.contains(QLatin1Char('/'))) {
                message.setDelayedReply(true);
                m_bus.send(message.createErrorReply(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("not a pattern: ") + pattern));
                return;
            }
        }
        set({{QStringLiteral("IgnorePatterns"), patterns}});
    }
    uint ConfirmDeletes()
    {
        calls << QStringLiteral("ConfirmDeletes");
        uint released = 0;
        for (KonedriveOutboxRow &row : outboxRows) {
            if (row.state == QLatin1String("held")) {
                row.state = QStringLiteral("ready");
                row.reason.clear();
                ++released;
            }
        }
        set({{QStringLiteral("HeldCount"), QVariant::fromValue<uint>(0)}});
        return released;
    }
    uint RestoreDeletes()
    {
        calls << QStringLiteral("RestoreDeletes");
        const auto dropped = outboxRows.removeIf([](const KonedriveOutboxRow &row) {
            return row.state == QLatin1String("held");
        });
        set({{QStringLiteral("HeldCount"), QVariant::fromValue<uint>(0)}});
        return uint(dropped);
    }
    KonedriveSkippedList NotUploaded()
    {
        calls << QStringLiteral("NotUploaded");
        return notUploadedList;
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
    QString m_path;
    KonedriveTransferList m_transfers;
    KonedriveTransferList m_uploads;
    QDBusMessage m_heldActivity;
    uint m_heldLimit = 0;
    QDBusMessage m_heldFreeUp;
    QVariantMap m_properties{
        {QStringLiteral("RootPath"), QString()},
        {QStringLiteral("RootState"), QStringLiteral("none")},
        {QStringLiteral("RootSource"), QString()},
        {QStringLiteral("LastError"), QString()},
        {QStringLiteral("ItemsListed"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("ItemsPlaced"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("SkippedCount"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("LastChecked"), QVariant::fromValue<qlonglong>(0)},
        {QStringLiteral("LocalBytes"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(0)},
        {QStringLiteral("PinnedCount"), QVariant::fromValue<uint>(0)},
        {QStringLiteral("PendingCount"), QVariant::fromValue<uint>(0)},
        {QStringLiteral("PendingBytes"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("BlockedCount"), QVariant::fromValue<uint>(0)},
        {QStringLiteral("HeldCount"), QVariant::fromValue<uint>(0)},
        {QStringLiteral("Paused"), false},
        {QStringLiteral("PausedUntil"), QVariant::fromValue<qlonglong>(0)},
        {QStringLiteral("IgnorePatterns"), QStringList{QStringLiteral("*.tmp"), QStringLiteral("~*")}},
        {QStringLiteral("MachineName"), QStringLiteral("fedora")},
    };
};

/// One account's object, /org/konedrive/Accounts/<id>, carrying both interfaces.
class FakeAccountObject : public QObject
{
    Q_OBJECT

public:
    FakeAccountObject(const QString &id, const QString &label, QObject *parent)
        : QObject(parent)
        , id(id)
        , path(fake::accountPath(id))
        , account(new FakeAccount1(this, fake::bus(), path, id, label))
        , sync(new FakeSync1(this, fake::bus(), path))
    {
    }

    const QString id;
    const QString path;
    FakeAccount1 *account;
    FakeSync1 *sync;
};

class FakeDaemon;

/// Accounts1 on the manager object; its methods act on the FakeDaemon.
class FakeAccounts1 : public QDBusAbstractAdaptor
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.Accounts1")
    Q_PROPERTY(QList<QDBusObjectPath> Accounts READ accounts)
    Q_PROPERTY(QString ClientId READ clientId)
    Q_PROPERTY(QString HelperState READ helperState)
    Q_PROPERTY(QString LastError READ lastError)

public:
    explicit FakeAccounts1(FakeDaemon *daemon);

    QList<QDBusObjectPath> accounts() const;
    QString clientId() const { return m_properties.value(QStringLiteral("ClientId")).toString(); }
    QString helperState() const { return m_properties.value(QStringLiteral("HelperState")).toString(); }
    QString lastError() const { return m_properties.value(QStringLiteral("LastError")).toString(); }

    void set(const QVariantMap &changes)
    {
        for (auto it = changes.cbegin(); it != changes.cend(); ++it) {
            m_properties.insert(it.key(), it.value());
        }
        fake::propertiesChanged(fake::bus(), fake::ManagerPath, DaemonController::InterfaceName, changes);
    }

    QStringList calls;
    /// Remove refuses NoHelper, as it does for an intercepted folder with no helper.
    bool refuseRemove = false;

public Q_SLOTS:
    QDBusObjectPath Add(const QString &label, const QDBusMessage &message);
    void Remove(const QDBusObjectPath &account, const QDBusMessage &message);
    void SetClientId(const QString &id, const QDBusMessage &message)
    {
        calls << QStringLiteral("SetClientId:") + id;
        if (id == QLatin1String("bad")) {
            message.setDelayedReply(true);
            fake::bus().send(message.createErrorReply(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("invalid client ID")));
            return;
        }
        set({{QStringLiteral("ClientId"), id}});
    }

private:
    FakeDaemon *m_daemon;
    QVariantMap m_properties{
        {QStringLiteral("ClientId"), QString()},
        {QStringLiteral("HelperState"), QStringLiteral("connected")},
        {QStringLiteral("LastError"), QString()},
    };
};

/// The daemon: the manager object and its accounts. `account` and `sync` are
/// the first account's, for tests about one account.
class FakeDaemon : public QObject
{
    Q_OBJECT

public:
    /// One account per label, in order; by default one, "Personal".
    explicit FakeDaemon(const QStringList &labels = {QStringLiteral("Personal")})
        : manager(new FakeAccounts1(this))
    {
        // Before anything is sent: Transfers is an a(stt).
        registerKonedriveSyncTypes();
        for (const QString &label : labels) {
            addAccount(label);
        }
    }

    /// Takes the daemon's name on the private bus. Returns false on failure.
    bool start()
    {
        auto connection = fake::bus();
        bool ok = connection.registerObject(fake::ManagerPath, this, QDBusConnection::ExportAdaptors);
        for (FakeAccountObject *object : std::as_const(objects)) {
            ok = connection.registerObject(object->path, object, QDBusConnection::ExportAdaptors) && ok;
        }
        m_started = true;
        return connection.registerService(DaemonController::ServiceName) && ok;
    }

    /// Gives the name up, as a daemon that exits does.
    void stop()
    {
        auto connection = fake::bus();
        connection.unregisterService(DaemonController::ServiceName);
        for (FakeAccountObject *object : std::as_const(objects)) {
            connection.unregisterObject(object->path);
        }
        connection.unregisterObject(fake::ManagerPath);
        m_started = false;
    }

    /// As Accounts1.Add does: a new account, exported, then announced in Accounts.
    FakeAccountObject *addAccount(const QString &label)
    {
        auto *object = new FakeAccountObject(fake::idFor(++m_lastId), label, this);
        objects << object;
        if (objects.size() == 1) {
            account = object->account;
            sync = object->sync;
        }
        if (m_started) {
            fake::bus().registerObject(object->path, object, QDBusConnection::ExportAdaptors);
        }
        announce();
        return object;
    }

    /// As Accounts1.Remove does, once it has forgotten the folder: signed out
    /// first (the daemon's `retire`), then unexported, then gone from Accounts.
    bool removeAccount(const QString &path)
    {
        for (FakeAccountObject *object : std::as_const(objects)) {
            if (object->path == path) {
                object->account->set({{QStringLiteral("State"), QStringLiteral("signed-out")},
                                      {QStringLiteral("DisplayName"), QString()},
                                      {QStringLiteral("Email"), QString()}});
                objects.removeOne(object);
                if (m_started) {
                    fake::bus().unregisterObject(path);
                }
                announce();
                object->deleteLater();
                return true;
            }
        }
        return false;
    }

    FakeAccountObject *object(int index) const { return objects.value(index); }

    FakeAccounts1 *manager;
    QList<FakeAccountObject *> objects;
    FakeAccount1 *account = nullptr;
    FakeSync1 *sync = nullptr;

private:
    void announce() { manager->set({{QStringLiteral("Accounts"), QVariant::fromValue(manager->accounts())}}); }

    bool m_started = false;
    int m_lastId = 0;
};

inline FakeAccounts1::FakeAccounts1(FakeDaemon *daemon)
    : QDBusAbstractAdaptor(daemon)
    , m_daemon(daemon)
{
}

inline QList<QDBusObjectPath> FakeAccounts1::accounts() const
{
    QList<QDBusObjectPath> paths;
    for (const FakeAccountObject *object : std::as_const(m_daemon->objects)) {
        paths << QDBusObjectPath(object->path);
    }
    return paths;
}

inline QDBusObjectPath FakeAccounts1::Add(const QString &label, const QDBusMessage &message)
{
    calls << QStringLiteral("Add:") + label;
    for (const FakeAccountObject *object : std::as_const(m_daemon->objects)) {
        if (object->account->label().compare(label.trimmed(), Qt::CaseInsensitive) == 0 || label.trimmed().isEmpty()) {
            message.setDelayedReply(true);
            fake::bus().send(message.createErrorReply(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("that label is taken")));
            return {};
        }
    }
    return QDBusObjectPath(m_daemon->addAccount(label.trimmed())->path);
}

inline void FakeAccounts1::Remove(const QDBusObjectPath &account, const QDBusMessage &message)
{
    calls << QStringLiteral("Remove:") + account.path();
    if (refuseRemove) {
        message.setDelayedReply(true);
        fake::bus().send(message.createErrorReply(QStringLiteral("org.konedrive.Error.NoHelper"), QStringLiteral("the konedrive helper is not connected")));
        return;
    }
    if (!m_daemon->removeAccount(account.path())) {
        message.setDelayedReply(true);
        fake::bus().send(message.createErrorReply(QStringLiteral("org.konedrive.Error.NoAccount"), QStringLiteral("no such account")));
    }
}

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
