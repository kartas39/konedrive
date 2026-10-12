#include "generated/reasontexts.h"
#include "uploadreasons.h"

#include <QLocale>
#include <QTest>

#include <limits>

/// The window's words for the daemon's codes, written out: every sentence
/// here is the one the window showed while it kept its own tables of them
/// (`app/uploadreasons.cpp` and `stillHereText` in `app/synccontroller.cpp`,
/// before the catalogue in `crates/konedrive-text`), so the generated tables
/// say the same, string for string.
class ReasonTextsTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void everyReasonReadsAsItDid_data()
    {
        QTest::addColumn<QString>("stored");
        QTest::addColumn<QString>("text");
        const auto row = [](const char *stored, const QString &text) {
            QTest::newRow(stored) << QString::fromUtf8(stored) << text;
        };
        const QString incomplete = QStringLiteral("KOneDrive's record of this change is incomplete (%1): it stays here until the file is changed again.");
        const QString keepsChanging = QStringLiteral("It keeps changing in OneDrive: tried again later.");

        row("name-characters", QStringLiteral("A name OneDrive refuses (it holds one of \" * : < > ? \\ |): rename it to upload it."));
        row("name-spaces", QStringLiteral("A name that starts or ends with a space, which OneDrive refuses: rename it to upload it."));
        row("name-reserved", QStringLiteral("A name OneDrive reserves: rename it to upload it."));
        row("name-not-utf8", QStringLiteral("A name that is not valid UTF-8: rename it to upload it."));
        row("too-large", QStringLiteral("Larger than OneDrive takes."));
        row("quota-exceeded", QStringLiteral("OneDrive is full: free some space in OneDrive."));
        row("forbidden", QStringLiteral("This sign-in does not allow uploads: sign in again."));
        row("open-for-writing", QStringLiteral("Open for writing in another program: it goes up once closed."));
        row("locked", QStringLiteral("Locked in OneDrive (open for co-authoring): tried again later."));
        row("network", QStringLiteral("OneDrive could not be reached: tried again later."));
        row("local-error", QStringLiteral("The local file could not be read: tried again later."));
        row("index-error", QStringLiteral("KOneDrive's local index failed: tried again later."));
        row("upload-error", QStringLiteral("The upload failed: tried again later."));
        row("refused", QStringLiteral("Refused by OneDrive."));
        row("refused: The name is not allowed", QStringLiteral("OneDrive refused it: The name is not allowed"));
        row("refused: ", QStringLiteral("OneDrive refused it: "));
        row("paused", QStringLiteral("Paused with the account: it goes on when the pause ends."));
        row("upload-session-open", QStringLiteral("Its name in OneDrive is held by an upload of this folder that has not ended: tried again later."));
        row("name-held-by-an-upload",
            QStringLiteral("Its name in OneDrive is held by an unfinished upload (another device, or one abandoned): tried again later."));
        row("changed in OneDrive again and again", keepsChanging);
        row("the upload session ended twice", QStringLiteral("OneDrive ended the upload twice: tried again later."));
        row("not allowed now", QStringLiteral("Uploads are not allowed now: it goes on when they are."));
        row("not allowed now: the folder is read-only: for now",
            QStringLiteral("Uploads are not allowed now: it goes on when they are (the folder is read-only: for now)."));
        row("blocked", incomplete.arg(QStringLiteral("blocked")));
        row("not-read-on-disk", QStringLiteral("KOneDrive could not read this on disk when it started: nothing of it is uploaded until it can be read at a start."));
        row("being-checked", QStringLiteral("Being checked: KOneDrive is asking OneDrive for the whole folder again, and goes on by itself."));
        row("quiet-time", QStringLiteral("Changed a moment ago: it goes up once it has been left alone for a moment."));
        row("waiting-for-space", QStringLiteral("OneDrive is full: free up space in OneDrive, then Refresh."));
        row("too-big", QStringLiteral("Too big for the space left in OneDrive: free up space there, then Refresh."));
        const QLocale locale;
        row("too-big:3221225472:1073741824",
            QStringLiteral("Too big: needs %1, %2 free.").arg(locale.formattedDataSize(3221225472LL), locale.formattedDataSize(1073741824LL)));
        row("too-big:9223372036854775807:1",
            QStringLiteral("Too big: needs %1, %2 free.").arg(locale.formattedDataSize(std::numeric_limits<qint64>::max()), locale.formattedDataSize(1)));
        row("too-big:03:1", QStringLiteral("Too big: needs %1, %2 free.").arg(locale.formattedDataSize(3), locale.formattedDataSize(1)));

        // No sentence: as the daemon wrote it.
        for (const char *stored : {"not-found",
                                   "changed-while-sending",
                                   "parent-not-in-onedrive",
                                   "hash-mismatch",
                                   "not-found: errno 5",
                                   "symlink",
                                   "state-unreadable: Input/output error (os error 5)",
                                   "something-new",
                                   "network: connection reset",
                                   "symlink: of some kind",
                                   "too-big:1:2:3",
                                   // Past what a signed 64-bit number holds: not printed negative.
                                   "too-big:9223372036854775808:1",
                                   "too-big:1:18446744073709551615",
                                   "too-big:",
                                   "error sending request for url (<url>)",
                                   ""}) {
            row(stored, QString::fromUtf8(stored));
        }
    }

    void everyReasonReadsAsItDid()
    {
        QFETCH(QString, stored);
        QFETCH(QString, text);
        QCOMPARE(uploadReasonText(stored), text);
    }

    void whatKeepsAnItemHereReadsAsItDid_data()
    {
        QTest::addColumn<QString>("waits");
        QTest::addColumn<QString>("text");
        const auto row = [](const char *waits, const QString &text) {
            QTest::newRow(waits) << QString::fromUtf8(waits) << text;
        };
        const QString plain = QStringLiteral("Still on this computer: it leaves once nothing in it waits to be uploaded.");
        row("", QString());
        row("cycle", plain);
        row("uploads:1", QStringLiteral("Still on this computer: 1 change in it waits to be uploaded."));
        row("uploads:12", QStringLiteral("Still on this computer: 12 changes in it wait to be uploaded."));
        row("uploads:0", QStringLiteral("Still on this computer: 0 changes in it wait to be uploaded."));
        row("uploads:many", plain);
        row("open-for-writing:/home/u/OneDrive/docs/a.txt", QStringLiteral("Still on this computer: /home/u/OneDrive/docs/a.txt is open in a program."));
        // A key of an older daemon: nothing can be named.
        row("mounted-inside:/home/u/OneDrive/docs/sub", plain);
        row("something-new:1", plain);
    }

    void whatKeepsAnItemHereReadsAsItDid()
    {
        QFETCH(QString, waits);
        QFETCH(QString, text);
        QCOMPARE(stillHereSentence(waits), text);
    }
};

QTEST_GUILESS_MAIN(ReasonTextsTest)
#include "reasontextstest.moc"
