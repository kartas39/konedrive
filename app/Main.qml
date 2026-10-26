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
/// Status; with no folder, its Status, where the folder is chosen, and its
/// OneDrive Settings. Adding a OneDrive opens the browser at once, and the
/// dialog that picks its folder once it is added.
Kirigami.ApplicationWindow {
    id: root

    /// The page shown: signIn, status, issues, oneDriveSettings or settings.
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
            return hasService && hasFolder;
        case "oneDriveSettings":
            return hasService && hasAccount;
        case "settings":
            return true;
        }
        return false;
    }

    /// Shows one of the pages, by name: a sidebar page, or the page that adds a OneDrive. A
    /// page with nothing to show gives way to the account's Status, or with no account to
    /// adding one.
    function showPage(name) {
        const pages = {
            "signIn": signInPage,
            "status": statusPage,
            "issues": issuesPage,
            "oneDriveSettings": oneDriveSettingsPage,
            "settings": settingsPage,
        };
        if (!pages[name]) {
            return;
        }
        if (!pageShows(name)) {
            name = hasService && !hasAccount ? "signIn" : "status";
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

    /// Shows where a notification leads, by its name (sectionName() of a WindowSection):
    /// Status, Status with its History opened, or a section of Issues brought to the top.
    function showSection(name) {
        if (name === "history") {
            showPage("status");
            if (currentPage === "status") {
                statusPage.showHistory();
            }
            return;
        }
        const issuesSections = ["cantUpload", "keptBothVersions", "notInFolder"];
        if (!issuesSections.includes(name)) {
            showPage(name);
            return;
        }
        showPage("issues");
        if (currentPage === "issues") {
            issuesPage.showSection(name);
        }
    }

    /// The page shown again once what it shows changes: one with nothing left to show gives way.
    function reconsiderPage() {
        if (!pageShows(currentPage)) {
            showPage(currentPage);
        }
    }

    /// Adding a OneDrive: Microsoft's sign-in in the browser at once, and the page that
    /// waits for it.
    function addOneDrive() {
        if (!Accounts.adding) {
            Accounts.clearAddError();
            Accounts.addAccount("");
        }
        showPage("signIn");
    }

    /// Opens the dialog that picks the folder of `item` (an AccountItem).
    function chooseFolder(item) {
        if (item) {
            folderPicker.openFor(item);
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

    Connections {
        target: Accounts
        function onOpenUrlRequested(url) {
            Qt.openUrlExternally(url);
        }
        // Sign In named and chose this account: its Status, and the dialog that picks its folder.
        function onAccountAdded(path) {
            Current.select(path);
            root.showPage("status");
            root.chooseFolder(Accounts.find(path));
        }
        // A sign-in given up or ended with nothing added while there is an account: its
        // Status again. One that failed stays, saying why.
        function onAddingChanged() {
            if (!Accounts.adding && Accounts.addError.length === 0 && root.currentPage === "signIn" && root.hasAccount) {
                root.showPage("status");
            }
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
            onAddRequested: root.addOneDrive()
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

        // This build, on every page, for bug reports: selectable, to copy. Below it a newer
        // release, and a service that runs another build.
        footer: ColumnLayout {
            spacing: 0

            Kirigami.Separator {
                Layout.fillWidth: true
            }
            QQC2.TextArea {
                objectName: "versionLine"
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.smallSpacing
                Layout.rightMargin: Kirigami.Units.smallSpacing
                Layout.topMargin: Kirigami.Units.smallSpacing
                Layout.bottomMargin: releaseLine.visible || serviceVersionLine.visible ? 0 : Kirigami.Units.smallSpacing
                text: Daemon.versionLine
                readOnly: true
                selectByMouse: true
                wrapMode: TextEdit.Wrap
                background: null
                padding: 0
                font: Kirigami.Theme.smallFont
                opacity: 0.7
            }
            // A newer release is out: its page, where its packages are.
            Kirigami.UrlButton {
                id: releaseLine

                objectName: "releaseLine"
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.smallSpacing
                Layout.rightMargin: Kirigami.Units.smallSpacing
                Layout.bottomMargin: serviceVersionLine.visible ? 0 : Kirigami.Units.smallSpacing
                visible: ReleaseCheck.newerVersion.length > 0
                text: i18nc("@info the version of a newer release", "Update available: %1", ReleaseCheck.newerVersion)
                url: ReleaseCheck.newerPage.toString()
                font: Kirigami.Theme.smallFont
                elide: Text.ElideRight
            }
            // The running service is another build (installed, not restarted).
            QQC2.TextArea {
                id: serviceVersionLine

                objectName: "serviceVersionLine"
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.smallSpacing
                Layout.rightMargin: Kirigami.Units.smallSpacing
                Layout.bottomMargin: Kirigami.Units.smallSpacing
                visible: text.length > 0
                text: Daemon.daemonBuildMismatch
                readOnly: true
                selectByMouse: true
                wrapMode: TextEdit.Wrap
                background: null
                padding: 0
                font: Kirigami.Theme.smallFont
                color: Kirigami.Theme.neutralTextColor
            }
        }
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
        OneDriveSettingsPage {
            id: oneDriveSettingsPage
        }
        SettingsPage {
            id: settingsPage
        }
    }

    FolderPicker {
        id: folderPicker
    }
}
