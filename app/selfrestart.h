#pragma once

#include <QObject>
#include <QString>

class DaemonController;

/// Notices that this window is no longer the installed program: the daemon
/// came back as another build than the window's (DaemonController's
/// daemonBuildMismatch), and the window's own program file has been replaced
/// since it started — which is what installing a newer package does. main()
/// then starts the installed program in this one's place. A window whose file
/// is still the one it started from is left alone, so one built by hand and
/// run against an installed daemon never restarts, and neither does the
/// program just started.
class SelfRestart : public QObject
{
    Q_OBJECT

public:
    /// `program` is the path this window was started from.
    SelfRestart(DaemonController *daemon, const QString &program, QObject *parent = nullptr);

    QString program() const { return m_program; }
    /// The file at `program` can be run and is not the one this window runs.
    bool programReplaced() const;

Q_SIGNALS:
    /// Once: the installed program should take this one's place.
    void wanted();

private:
    void check();

    DaemonController *m_daemon;
    QString m_program;
    /// The device and inode of the file this window runs; `m_known` false when they could not be read.
    quint64 m_device = 0;
    quint64 m_inode = 0;
    bool m_known = false;
    bool m_asked = false;
};
