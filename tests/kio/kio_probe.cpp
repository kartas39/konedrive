// Measures what KIO does to files in a folder Dolphin shows (read-phase spec
// §10, limitation K1). Every finding is one line: RESULT <name>: <observed>.
// Run through tests/kio/run.sh, never directly: it writes into $HOME and
// $XDG_CACHE_HOME.

#include <KCoreDirLister>
#include <KFileItem>
#include <KIO/PreviewJob>

#include <QCryptographicHash>
#include <QDir>
#include <QEventLoop>
#include <QFile>
#include <QGuiApplication>
#include <QImage>
#include <QPixmap>
#include <QTimer>
#include <QUrl>

#include <fcntl.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <cstdio>
#include <set>

namespace
{
QString folder;
QString thumbs;

void result(const QString &name, const QString &observed)
{
    std::printf("RESULT %s: %s\n", qPrintable(name), qPrintable(observed));
    std::fflush(stdout);
}

/// The names of files in `folder` opened since construction (IN_OPEN on the
/// directory reports opens of its entries, with their names).
class OpenWatch
{
public:
    OpenWatch()
        : m_fd(inotify_init1(IN_NONBLOCK | IN_CLOEXEC))
    {
        inotify_add_watch(m_fd, QFile::encodeName(folder).constData(), IN_OPEN);
    }
    ~OpenWatch() { close(m_fd); }

