// Measures what KIO does when a file is moved to the trash, as a file manager asks for it.
// Every finding is one line: RESULT <name>: <observed>.
//
//   trash_probe plain <path>…    KIO::trash alone: the job a file manager's "Move to Trash" ends in.
//   trash_probe dolphin <path>…  KIO::DeleteOrTrashJob, the job Dolphin starts, with every question it
//                                asks the user printed and answered yes.
//
// Run in the VM suite's guest (tests/fuse-probe/guest.sh, group `kio_trash`), never directly: it
// writes into $HOME and $XDG_DATA_HOME.

#include <KIO/AskUserActionInterface>
#include <KIO/CopyJob>
#include <KIO/DeleteOrTrashJob>
#include <KJobUiDelegate>

#include <QGuiApplication>
#include <QTimer>
#include <QUrl>

#include <cstdio>

namespace
{
void result(const QString &name, const QString &observed)
{
    std::printf("RESULT %s: %s\n", qPrintable(name), qPrintable(observed));
    std::fflush(stdout);
}

QString deletionName(KIO::AskUserActionInterface::DeletionType type)
{
    switch (type) {
    case KIO::AskUserActionInterface::Delete:
        return QStringLiteral("Delete");
    case KIO::AskUserActionInterface::Trash:
        return QStringLiteral("Trash");
    case KIO::AskUserActionInterface::EmptyTrash:
        return QStringLiteral("EmptyTrash");
    case KIO::AskUserActionInterface::DeleteInsteadOfTrash:
        return QStringLiteral("DeleteInsteadOfTrash");
    case KIO::AskUserActionInterface::DeleteNoTrashAvailable:
        return QStringLiteral("DeleteNoTrashAvailable");
    }
    return QStringLiteral("unknown");
}

/// The user, who is asked and says yes to everything; what was asked is printed.
class YesUser : public KIO::AskUserActionInterface
{
public:
    explicit YesUser(QObject *parent)
        : KIO::AskUserActionInterface(parent)
    {
    }

    void askUserRename(KJob *job, const QString &, const QUrl &, const QUrl &, KIO::RenameDialog_Options, KIO::filesize_t, KIO::filesize_t, const QDateTime &, const QDateTime &, const QDateTime &, const QDateTime &) override
    {
        result(QStringLiteral("asked"), QStringLiteral("rename"));
        QTimer::singleShot(0, this, [this, job] { Q_EMIT askUserRenameResult(KIO::Result_Cancel, QUrl(), job); });
    }
    void askUserSkip(KJob *job, KIO::SkipDialog_Options, const QString &errorText) override
    {
        result(QStringLiteral("asked"), QStringLiteral("skip: %1").arg(errorText));
        QTimer::singleShot(0, this, [this, job] { Q_EMIT askUserSkipResult(KIO::Result_Cancel, job); });
    }
    void askUserDelete(const QList<QUrl> &urls, DeletionType deletionType, ConfirmationType, QWidget *parent) override
    {
        result(QStringLiteral("asked"), QStringLiteral("delete, as %1, of %2 item(s); answered yes").arg(deletionName(deletionType)).arg(urls.size()));
        QTimer::singleShot(0, this, [this, urls, deletionType, parent] { Q_EMIT askUserDeleteResult(true, urls, deletionType, parent); });
    }
    void requestUserMessageBox(MessageDialogType type, const QString &text, const QString &title, const QString &, const QString &, const QString &, const QString &, const QString &, const QString &details, QWidget *) override
    {
        result(QStringLiteral("shown"), QStringLiteral("message box %1, title \"%2\": %3 %4").arg(int(type)).arg(title, text, details));
        QTimer::singleShot(0, this, [this] { Q_EMIT messageBoxResult(0); });
    }
    void askIgnoreSslErrors(const QVariantMap &, QWidget *) override
    {
    }
};

QString said(KJob *job)
{
    return job->error() == 0 ? QStringLiteral("no error") : QStringLiteral("error %1 (%2)").arg(job->error()).arg(job->errorString());
}
}

int main(int argc, char **argv)
{
    QGuiApplication app(argc, argv);
    const QStringList arguments = app.arguments();
    if (arguments.size() < 3) {
        std::fprintf(stderr, "usage: trash_probe plain|dolphin <path>...\n");
        return 2;
    }
    QList<QUrl> urls;
    for (const QString &path : arguments.mid(2)) {
        urls.append(QUrl::fromLocalFile(path));
    }
    KJob *job = nullptr;
    if (arguments.at(1) == QLatin1String("plain")) {
        job = KIO::trash(urls, KIO::HideProgressInfo);
    } else {
        auto *asking = new KIO::DeleteOrTrashJob(urls, KIO::AskUserActionInterface::Trash, KIO::AskUserActionInterface::DefaultConfirmation, &app);
        auto *delegate = new KJobUiDelegate;
        new YesUser(delegate);
        asking->setUiDelegate(delegate);
        asking->start();
        job = asking;
    }
    QObject::connect(job, &KJob::result, &app, [&app](KJob *ended) {
        result(QStringLiteral("job"), said(ended));
        app.quit();
    });
    QTimer::singleShot(60000, &app, [&app] {
        result(QStringLiteral("job"), QStringLiteral("did not end within 60 s"));
        app.exit(3);
    });
    return app.exec();
}
