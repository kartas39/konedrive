#include "selfrestart.h"

#include "daemoncontroller.h"
#include "trayicon.h"

#include <QFile>

#include <sys/stat.h>
#include <unistd.h>

namespace
{
bool identity(const QString &path, quint64 *device, quint64 *inode)
{
    struct stat st;
    if (::stat(QFile::encodeName(path).constData(), &st) != 0) {
        return false;
    }
    *device = quint64(st.st_dev);
    *inode = quint64(st.st_ino);
    return true;
}
}

SelfRestart::SelfRestart(DaemonController *daemon, const QString &program, QObject *parent)
    : QObject(parent)
    , m_daemon(daemon)
    , m_program(program)
{
    // The file this process runs, whatever is at `program` by now.
    m_known = identity(QStringLiteral("/proc/self/exe"), &m_device, &m_inode);
    connect(m_daemon, &DaemonController::daemonBuildChanged, this, &SelfRestart::check);
}

bool SelfRestart::programReplaced() const
{
    quint64 device = 0;
    quint64 inode = 0;
    // A file that is not there or cannot be run (an update half done) is nothing to start.
    return m_known && identity(m_program, &device, &inode) && (device != m_device || inode != m_inode)
        && ::access(QFile::encodeName(m_program).constData(), X_OK) == 0;
}

void SelfRestart::check()
{
    if (m_asked || m_daemon->daemonBuildMismatch().isEmpty()) {
        return;
    }
    const bool replaced = programReplaced();
    qCDebug(KONEDRIVE_APP) << "the daemon is another build; this program" << (replaced ? "was replaced" : "is still the one installed");
    if (!replaced) {
        return;
    }
    m_asked = true;
    Q_EMIT wanted();
}
