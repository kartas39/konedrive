// Overlay emblems in Dolphin: a cloud on an online-only file, a check mark on
// a downloaded one, sync arrows on one being downloaded, freed up or waiting
// to be uploaded, and an error sign on one whose upload is blocked.
//
// Dolphin loads this with QPluginLoader::instance(), once per process, and
// asks every view's roles updater to call getOverlays() for each file it
// shows (dolphin src/kitemviews/kfileitemmodelrolesupdater.cpp).

#include "overlayengine.h"

#include <KOverlayIconPlugin>

class KonedriveOverlayPlugin : public KOverlayIconPlugin
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID "org.kde.overlayicon.konedrive" FILE "konedriveoverlay.json")

public:
    KonedriveOverlayPlugin()
    {
        connect(&m_engine, &konedrive::OverlayEngine::overlaysChanged, this, &KOverlayIconPlugin::overlaysChanged);
    }

    QStringList getOverlays(const QUrl &item) override
    {
        return m_engine.overlays(item);
    }

private:
    konedrive::OverlayEngine m_engine;
};

#include "overlayplugin.moc"
