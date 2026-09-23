#include "accountcontroller.h"

#include <QDBusConnection>
#include <QDBusContext>
#include <QDBusError>
#include <QDBusMessage>
#include <QSignalSpy>
#include <QTest>

#include <memory>

namespace
{
const QString FakeBusName = QStringLiteral("fake-daemon");
const QString ClientId = QStringLiteral("0f8fad5b-d9cb-469f-a165-70867728950e");
}

/// Stands in for konedrived on the private session bus.
class FakeAccount : public QObject, protected QDBusContext
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
    explicit FakeAccount(const QDBusConnection &bus)
        : m_bus(bus)
    {
    }

    QString state() const { return m_properties.value(QStringLiteral("State")).toString(); }
    QString lastError() const { return m_properties.value(QStringLiteral("LastError")).toString(); }
    QString clientId() const { return m_properties.value(QStringLiteral("ClientId")).toString(); }
    QString displayName() const { return m_properties.value(QStringLiteral("DisplayName")).toString(); }
    QString email() const { return m_properties.value(QStringLiteral("Email")).toString(); }
    qulonglong quotaUsed() const { return m_properties.value(QStringLiteral("QuotaUsed")).toULongLong(); }
    qulonglong quotaTotal() const { return m_properties.value(QStringLiteral("QuotaTotal")).toULongLong(); }

    /// Changes properties and emits PropertiesChanged like konedrived does.
    void set(const QVariantMap &changes)
    {
        for (auto it = changes.cbegin(); it != changes.cend(); ++it) {
            m_properties.insert(it.key(), it.value());
        }
        auto signal = QDBusMessage::createSignal(AccountController::ObjectPath,
                                                 QStringLiteral("org.freedesktop.DBus.Properties"),
                                                 QStringLiteral("PropertiesChanged"));
        signal << AccountController::InterfaceName << changes << QStringList();
        m_bus.send(signal);
    }

    QStringList calls;

public Q_SLOTS:
    void SetClientId(const QString &id)
    {
        calls << QStringLiteral("SetClientId:") + id;
        if (id == QLatin1String("bad")) {
            sendErrorReply(QDBusError::InvalidArgs, QStringLiteral("invalid client ID"));
            return;
        }
        set({{QStringLiteral("ClientId"), id}});
    }

    QString BeginSignIn()
    {
        calls << QStringLiteral("BeginSignIn");
        set({{QStringLiteral("State"), QStringLiteral("signing-in")}});
        return QStringLiteral("https://login.example/authorize?x=1");
    }

    void CancelSignIn()
    {
        calls << QStringLiteral("CancelSignIn");
        set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
    }

    void SignOut()
    {
        calls << QStringLiteral("SignOut");
        set({{QStringLiteral("State"), QStringLiteral("signed-out")}, {QStringLiteral("DisplayName"), QString()}});
    }

    void RefreshAccountInfo()
    {
        calls << QStringLiteral("RefreshAccountInfo");
    }

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

class AccountControllerTest : public QObject
{
    Q_OBJECT

private:
    std::unique_ptr<FakeAccount> m_fake;

    static QDBusConnection fakeBus()
    {
        return QDBusConnection::connectToBus(QDBusConnection::SessionBus, FakeBusName);
    }

    void startFake()
    {
        auto bus = fakeBus();
        m_fake = std::make_unique<FakeAccount>(bus);
        QVERIFY(bus.registerObject(AccountController::ObjectPath,
                                   m_fake.get(),
                                   QDBusConnection::ExportAllSlots | QDBusConnection::ExportAllProperties));
        QVERIFY(bus.registerService(AccountController::ServiceName));
    }

private Q_SLOTS:
    void cleanup()
    {
        auto bus = fakeBus();
        bus.unregisterService(AccountController::ServiceName);
        bus.unregisterObject(AccountController::ObjectPath);
        m_fake.reset();
    }

    void reportsUnavailableWithoutDaemon()
    {
        AccountController controller;
        QTest::qWait(300);
        QVERIFY(!controller.serviceAvailable());
    }

    void readsInitialProperties()
    {
        startFake();
        AccountController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        QCOMPARE(controller.state(), QStringLiteral("signed-out"));
        QCOMPARE(controller.clientId(), QString());
    }

    void noticesDaemonStartingLater()
    {
        AccountController controller;
        QTest::qWait(100);
        QVERIFY(!controller.serviceAvailable());
        startFake();
        QTRY_VERIFY(controller.serviceAvailable());
    }

    void followsPropertiesChanged()
    {
        startFake();
        AccountController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        m_fake->set({
            {QStringLiteral("State"), QStringLiteral("signed-in")},
            {QStringLiteral("DisplayName"), QStringLiteral("Test User")},
            {QStringLiteral("QuotaTotal"), QVariant::fromValue<qulonglong>(5368709120ULL)},
        });
        QTRY_COMPARE(controller.state(), QStringLiteral("signed-in"));
        QCOMPARE(controller.displayName(), QStringLiteral("Test User"));
        QCOMPARE(controller.quotaTotal(), 5368709120ULL);
    }

    void signInRequestsBrowserAndCancelClearsUrl()
    {
        startFake();
        AccountController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        QSignalSpy openSpy(&controller, &AccountController::openUrlRequested);
        controller.signIn();
        QTRY_COMPARE(openSpy.count(), 1);
        QCOMPARE(openSpy.at(0).at(0).toString(), QStringLiteral("https://login.example/authorize?x=1"));
        QCOMPARE(controller.signInUrl(), QStringLiteral("https://login.example/authorize?x=1"));
        QTRY_COMPARE(controller.state(), QStringLiteral("signing-in"));

        controller.cancelSignIn();
        QTRY_COMPARE(controller.state(), QStringLiteral("signed-out"));
        QCOMPARE(controller.signInUrl(), QString());
        QVERIFY(m_fake->calls.contains(QStringLiteral("CancelSignIn")));
    }

    void showsMethodErrorsAndClearsThemOnSuccess()
    {
        startFake();
        AccountController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        controller.setClientId(QStringLiteral("bad"));
        QTRY_VERIFY(controller.actionError().contains(QStringLiteral("invalid client ID")));

        controller.setClientId(QStringLiteral("  ") + ClientId + QStringLiteral(" "));
        QTRY_COMPARE(controller.clientId(), ClientId);
        QVERIFY(controller.actionError().isEmpty());
        QVERIFY(m_fake->calls.contains(QStringLiteral("SetClientId:") + ClientId));
    }

    void signOutAndRefreshCallTheDaemon()
    {
        startFake();
        AccountController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        controller.refreshAccountInfo();
        controller.signOut();
        QTRY_VERIFY(m_fake->calls.contains(QStringLiteral("SignOut")));
        QVERIFY(m_fake->calls.contains(QStringLiteral("RefreshAccountInfo")));
    }
};

QTEST_GUILESS_MAIN(AccountControllerTest)

#include "accountcontrollertest.moc"
