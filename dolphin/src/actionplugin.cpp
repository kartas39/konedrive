// "Download" and "Free up space" in Dolphin's context menu.
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
#include <QIcon>
#include <QPointer>
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
        const konedrive::ActionTargets targets = konedrive::actionTargets(paths);

        QList<QAction *> result;
        if (!targets.download.isEmpty()) {
            result.append(makeAction(QStringLiteral("konedrive_download"),
                                     QString::fromLatin1(konedrive::DownloadIcon),
                                     i18nc("@action:inmenu", "Download"),
                                     konedrive::Operation::Download,
                                     targets.download,
                                     parentWidget));
        }
        if (!targets.freeUpSpace.isEmpty()) {
            result.append(makeAction(QStringLiteral("konedrive_free_up_space"),
                                     QString::fromLatin1(konedrive::FreeUpSpaceIcon),
                                     i18nc("@action:inmenu", "Free up space"),
                                     konedrive::Operation::FreeUpSpace,
                                     targets.freeUpSpace,
                                     parentWidget));
        }
        return result;
    }

private:
    QAction *makeAction(const QString &objectName,
                        const QString &icon,
                        const QString &text,
                        konedrive::Operation operation,
                        const QStringList &paths,
                        QWidget *parentWidget)
    {
        auto *action = new QAction(QIcon::fromTheme(icon), text, parentWidget);
        action->setObjectName(objectName);
        connect(action, &QAction::triggered, this, [this, operation, paths]() {
            m_client->start(operation, paths);
        });
        m_previousActions.append(action);
        return action;
    }

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
