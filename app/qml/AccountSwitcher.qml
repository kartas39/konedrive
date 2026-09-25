import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.components as Components
import org.konedrive.app

/// The top of the sidebar: the account the pages below show, and a menu to
/// switch to another or add one. Shown with one account too — it names the
/// account and is where "Add Account…" lives. A warning sign on it means
/// another account needs attention.
QQC2.ItemDelegate {
    id: switcher

    signal addRequested()

    readonly property var account: Current.account

    objectName: "accountSwitcher"
    padding: Kirigami.Units.largeSpacing
    text: account ? account.label : i18nc("@info:status", "No account yet")
    Accessible.description: Current.othersNeedAttention ? i18nc("@info:tooltip", "Another account needs your attention") : ""
    onClicked: accountMenu.popup(switcher, 0, switcher.height)

    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing

        Components.Avatar {
            visible: switcher.account !== null
            implicitWidth: Kirigami.Units.iconSizes.medium
            implicitHeight: Kirigami.Units.iconSizes.medium
            name: switcher.account ? switcher.account.label : ""
        }
        // With no account, a person rather than the initials of nothing.
        Kirigami.Icon {
            visible: switcher.account === null
            source: "im-user"
            implicitWidth: Kirigami.Units.iconSizes.medium
            implicitHeight: Kirigami.Units.iconSizes.medium
        }
        ColumnLayout {
            Layout.fillWidth: true
            spacing: 0

            QQC2.Label {
                Layout.fillWidth: true
                text: switcher.text
                font.bold: true
                elide: Text.ElideRight
            }
            QQC2.Label {
                Layout.fillWidth: true
                visible: text.length > 0
                text: switcher.account ? switcher.account.email : ""
                font: Kirigami.Theme.smallFont
                opacity: 0.7
                elide: Text.ElideRight
            }
        }
        Kirigami.Icon {
            objectName: "othersNeedAttention"
            visible: Current.othersNeedAttention
            source: "state-warning"
            implicitWidth: Kirigami.Units.iconSizes.small
            implicitHeight: Kirigami.Units.iconSizes.small

            QQC2.ToolTip.visible: attentionHover.hovered
            QQC2.ToolTip.text: switcher.Accessible.description
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            HoverHandler {
                id: attentionHover
            }
        }
        Kirigami.Icon {
            source: "arrow-down"
            implicitWidth: Kirigami.Units.iconSizes.small
            implicitHeight: Kirigami.Units.iconSizes.small
        }
    }

    QQC2.Menu {
        id: accountMenu
        objectName: "accountMenu"

        onAboutToShow: {
            for (let i = 0; i < count; ++i) {
                const entry = itemAt(i);
                if (entry && entry.path !== undefined) {
                    entry.checked = entry.path === Current.path;
                }
            }
        }

        Instantiator {
            model: Accounts
            delegate: QQC2.MenuItem {
                id: accountEntry

                required property string path
                required property string label
                required property string email
                required property string iconName

                text: label
                icon.name: iconName
                // A radio choice; which one is checked is set as the menu opens.
                checkable: true
                autoExclusive: true
                onTriggered: Current.select(path)

                contentItem: RowLayout {
                    spacing: Kirigami.Units.smallSpacing
                    Item {
                        Layout.preferredWidth: accountEntry.indicator ? accountEntry.indicator.width : 0
                    }
                    Kirigami.Icon {
                        source: accountEntry.iconName
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: accountEntry.label
                        elide: Text.ElideRight
                    }
                    QQC2.Label {
                        visible: text.length > 0
                        text: accountEntry.email
                        opacity: 0.7
                        elide: Text.ElideRight
                        Layout.maximumWidth: Kirigami.Units.gridUnit * 14
                    }
                    Item {
                        Layout.preferredWidth: Kirigami.Units.smallSpacing
                    }
                }
            }
            onObjectAdded: (index, object) => accountMenu.insertItem(index, object)
            onObjectRemoved: (index, object) => accountMenu.removeItem(object)
        }
        QQC2.MenuSeparator {
            visible: Accounts.count > 0
        }
        QQC2.MenuItem {
            objectName: "addAccountItem"
            text: i18nc("@action:inmenu", "Add Account…")
            icon.name: "list-add-user"
            onTriggered: switcher.addRequested()
        }
    }
}
