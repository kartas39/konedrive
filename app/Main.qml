import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.konedrive.app

import "qml"

/// The window: pages chosen from a sidebar on the left, which folds into a
/// drawer behind a menu button when the window is narrow.
Kirigami.ApplicationWindow {
    id: root

    /// The page shown: status, activity, conflicts, skipped, account or settings.
    property string currentPage: "status"
    /// Wide enough for the sidebar beside a page.
    readonly property bool sidebarFits: width >= Kirigami.Units.gridUnit * 36

    title: i18nc("@title:window", "KOneDrive")
    // main.cpp shows the window unless started with --background.
    visible: false
    width: Kirigami.Units.gridUnit * 46
    height: Kirigami.Units.gridUnit * 34
    minimumWidth: Kirigami.Units.gridUnit * 20
    minimumHeight: Kirigami.Units.gridUnit * 20

    /// Shows one of the pages, by name.
    function showPage(name) {
        const pages = {
            "status": statusPage,
            "activity": activityPage,
            "conflicts": conflictsPage,
            "skipped": skippedPage,
            "account": accountPage,
            "settings": settingsPage,
        };
        if (!pages[name]) {
            return;
        }
        if (name !== currentPage || pageStack.depth === 0) {
            currentPage = name;
            pageStack.clear();
            pageStack.push(pages[name]);
        }
        if (drawer.modal) {
            drawer.close();
        }
    }

    /// A unix time as the time of day today, or a short date and time before.
    function when(unixSeconds) {
        const date = new Date(unixSeconds * 1000);
        if (date.toDateString() === new Date().toDateString()) {
            return date.toLocaleTimeString(Qt.locale(), Locale.ShortFormat);
        }
        return date.toLocaleString(Qt.locale(), Locale.ShortFormat);
    }

    Connections {
        target: Account
        function onOpenUrlRequested(url) {
            Qt.openUrlExternally(url);
        }
    }

    Component.onCompleted: pageStack.push(statusPage)
    pageStack.globalToolBar.showNavigationButtons: Kirigami.ApplicationHeaderStyle.NoNavigationButtons

    globalDrawer: Kirigami.GlobalDrawer {
        id: drawer

        objectName: "sidebar"
        modal: !root.sidebarFits
        handleVisible: modal
        width: Kirigami.Units.gridUnit * 12
        leftPadding: 0
        rightPadding: 0
        topPadding: Kirigami.Units.smallSpacing
        // A sidebar is always open; a drawer opens from its button.
        onModalChanged: drawerOpen = !modal
        Component.onCompleted: drawerOpen = !modal

        Repeater {
            model: [
                { name: "status", text: i18nc("@title sidebar", "Status"), icon: Status.iconName },
                { name: "activity", text: i18nc("@title sidebar", "Activity"), icon: "view-history" },
                { name: "conflicts", text: i18nc("@title sidebar", "Conflicts"), icon: "document-duplicate" },
                { name: "skipped", text: i18nc("@title sidebar", "Not in the Folder"), icon: "view-hidden" },
                { name: "account", text: i18nc("@title sidebar", "Account"), icon: "im-user" },
                { name: "settings", text: i18nc("@title sidebar", "Settings"), icon: "settings-configure" },
            ]
            delegate: QQC2.ItemDelegate {
                id: entry

                required property var modelData
                readonly property int badge: modelData.name === "conflicts" ? Sync.conflictCount : 0

                objectName: "sidebar-" + modelData.name
                Layout.fillWidth: true
                highlighted: root.currentPage === modelData.name
                text: modelData.text
                icon.name: modelData.icon
                onClicked: root.showPage(modelData.name)

                contentItem: RowLayout {
                    spacing: Kirigami.Units.largeSpacing
                    Kirigami.Icon {
                        source: entry.icon.name
                        implicitWidth: Kirigami.Units.iconSizes.smallMedium
                        implicitHeight: Kirigami.Units.iconSizes.smallMedium
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: entry.text
                        elide: Text.ElideRight
                    }
                    // The count of conflicts, when there are any.
                    Rectangle {
                        visible: entry.badge > 0
                        radius: height / 2
                        color: Kirigami.Theme.negativeTextColor
                        implicitHeight: badgeLabel.implicitHeight + Kirigami.Units.smallSpacing
                        implicitWidth: Math.max(implicitHeight, badgeLabel.implicitWidth + Kirigami.Units.largeSpacing)
                        QQC2.Label {
                            id: badgeLabel
                            anchors.centerIn: parent
                            text: entry.badge
                            color: Kirigami.Theme.highlightedTextColor
                            font.bold: true
                        }
                    }
                }
            }
        }
        Item {
            Layout.fillHeight: true
        }
    }

    // The pages live for the window's lifetime: the page row borrows the one
    // shown and hands it back here (hidden) when another is chosen.
    Item {
        visible: false

        StatusPage {
            id: statusPage
        }
        ActivityPage {
            id: activityPage
        }
        ConflictsPage {
            id: conflictsPage
        }
        SkippedPage {
            id: skippedPage
        }
        AccountPage {
            id: accountPage
        }
        SettingsPage {
            id: settingsPage
        }
    }
}
