#pragma once

#include <QObject>
#include <QString>

/// "Start at login": the XDG autostart entry
/// $XDG_CONFIG_HOME/autostart/org.konedrive.KOneDrive.desktop, which starts
/// the app hidden in the tray. The entry itself is the truth, so removing it
/// in System Settings turns the switch off. Whether the user has chosen yet
/// is kept in konedriverc, so the first run can turn it on once and never
/// again behind the user's back.
class Autostart : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool enabled READ enabled WRITE setEnabled NOTIFY enabledChanged)
    /// Why the last change could not be made; empty after one that worked.
    Q_PROPERTY(QString error READ error NOTIFY errorChanged)

public:
    /// `program` is what the entry runs, followed by --background.
    explicit Autostart(const QString &program = defaultProgram(), QObject *parent = nullptr);

    static QString defaultProgram();
    static QString entryPath();

    bool enabled() const;
    /// Writes or removes the entry. If that fails, the switch keeps its old
    /// state, `error` says why, and enabledChanged is still emitted so a
    /// switch the user flipped goes back.
    void setEnabled(bool enabled);
    QString error() const { return m_error; }

    /// With no choice stored yet, turns start at login on and stores that.
    void applyFirstRunDefault();

Q_SIGNALS:
    void enabledChanged();
    void errorChanged();

private:
    void setError(const QString &error);

    QString m_program;
    QString m_error;
};
