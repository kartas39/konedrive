#include "serviceunit.h"

#include <QSignalSpy>
#include <QTest>

/// Starting the daemon's systemd unit from the window. The test bus has no systemd on it:
/// a start is refused there, as one is when the user's systemd cannot be reached, and the
/// refusal is said rather than waited on.
class ServiceUnitTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void aStartSystemdRefusesEndsWithWhy()
    {
        ServiceUnit unit;
        QSignalSpy changed(&unit, &ServiceUnit::changed);
        unit.start();
        QVERIFY(unit.starting());
        QVERIFY(unit.startError().isEmpty());
        QTRY_VERIFY(!unit.starting());
        QVERIFY(unit.startError().startsWith(QStringLiteral("The service could not be started: ")));
        QCOMPARE(changed.count(), 2);

        // A new start clears what the last one said until it ends too.
        unit.start();
        QVERIFY(unit.startError().isEmpty());
        QTRY_VERIFY(!unit.starting());
        QVERIFY(!unit.startError().isEmpty());
    }
};

QTEST_MAIN(ServiceUnitTest)
#include "serviceunittest.moc"
