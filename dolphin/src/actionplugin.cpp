// "Always keep on this device", "Free up space" and "Open in OneDrive" in
// Dolphin's context menu, as a section of their own under the heading
// "OneDrive".
//
// KFileItemActions creates this once per Dolphin window and calls actions()
// each time a context menu is built (kio src/widgets/kfileitemactions.cpp);
// an `error` it emits is shown in the window's message bar.

#include "filestate.h"
#include "syncclient.h"

#include <KAbstractFileItemActionPlugin>
#include <KFileItem>
#include <KFileItemListProperties>
#include <KLocalizedString>
#include <KPluginFactory>

#include <QAction>
#include <QDesktopServices>
#include <QIcon>
#include <QPointer>
#include <QUrl>
#include <QWidget>

class KonedriveActionPlugin : public KAbstractFileItemActionPlugin
{
    Q_OBJECT

public:
    KonedriveActionPlugin(QObject *parent, const QVariantList &)
        : KAbstractFileItemActionPlugin(parent)
        , m_client(new konedrive::SyncClient(QDBusConnection::sessionBus(), this))
    {
        connect(m_client, &konedrive::SyncClient::failed, this, &KAbstractFileItemActionPlugin::error);
        connect(m_client, &konedrive::SyncClient::freeUpKeptBusy, this, [this](uint busy) {
            Q_EMIT error(i18ncp("@info", "%1 file is in use or was changed here and was kept.", "%1 files are in use or were changed here and were kept.", busy));
        });
        connect(m_client, &konedrive::SyncClient::webUrlReady, this, [this](const QString &path, const QString &address) {
            // Only an address of the web is handed to the desktop, whatever
            // answered on the bus: never a local file or another scheme.
            const QUrl url(address, QUrl::StrictMode);
            if (!url.isValid() || url.scheme() != QLatin1String("https") || !QDesktopServices::openUrl(url)) {
                Q_EMIT error(i18nc("@info", "The page of “%1” in OneDrive could not be opened.", konedrive::fileName(path)));
            }
        });
    }

    ~KonedriveActionPlugin() override
    {
        discardPreviousActions();
    }

    QList<QAction *> actions(const KFileItemListProperties &fileItemInfos, QWidget *parentWidget) override
    {
        // The previous menu is gone by now; its actions would otherwise live
        // as long as the window they are parented to.
        discardPreviousActions();

        QStringList paths;
        const KFileItemList items = fileItemInfos.items();
        for (const KFileItem &item : items) {
            const QString path = item.localPath();
            if (!path.isEmpty()) {
                paths.append(path);
            }
        }
        const konedrive::MenuState state = konedrive::menuState(paths);

        QList<QAction *> entries;
        if (state.showAlwaysKeep) {
            auto *action = new QAction(QIcon::fromTheme(QString::fromLatin1(konedrive::AlwaysKeepIcon)),
                                       i18nc("@action:inmenu", "Always Keep on This Device"),
                                       parentWidget);
            action->setObjectName(QStringLiteral("konedrive_always_keep"));
            action->setCheckable(true);
            action->setChecked(state.alwaysKeepChecked);
            action->setEnabled(state.alwaysKeepEnabled);
            if (!state.alwaysKeepEnabled) {
                action->setToolTip(i18nc("@info:tooltip", "Kept on this device because “%1” is.", state.blockingFolder));
            }
            const QStringList paths = state.inRoot;
            connect(action, &QAction::triggered, this, [this, paths](bool checked) {
                // Windows-like (D-A): unchecking it only unpins -- it never
                // frees space on its own, "Free up space" does that.
                m_client->start(checked ? konedrive::Operation::AlwaysKeep : konedrive::Operation::Unpin, paths);
            });
            entries.append(action);
        }
        if (state.showFreeUp) {
            auto *action = new QAction(QIcon::fromTheme(QString::fromLatin1(konedrive::FreeUpSpaceIcon)), i18nc("@action:inmenu", "Free Up Space"), parentWidget);
            action->setObjectName(QStringLiteral("konedrive_free_up_space"));
            action->setEnabled(state.freeUpEnabled);
            if (!state.freeUpEnabled) {
                action->setToolTip(i18nc("@info:tooltip", "Kept on this device because “%1” is; unpin it first.", state.blockingFolder));
            }
            const QStringList paths = state.inRoot;
            connect(action, &QAction::triggered, this, [this, paths]() {
                m_client->start(konedrive::Operation::FreeUpSpace, paths);
            });
            entries.append(action);
        }
        if (state.showOpenOnline) {
            auto *action = new QAction(QIcon::fromTheme(QString::fromLatin1(konedrive::OpenOnlineIcon)), i18nc("@action:inmenu", "Open in OneDrive"), parentWidget);
            action->setObjectName(QStringLiteral("konedrive_open_online"));
            action->setEnabled(state.openOnlineEnabled);
            if (!state.openOnlineEnabled) {
                action->setToolTip(i18nc("@info:tooltip", "Not in OneDrive yet."));
            }
            const QString path = state.openOnlinePath;
            connect(action, &QAction::triggered, this, [this, path]() {
                m_client->start(konedrive::Operation::OpenOnline, {path});
            });
            entries.append(action);
        }
        if (entries.isEmpty()) {
            return {};
        }

        // A section of the menu itself, not a submenu: a separator that
        // carries the heading (drawn where the widget style draws a
        // separator's text), the entries, and a closing separator.
        auto *heading = new QAction(parentWidget);
        heading->setSeparator(true);
        heading->setText(i18nc("@title:menu the heading of KOneDrive's entries", "OneDrive"));
        heading->setObjectName(QStringLiteral("konedrive_section"));
        auto *end = new QAction(parentWidget);
        end->setSeparator(true);
        end->setObjectName(QStringLiteral("konedrive_section_end"));

        QList<QAction *> result;
        result.append(heading);
        result.append(entries);
        result.append(end);
        for (QAction *action : std::as_const(result)) {
            m_previousActions.append(action);
        }
        return result;
    }

private:
    void discardPreviousActions()
    {
        for (const QPointer<QAction> &action : std::as_const(m_previousActions)) {
            delete action.data();
        }
        m_previousActions.clear();
    }

    konedrive::SyncClient *m_client;
    QList<QPointer<QAction>> m_previousActions;
};

K_PLUGIN_CLASS_WITH_JSON(KonedriveActionPlugin, "konedriveactions.json")

#include "actionplugin.moc"
