#include "refusaltext.h"

#include <QFile>
#include <QTest>

/// The plugin's words for every operation and every refusal, as they were
/// while `dolphin/src/refusaltext.cpp` kept its own table of them (before
/// the catalogue in `crates/konedrive-text`): `refusaltexts.expected` was
/// written from that table, a line for each operation, error name and
/// message, so the generated table says the same, string for string.
class RefusalTextsTest : public QObject
{
    Q_OBJECT

    static QStringList names()
    {
        QStringList names;
        // Every name the daemon refuses a call under (konedrive-dbus, `Refusal`).
        for (const char *refusal : {"NotEmpty",       "Unsupported",       "InUse",       "NoRoot",         "NoHelper",       "NotManaged",
                                    "NotHydrated",    "ModifiedLocally",   "OutsideRoot", "AlreadyRegistered", "NotSignedIn", "NoSource",
                                    "NoConflict",     "NotAllowed",        "Overlaps",    "NoAccount",      "NotUploaded",    "PendingUploads",
                                    "Unreachable",    "NotUp",             "Failed",      "WritesNotAllowed", "ModeNotGranted",
                                    // One this plugin does not know.
                                    "SomethingNew"}) {
            names.append(QStringLiteral("org.konedrive.Error.") + QLatin1String(refusal));
        }
        // The bus's own, the daemon not running or stopped, a name that only ends like ours.
        for (const char *name : {"org.freedesktop.DBus.Error.InvalidArgs",
                                 "org.freedesktop.DBus.Error.Failed",
                                 "org.freedesktop.DBus.Error.UnknownObject",
                                 "org.freedesktop.DBus.Error.UnknownMethod",
                                 "org.freedesktop.DBus.Error.UnknownInterface",
                                 "org.freedesktop.zbus.Error",
                                 "org.freedesktop.DBus.Error.ServiceUnknown",
                                 "org.freedesktop.DBus.Error.NameHasNoOwner",
                                 "org.freedesktop.DBus.Error.TimedOut",
                                 "org.freedesktop.DBus.Error.Spawn.ChildExited",
                                 "org.freedesktop.systemd1.NoSuchUnit",
                                 "org.freedesktop.DBus.Error.NoReply",
                                 "org.example.Error.InUse",
                                 konedrive::AlreadyWaitingError,
                                 konedrive::TooManyWaitingError}) {
            names.append(QLatin1String(name));
        }
        return names;
    }

private Q_SLOTS:
    void everyRefusalReadsAsItDid()
    {
        using konedrive::Operation;
        const QList<QPair<QString, Operation>> operations = {
            {QStringLiteral("keep"), Operation::AlwaysKeep},
            {QStringLiteral("unpin"), Operation::Unpin},
            {QStringLiteral("free-up"), Operation::FreeUpSpace},
            {QStringLiteral("open-online"), Operation::OpenOnline},
        };
        const QStringList messages = {QString(), QStringLiteral("/home/u/OneDrive/Docs/doc.bin is pinned by /home/u/OneDrive/Docs: unpin it first")};

        QFile file(QStringLiteral(KONEDRIVE_EXPECTED_TEXTS));
        QVERIFY2(file.open(QIODevice::ReadOnly), qPrintable(file.fileName()));
        const QStringList expected = QString::fromUtf8(file.readAll()).split(QLatin1Char('\n'), Qt::SkipEmptyParts);

        QStringList lines;
        for (const auto &[word, operation] : operations) {
            for (const QString &name : names()) {
                for (const QString &message : messages) {
                    const konedrive::Failure failure{QStringLiteral("/home/u/OneDrive/Docs/doc.bin"), name, message};
                    lines.append(QStringList{word, name, message, konedrive::refusalText(operation, failure)}.join(QLatin1Char('|')));
                }
            }
        }
        QCOMPARE(lines.size(), expected.size());
        for (qsizetype i = 0; i < lines.size(); ++i) {
            QCOMPARE(lines.at(i), expected.at(i));
        }
    }
};

QTEST_GUILESS_MAIN(RefusalTextsTest)
#include "refusaltextstest.moc"
