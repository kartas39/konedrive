// "Always keep on this device", "Free up space" and "Open in OneDrive" in
// Dolphin's context menu, as a section of their own under the heading
// "OneDrive".
//
// KFileItemActions creates this once per Dolphin window and calls actions()
// each time a context menu is built (kio src/widgets/kfileitemactions.cpp);
// an `error` it emits is shown in the window's message bar.

#include "filestate.h"
#include "generated/refusaltexts.h"
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

#include <optional>

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
        discardPreviousMenu();
    }

    QList<QAction *> actions(const KFileItemListProperties &fileItemInfos, QWidget *parentWidget) override
    {
        // The previous menu is gone by now; its actions would otherwise live
        // as long as the window they are parented to, and its answer, if it
        // is still to come, is nobody's.
        discardPreviousMenu();

        QStringList paths;
        const KFileItemList items = fileItemInfos.items();
        for (const KFileItem &item : items) {
            const QString path = item.localPath();
            if (!path.isEmpty()) {
                paths.append(path);
            }
        }
        // By the marks alone: a right click outside every sync folder costs
        // no call. Everything else is the daemon's answer, and nothing is
        // decided here from the marks instead.
        if (!konedrive::anyInSyncFolder(paths)) {
            return {};
        }

        // The menu is never held up for the daemon: the entries are handed
        // over at once, waiting -- shown, disabled, unchecked, with nothing
        // to ask the daemon for -- and are set when the answer comes.
        auto *menu = new Menu(this);
        m_menu = menu;

        // A section of the menu itself, not a submenu: a separator that
        // carries the heading (drawn where the widget style draws a
        // separator's text), the entries, and a closing separator.
        menu->heading = new QAction(parentWidget);
        menu->heading->setSeparator(true);
        menu->heading->setText(i18nc("@title:menu the heading of KOneDrive's entries", "OneDrive"));
        menu->heading->setObjectName(QStringLiteral("konedrive_section"));
        menu->heading->setProperty(konedrive::WaitingProperty, true);

        menu->alwaysKeep = new QAction(QIcon::fromTheme(QString::fromLatin1(konedrive::AlwaysKeepIcon)), i18nc("@action:inmenu", "Always Keep on This Device"), parentWidget);
        menu->alwaysKeep->setObjectName(QStringLiteral("konedrive_always_keep"));
        menu->alwaysKeep->setCheckable(true);
        menu->alwaysKeep->setEnabled(false);
        // The calls are the menu's own: with the menu discarded they are
        // not made, and before the answer there are no paths to make them with.
        connect(menu->alwaysKeep, &QAction::triggered, menu, [this, menu](bool checked) {
            // Windows-like (D-A): unchecking it only unpins -- it never
            // frees space on its own, "Free up space" does that.
            m_client->start(checked ? konedrive::Operation::AlwaysKeep : konedrive::Operation::Unpin, menu->paths);
        });

        menu->freeUp = new QAction(QIcon::fromTheme(QString::fromLatin1(konedrive::FreeUpSpaceIcon)), i18nc("@action:inmenu", "Free Up Space"), parentWidget);
        menu->freeUp->setObjectName(QStringLiteral("konedrive_free_up_space"));
        menu->freeUp->setEnabled(false);
        connect(menu->freeUp, &QAction::triggered, menu, [this, menu]() {
            m_client->start(konedrive::Operation::FreeUpSpace, menu->paths);
        });

        menu->openOnline = new QAction(QIcon::fromTheme(QString::fromLatin1(konedrive::OpenOnlineIcon)), i18nc("@action:inmenu", "Open in OneDrive"), parentWidget);
        menu->openOnline->setObjectName(QStringLiteral("konedrive_open_online"));
        menu->openOnline->setEnabled(false);
        connect(menu->openOnline, &QAction::triggered, menu, [this, menu]() {
            if (!menu->openOnlinePath.isEmpty()) {
                m_client->start(konedrive::Operation::OpenOnline, {menu->openOnlinePath});
            }
        });

        menu->end = new QAction(parentWidget);
        menu->end->setSeparator(true);
        menu->end->setObjectName(QStringLiteral("konedrive_section_end"));

        // The answer is the menu's too: it is dropped with it.
        m_client->askMenu(paths, menu, [menu](const std::optional<konedrive::MenuAnswer> &answer) {
            // No answer -- no daemon, an error, none in time -- is an answer
            // that offers nothing.
            menu->show(answer.value_or(konedrive::MenuAnswer()));
        });

        return {menu->heading, menu->alwaysKeep, menu->freeUp, menu->openOnline, menu->end};
    }

