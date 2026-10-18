import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.konedrive.app

/// The window: pages chosen from a sidebar on the left, which folds into a
/// drawer behind a menu button when the window is narrow. The OneDrive
/// switcher heads the sidebar; the pages under it show the OneDrive chosen
/// there — an account, its folder and how it syncs, as one — and Settings,
/// below them, is the whole app's. A page shows only when there is something
/// on it to see or do: with no account, adding one; with no service, its
/// Status; with no folder, its Status, where the folder is chosen.
Kirigami.ApplicationWindow {
    id: root

    /// The page shown: signIn, status, issues, activity, storage, oneDriveSettings or settings.
    property string currentPage: "status"
    /// Wide enough for the sidebar beside a page.
    readonly property bool sidebarFits: width >= Kirigami.Units.gridUnit * 36
    readonly property bool hasService: Daemon.serviceAvailable
    /// An account is chosen: the per-account pages have something to show.
    readonly property bool hasAccount: Current.account !== null
    /// And it has a folder: the pages about the folder have something to show.
    readonly property bool hasFolder: hasAccount && Current.sync !== null && Current.sync.rootPath.length > 0

    title: i18nc("@title:window", "KOneDrive")
    // main.cpp shows the window unless started with --background.
    visible: false
    width: Kirigami.Units.gridUnit * 46
    height: Kirigami.Units.gridUnit * 36
    minimumWidth: Kirigami.Units.gridUnit * 20
    minimumHeight: Kirigami.Units.gridUnit * 20

    /// Whether the page `name` has something to show now.
    function pageShows(name) {
        switch (name) {
        case "signIn":
            return hasService;
        case "status":
            return !hasService || hasAccount;
        case "issues":
        case "conflicts":
        case "skipped":
        case "activity":
        case "storage":
        case "oneDriveSettings":
            return hasService && hasFolder;
        case "settings":
            return true;
        }
        return false;
    }

    /// Shows one of the pages, by name: a sidebar page, the page that adds a OneDrive, or
    /// one of the two lists that Issues leads to (conflicts, skipped), shown over it. A page
    /// with nothing to show gives way to the account's Status, or with no account to adding one.
    function showPage(name) {
        const pages = {
            "signIn": signInPage,
            "status": statusPage,
            "issues": issuesPage,
            "activity": activityPage,
            "storage": storagePage,
            "oneDriveSettings": oneDriveSettingsPage,
            "settings": settingsPage,
        };
        const lists = {
            "conflicts": conflictsPage,
            "skipped": skippedPage,
        };
        if (!pages[name] && !lists[name]) {
            return;
        }
        if (!pageShows(name)) {
            name = hasService && !hasAccount ? "signIn" : "status";
        }
        const sidebarPage = lists[name] ? "issues" : name;
        if (sidebarPage !== currentPage || pageStack.depth === 0 || (!lists[name] && pageStack.depth > 1)) {
            currentPage = sidebarPage;
            pageStack.clear();
            pageStack.push(pages[sidebarPage]);
        }
        if (lists[name]) {
            if (pageStack.depth > 1) {
                pageStack.pop(pageStack.get(0));
            }
            pageStack.push(lists[name]);
        }
        if (drawer.modal) {
            drawer.close();
        }
    }

    /// The page shown again once what it shows changes: one with nothing left to show gives way.
    function reconsiderPage() {
        if (!pageShows(currentPage)) {
            showPage(currentPage);
        }
    }

    /// A per-account page's title: with more than one account it names the
    /// account too ("Status · Personal"), since a narrow window hides the switcher.
    function accountTitle(title) {
        if (Accounts.count > 1 && Current.account) {
            return i18nc("@title page title, account label", "%1 · %2", title, Current.account.label);
        }
        return title;
    }

    /// The mode dialog, for the account shown.
    function changeMode() {
        if (Current.item) {
            modeDialog.openFor(Current.item);
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
        target: Accounts
        function onOpenUrlRequested(url) {
            Qt.openUrlExternally(url);
        }
        // Sign In named and chose this account: its Status offers it a folder next.
        function onAccountAdded(path) {
            Current.select(path);
            root.showPage("status");
        }
    }
    Connections {
        target: Current
        function onChanged() {
            root.reconsiderPage();
        }
    }
    onHasServiceChanged: reconsiderPage()
    onHasAccountChanged: reconsiderPage()
    onHasFolderChanged: reconsiderPage()

    Component.onCompleted: showPage(pageShows("status") ? "status" : "signIn")
    pageStack.globalToolBar.showNavigationButtons: Kirigami.ApplicationHeaderStyle.NoNavigationButtons

    component SidebarEntry: QQC2.ItemDelegate {
        id: entry

        required property string name
        property int badge: 0

        objectName: "sidebar-" + name
        Layout.fillWidth: true
        highlighted: root.currentPage === name
        onClicked: root.showPage(name)

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
            // How many things need the user, when any do.
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

        header: AccountSwitcher {
            visible: Accounts.count > 0
            onAddRequested: root.showPage("signIn")
        }

        // With no account: adding one.
        SidebarEntry {
            name: "signIn"
            visible: root.hasService && !root.hasAccount
            text: i18nc("@title sidebar", "Add OneDrive")
            icon.name: "list-add-user"
        }
        // The pages of the OneDrive chosen above.
        // The model is fixed: a model that changes makes the delegates anew, and a layout
        // keeps hold of the hidden ones it had.
        Repeater {
            model: [
                { name: "status", text: i18nc("@title sidebar", "Status"), icon: "" },
                { name: "issues", text: i18nc("@title sidebar", "Issues"), icon: "dialog-warning" },
                { name: "activity", text: i18nc("@title sidebar", "Activity"), icon: "view-history" },
                { name: "storage", text: i18nc("@title sidebar", "Storage"), icon: "drive-harddisk" },
                { name: "oneDriveSettings", text: i18nc("@title sidebar", "OneDrive Settings"), icon: "folder-cloud" },
            ]
            delegate: SidebarEntry {
                required property var modelData
                name: modelData.name
                text: modelData.text
                // Status's icon is the account's state.
                icon.name: modelData.name === "status" ? (Current.status ? Current.status.iconName : "state-offline") : modelData.icon
                visible: root.pageShows(modelData.name)
                // Both versions kept, and the changes that need the user to go up.
                badge: modelData.name === "issues" && Current.sync ? Current.sync.conflictCount + Current.sync.blockedCount : 0
            }
        }
        Kirigami.Separator {
            Layout.fillWidth: true
            Layout.topMargin: Kirigami.Units.smallSpacing
            Layout.bottomMargin: Kirigami.Units.smallSpacing
        }
        // The whole app's.
        SidebarEntry {
            name: "settings"
            text: i18nc("@title sidebar", "Settings")
            icon.name: "settings-configure"
        }
        Item {
            Layout.fillHeight: true
        }
    }

    ModeDialog {
        id: modeDialog
    }

    // The pages live for the window's lifetime: the page row borrows the one
    // shown and hands it back here (hidden) when another is chosen.
    Item {
        visible: false

        SignInPage {
            id: signInPage
        }
        StatusPage {
            id: statusPage
        }
        IssuesPage {
            id: issuesPage
        }
        ConflictsPage {
            id: conflictsPage
        }
        SkippedPage {
            id: skippedPage
        }
        ActivityPage {
            id: activityPage
        }
        StoragePage {
            id: storagePage
        }
        OneDriveSettingsPage {
            id: oneDriveSettingsPage
        }
        SettingsPage {
            id: settingsPage
        }
    }
}
