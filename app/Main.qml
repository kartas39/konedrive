import QtCore
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Dialogs
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.konedrive.app

/// The window: pages chosen from a sidebar on the left, which folds into a
/// drawer behind a menu button when the window is narrow. The OneDrive
/// switcher heads the sidebar; the pages under it show the OneDrive chosen
/// there — an account, its folder and how it syncs, as one — and Settings,
/// below them, is the whole app's. The folder picker and the mode dialog are
/// the window's, opened from several pages.
Kirigami.ApplicationWindow {
    id: root

    /// The page shown: overview, issues, activity, oneDriveSettings or settings.
    property string currentPage: "overview"
    /// Wide enough for the sidebar beside a page.
    readonly property bool sidebarFits: width >= Kirigami.Units.gridUnit * 36
    /// An account is chosen: the per-account pages have something to show.
    readonly property bool hasAccount: Current.account !== null
    /// The account just given a folder here while read-only: the Overview asks how it
    /// syncs, until that is answered or another account is set up.
    property string modeChoiceFor: ""
    /// The home directory, and the folder offered first for a OneDrive: ~/OneDrive.
    readonly property string home: decodeURIComponent(StandardPaths.writableLocation(StandardPaths.HomeLocation).toString().replace(/^file:\/\//, ""))
    readonly property string defaultFolder: home + "/OneDrive"

    title: i18nc("@title:window", "KOneDrive")
    // main.cpp shows the window unless started with --background.
    visible: false
    width: Kirigami.Units.gridUnit * 46
    height: Kirigami.Units.gridUnit * 36
    minimumWidth: Kirigami.Units.gridUnit * 20
    minimumHeight: Kirigami.Units.gridUnit * 20

    /// Shows one of the pages, by name: a sidebar page, or one of the two lists that
    /// Issues leads to (conflicts, skipped), shown over it. With no account, the
    /// per-account pages give way to the Overview, which leads through setting one up.
    function showPage(name) {
        const pages = {
            "overview": overviewPage,
            "issues": issuesPage,
            "activity": activityPage,
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
        if (!hasAccount && name !== "settings") {
            name = "overview";
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

    /// A per-account page's title: with more than one account it names the
    /// account too ("Overview · Personal"), since a narrow window hides the switcher.
    function accountTitle(title) {
        if (Accounts.count > 1 && Current.account) {
            return i18nc("@title page title, account label", "%1 · %2", title, Current.account.label);
        }
        return title;
    }

    /// Straight to Microsoft's sign-in in the browser.
    function signIn() {
        if (Accounts.adding) {
            return;
        }
        Accounts.clearAddError();
        Accounts.addAccount("");
    }

    /// The mode dialog, for the account shown.
    function changeMode() {
        if (Current.item) {
            modeDialog.openFor(Current.item);
        }
    }

    /// The folder picker, for `item` (an AccountItem).
    function pickFolder(item) {
        if (item) {
            folderDialog.openFor(item);
        }
    }

    /// Registers `path` as the folder of `item`; a read-only account is then asked how it syncs.
    function useFolder(item, path) {
        if (!item) {
            return;
        }
        if (item.account.mode === "read-only") {
            modeChoiceFor = item.path;
        }
        item.sync.chooseFolder("file://" + path);
    }

    /// Whether an account's folder is `path`.
    function folderInUse(path) {
        for (let i = 0; i < accountFolders.count; ++i) {
            const folder = accountFolders.objectAt(i);
            if (folder && folder.rootPath === path) {
                return true;
            }
        }
        return false;
    }

    /// A path as people read it: the home directory as ~.
    function shownPath(path) {
        return path === home || path.startsWith(home + "/") ? "~" + path.slice(home.length) : path;
    }

    /// A unix time as the time of day today, or a short date and time before.
    function when(unixSeconds) {
        const date = new Date(unixSeconds * 1000);
        if (date.toDateString() === new Date().toDateString()) {
            return date.toLocaleTimeString(Qt.locale(), Locale.ShortFormat);
        }
        return date.toLocaleString(Qt.locale(), Locale.ShortFormat);
    }

    // Every account's folder, for folderInUse().
    Instantiator {
        id: accountFolders
        model: Accounts
        delegate: QtObject {
            required property string rootPath
        }
    }

    Connections {
        target: Accounts
        function onOpenUrlRequested(url) {
            Qt.openUrlExternally(url);
        }
        // Sign In named and chose this account: the Overview offers it a folder next.
        function onAccountAdded(path) {
            root.modeChoiceFor = "";
            Current.select(path);
            root.showPage("overview");
        }
    }
    Connections {
        target: Current
        function onChanged() {
            if (!root.hasAccount && root.currentPage !== "settings") {
                root.showPage("overview");
            }
        }
    }

    Component.onCompleted: pageStack.push(overviewPage)
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
            onAddRequested: root.signIn()
        }

        // The pages of the OneDrive chosen above.
        Repeater {
            model: [
                { name: "overview", text: i18nc("@title sidebar", "Overview"), icon: Current.status ? Current.status.iconName : "state-offline" },
                { name: "issues", text: i18nc("@title sidebar", "Issues"), icon: "dialog-warning" },
                { name: "activity", text: i18nc("@title sidebar", "Activity"), icon: "view-history" },
                { name: "oneDriveSettings", text: i18nc("@title sidebar", "OneDrive Settings"), icon: "folder-cloud" },
            ]
            delegate: SidebarEntry {
                required property var modelData
                name: modelData.name
                text: modelData.text
                icon.name: modelData.icon
                enabled: root.hasAccount || modelData.name === "overview"
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

    FolderDialog {
        id: folderDialog
        objectName: "folderDialog"

        /// The account it was opened for (an AccountItem); null once that account is gone.
        property QtObject item: null

        function openFor(account) {
            item = account;
            open();
        }

        title: i18nc("@title:window", "Choose an Empty Folder for OneDrive")
        currentFolder: StandardPaths.writableLocation(StandardPaths.HomeLocation)
        onAccepted: {
            if (item) {
                if (item.account.mode === "read-only") {
                    root.modeChoiceFor = item.path;
                }
                item.sync.chooseFolder(selectedFolder);
            }
            item = null;
        }
        onRejected: item = null
    }

    ModeDialog {
        id: modeDialog
    }

    // The pages live for the window's lifetime: the page row borrows the one
    // shown and hands it back here (hidden) when another is chosen.
    Item {
        visible: false

        OverviewPage {
            id: overviewPage
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
        OneDriveSettingsPage {
            id: oneDriveSettingsPage
        }
        SettingsPage {
            id: settingsPage
        }
    }
}