private:
    /// The entries handed over for one menu, and what they ask the daemon
    /// for once it has answered.
    ///
    /// The actions belong to the widget they were made for, which can go at
    /// any time -- the window closed while the answer was on its way -- so
    /// each is looked at through a guarded pointer, and one that is gone is
    /// not touched.
    class Menu : public QObject
    {
    public:
        using QObject::QObject;

        QPointer<QAction> heading;
        QPointer<QAction> alwaysKeep;
        QPointer<QAction> freeUp;
        QPointer<QAction> openOnline;
        QPointer<QAction> end;
        /// What Pin(), Unpin() or FreeUp() is called with: empty until the
        /// daemon has answered, so that a waiting entry asks for nothing.
        QStringList paths;
        /// What WebUrl() is called with; empty until then, too.
        QString openOnlinePath;

        /// Sets every entry that is still there from `answer`.
        void show(const konedrive::MenuAnswer &answer)
        {
            using Keep = konedrive::MenuAnswer::AlwaysKeep;
            using Offer = konedrive::MenuAnswer::Offer;
            using Why = konedrive::MenuAnswer::FreeUpWhy;
            paths = answer.paths;
            openOnlinePath = answer.openOnlinePath;

            if (alwaysKeep) {
                alwaysKeep->setVisible(answer.alwaysKeep != Keep::Hidden);
                alwaysKeep->setChecked(answer.alwaysKeep == Keep::On || answer.alwaysKeep == Keep::OnLocked);
                // Checked and locked: unchecking it (Unpin()) would be refused.
                alwaysKeep->setEnabled(answer.alwaysKeep == Keep::Off || answer.alwaysKeep == Keep::On);
                if (answer.alwaysKeep == Keep::OnLocked) {
                    alwaysKeep->setToolTip(konedrive::keptByAFolderToolTip(answer.blockedBy));
                }
            }
            if (freeUp) {
                freeUp->setVisible(answer.freeUp != Offer::Hidden);
                freeUp->setEnabled(answer.freeUp == Offer::Enabled);
                // Why FreeUp() would be refused, in a tooltip's length: the
                // catalogue's words (crates/konedrive-text, src/menu.rs),
                // beside those of the refusal itself.
                const QString why = konedrive::freeUpWhyToolTip(answer.freeUp == Offer::Disabled ? answer.freeUpWhy : Why::NotSaid, answer.blockedBy);
                if (!why.isEmpty()) {
                    freeUp->setToolTip(why);
                }
            }
            if (openOnline) {
                openOnline->setVisible(answer.openOnline != Offer::Hidden);
                openOnline->setEnabled(answer.openOnline == Offer::Enabled);
                if (answer.openOnline == Offer::Disabled) {
                    openOnline->setToolTip(konedrive::notInOneDriveToolTip());
                }
            }
            // With nothing offered the section goes too.
            const bool any = answer.alwaysKeep != Keep::Hidden || answer.freeUp != Offer::Hidden || answer.openOnline != Offer::Hidden;
            if (end) {
                end->setVisible(any);
            }
            if (heading) {
                heading->setVisible(any);
                heading->setProperty(konedrive::WaitingProperty, false);
            }
        }

        /// The actions go with the menu they were made for.
        ~Menu() override
        {
            for (const QPointer<QAction> &action : {heading, alwaysKeep, freeUp, openOnline, end}) {
                delete action.data();
            }
        }
    };

    void discardPreviousMenu()
    {
        delete m_menu.data();
    }

    konedrive::SyncClient *m_client;
    QPointer<Menu> m_menu;
};

K_PLUGIN_CLASS_WITH_JSON(KonedriveActionPlugin, "konedriveactions.json")

#include "actionplugin.moc"
