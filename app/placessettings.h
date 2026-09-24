#pragma once

#include <QObject>

/// "Show in Places" (docs/limitations-and-workarounds.md, section 8): whether
/// the registered folder gets an entry in KDE's Places panel and file
/// dialogs. Stored the same way Autostart's StartAtLogin is, under
/// `ShowInPlaces` in konedriverc's [General] group; on by default.
class PlacesSettings : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool enabled READ enabled WRITE setEnabled NOTIFY enabledChanged)

public:
    explicit PlacesSettings(QObject *parent = nullptr);

    bool enabled() const { return m_enabled; }
    void setEnabled(bool enabled);

Q_SIGNALS:
    void enabledChanged();

private:
    bool m_enabled;
};
