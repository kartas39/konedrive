#pragma once

#include <QObject>

/// "Show download progress" (Ruling 3): whether long downloads are
/// reported to Plasma as KJobs. Stored the same way Autostart's StartAtLogin
/// is, under `ShowDownloadProgress` in konedriverc's [General] group; on by
/// default.
class DownloadProgressSettings : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool enabled READ enabled WRITE setEnabled NOTIFY enabledChanged)

public:
    explicit DownloadProgressSettings(QObject *parent = nullptr);

    bool enabled() const { return m_enabled; }
    void setEnabled(bool enabled);

Q_SIGNALS:
    void enabledChanged();

private:
    bool m_enabled;
};