    std::set<QString> opened()
    {
        std::set<QString> names;
        alignas(inotify_event) char buffer[64 * 1024];
        for (;;) {
            const ssize_t n = read(m_fd, buffer, sizeof buffer);
            if (n <= 0) {
                break;
            }
            for (char *p = buffer; p < buffer + n;) {
                auto *event = reinterpret_cast<inotify_event *>(p);
                if (event->len > 0) {
                    names.insert(QFile::decodeName(event->name));
                }
                p += sizeof(inotify_event) + event->len;
            }
        }
        return names;
    }

private:
    int m_fd;
};

QString path(const QString &name) { return folder + QLatin1Char('/') + name; }

void writeJpeg(const QString &file)
{
    QImage image(400, 300, QImage::Format_RGB32);
    image.fill(Qt::blue);
    image.save(file, "JPG");
}

/// A stand-in for a placeholder: the right size, no data at all.
void makeSparse(const QString &file, qint64 size, time_t mtime)
{
    QFile f(file);
    f.open(QIODevice::WriteOnly);
    f.resize(size);
    f.close();
    const struct timespec times[2] = {{mtime, 0}, {mtime, 0}};
    utimensat(AT_FDCWD, QFile::encodeName(file).constData(), times, 0);
}

QByteArray fullyEncoded(const QString &file) { return QUrl::fromLocalFile(file).toString(QUrl::FullyEncoded).toUtf8(); }
QByteArray prettyDecoded(const QString &file) { return QUrl::fromLocalFile(file).toString().toUtf8(); }
QString md5Png(const QByteArray &uri) { return QString::fromLatin1(QCryptographicHash::hash(uri, QCryptographicHash::Md5).toHex()) + QStringLiteral(".png"); }

/// Our own thumbnail for `file`: solid red, so a preview that comes back red
/// can only have come from the cache.
void writeOwnThumbnail(const QString &file, const QString &sizeDir, int edge, const QByteArray &uri)
{
    QImage image(edge, edge * 3 / 4, QImage::Format_ARGB32);
    image.fill(Qt::red);
    struct stat st {};
    stat(QFile::encodeName(file).constData(), &st);
    image.setText(QStringLiteral("Thumb::URI"), QString::fromUtf8(uri));
    image.setText(QStringLiteral("Thumb::MTime"), QString::number(st.st_mtime));
    image.setText(QStringLiteral("Software"), QStringLiteral("konedrive kio probe"));
    const QString dir = thumbs + QLatin1Char('/') + sizeDir;
    QDir().mkpath(dir);
    image.save(dir + QLatin1Char('/') + md5Png(uri), "PNG");
}

struct Preview {
    bool got = false;
    bool failed = false;
    QRgb center = 0;
};

Preview preview(const QString &file, int edge)
{
    Preview outcome;
    const KFileItemList items{KFileItem(QUrl::fromLocalFile(file))};
    const QStringList plugins = KIO::PreviewJob::availablePlugins();
    // Starts itself from the event loop.
    auto *job = new KIO::PreviewJob(items, QSize(edge, edge), &plugins);
    job->setIgnoreMaximumSize(true);
    QEventLoop loop;
    QObject::connect(job, &KIO::PreviewJob::gotPreview, [&](const KFileItem &, const QPixmap &pixmap) {
        outcome.got = true;
        const QImage image = pixmap.toImage();
        outcome.center = image.pixel(image.width() / 2, image.height() / 2);
    });
    QObject::connect(job, &KIO::PreviewJob::failed, [&](const KFileItem &) {
        outcome.failed = true;
    });
    QObject::connect(job, &KJob::result, &loop, &QEventLoop::quit);
    QTimer::singleShot(30000, &loop, &QEventLoop::quit);
    loop.exec();
    return outcome;
}

QString describe(const Preview &p)
{
    if (!p.got) {
        return p.failed ? QStringLiteral("no preview (failed)") : QStringLiteral("no preview");
    }
    return QStringLiteral("preview, center pixel #%1").arg(p.center & 0xffffff, 6, 16, QLatin1Char('0'));
}

QString listDir(const QString &sizeDir)
{
    return QDir(thumbs + QLatin1Char('/') + sizeDir).entryList(QDir::Files).join(QLatin1Char(' '));
}

// A: which name and which directory KIO uses, from thumbnails it makes itself.
void cacheNaming()
{
    const QStringList names{QStringLiteral("plain.jpg"), QStringLiteral("with space.jpg"), QStringLiteral("фото.jpg"),
                            QStringLiteral("sym (1)+&,;=@:!$'*.jpg")};
    for (const QString &name : names) {
        writeJpeg(path(name));
    }
    for (const int edge : {128, 256, 512, 1024}) {
        for (const QString &name : names) {
            preview(path(name), edge);
        }
    }
    for (const QString &sizeDir : {QStringLiteral("normal"), QStringLiteral("large"), QStringLiteral("x-large"), QStringLiteral("xx-large")}) {
        result(QStringLiteral("A.dir %1").arg(sizeDir), listDir(sizeDir));
    }
    for (const QString &name : names) {
        const QString file = path(name);
        const QString fully = md5Png(fullyEncoded(file));
        const QString pretty = md5Png(prettyDecoded(file));
        const QString normal = thumbs + QStringLiteral("/normal/");
        QString scheme = QStringLiteral("neither");
        if (QFile::exists(normal + fully)) {
            scheme = QStringLiteral("FullyEncoded");
        } else if (QFile::exists(normal + pretty)) {
            scheme = QStringLiteral("PrettyDecoded");
        }
        QImage made;
        made.load(normal + (scheme == QStringLiteral("PrettyDecoded") ? pretty : fully));
        result(QStringLiteral("A.name %1").arg(name),
               QStringLiteral("%1; Thumb::URI=%2").arg(scheme, made.text(QStringLiteral("Thumb::URI"))));
    }
}

// B: a file with no data and our own cached thumbnail — drawn from the cache
// without an open, or opened anyway?
void cacheHonoured()
{
    const time_t mtime = 1700000000;
    const QString withEntry = path(QStringLiteral("sparse.jpg"));
    const QString withoutEntry = path(QStringLiteral("sparse-nocache.jpg"));
    makeSparse(withEntry, 500000, mtime);
    makeSparse(withoutEntry, 500000, mtime);
    for (const auto &[sizeDir, edge] : {std::pair{QStringLiteral("normal"), 128}, std::pair{QStringLiteral("large"), 256}}) {
        writeOwnThumbnail(withEntry, sizeDir, edge, fullyEncoded(withEntry));
        writeOwnThumbnail(withEntry, sizeDir, edge, prettyDecoded(withEntry));
    }
    for (const int edge : {128, 256}) {
        OpenWatch watch;
        const Preview p = preview(withEntry, edge);
        const bool opened = watch.opened().count(QStringLiteral("sparse.jpg")) > 0;
        result(QStringLiteral("B.cached %1").arg(edge),
               QStringLiteral("%1; opened=%2").arg(describe(p), opened ? QStringLiteral("yes") : QStringLiteral("no")));
    }
    // The control: without an entry KIO must open the file, or the watch
    // above proves nothing.
    OpenWatch watch;
    const Preview p = preview(withoutEntry, 128);
    const bool opened = watch.opened().count(QStringLiteral("sparse-nocache.jpg")) > 0;
    result(QStringLiteral("B.control"),
           QStringLiteral("%1; opened=%2").arg(describe(p), opened ? QStringLiteral("yes") : QStringLiteral("no")));
}

// C: listing a folder, then resolving each item's type the way Dolphin's
// roles updater does.
void typeDetection()
{
    const QStringList names{QStringLiteral("data.bin"), QStringLiteral("noextension"), QStringLiteral("report.pdf"),
                            QStringLiteral("photo2.jpg"), QStringLiteral("noext-with-attr")};
    for (const QString &name : names) {
        makeSparse(path(name), 100000, 1700000000);
    }
    const QByteArray mime("image/jpeg");
    setxattr(QFile::encodeName(path(QStringLiteral("noext-with-attr"))).constData(), "user.mime_type", mime.constData(), mime.size(), 0);

    OpenWatch listingWatch;
    KCoreDirLister lister;
    QEventLoop loop;
    QObject::connect(&lister, &KCoreDirLister::completed, &loop, &QEventLoop::quit);
    QTimer::singleShot(30000, &loop, &QEventLoop::quit);
    lister.openUrl(QUrl::fromLocalFile(folder));
    loop.exec();
    QStringList openedByListing;
    for (const QString &name : listingWatch.opened()) {
        openedByListing << name;
    }
    result(QStringLiteral("C.listing"), QStringLiteral("opened: [%1]").arg(openedByListing.join(QStringLiteral(", "))));

    for (const KFileItem &item : lister.items()) {
        const QString name = item.name();
        if (!names.contains(name)) {
            continue;
        }
        OpenWatch watch;
        const QString type = item.determineMimeType().name();
        const bool opened = watch.opened().count(name) > 0;
        result(QStringLiteral("C.type %1").arg(name),
               QStringLiteral("%1; opened=%2").arg(type, opened ? QStringLiteral("yes") : QStringLiteral("no")));
    }
}
} // namespace

int main(int argc, char **argv)
{
    QGuiApplication app(argc, argv);
    if (argc != 3) {
        std::fprintf(stderr, "usage: kio_probe <folder> <thumbnail-cache-dir>\n");
        return 2;
    }
    folder = QString::fromLocal8Bit(argv[1]);
    thumbs = QString::fromLocal8Bit(argv[2]);
    QDir().mkpath(folder);
    cacheNaming();
    cacheHonoured();
    typeDetection();
    return 0;
}
