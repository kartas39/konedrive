import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// Downloads and uploads under way, the changes waiting to be uploaded, then
/// the recent events; a click shows the file in the file manager.
FormCard.FormCardPage {
    id: page

    readonly property var sync: Current.sync
    readonly property var window: QQC2.ApplicationWindow.window

    objectName: "activityPage"
    title: window ? window.accountTitle(i18nc("@title", "Activity")) : i18nc("@title", "Activity")

    /// How many changes have not gone up yet — pending, blocked and held —
    /// and the size of the files they send.
    readonly property int waitingCount: sync ? sync.pendingCount + sync.blockedCount + sync.heldCount : 0
    readonly property var waitingBytes: sync ? sync.pendingBytes + sync.blockedBytes : 0
    /// Whether the Not Uploaded page lists anything.
    readonly property bool keptBack: sync !== null && sync.notUploadedSummary.length > 0
    /// The counts the summary was last asked for at.
    property string shownCounts: ""

    /// What is kept back has no signal of its own: its summary is read when
    /// the page is shown, shows another account, or a count moves while it is.
    function loadWaiting() {
        if (visible && sync) {
            shownCounts = waitingCount + "/" + sync.blockedCount;
            sync.loadNotUploaded();
        }
    }

    onVisibleChanged: loadWaiting()
    onSyncChanged: loadWaiting()
    Connections {
        target: page.sync
        enabled: page.visible
        function onSyncChanged() {
            if (page.waitingCount + "/" + page.sync.blockedCount !== page.shownCounts) {
                page.loadWaiting();
            }
        }
        function onServiceAvailableChanged() {
            page.loadWaiting();
        }
    }

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: page.sync ? page.sync.actionError : ""
        visible: text.length > 0
    }

    FormCard.FormHeader {
        visible: page.sync !== null && page.sync.transfers.count > 0
        title: i18nc("@title:group", "Downloading now")
    }
    FormCard.FormCard {
        visible: page.sync !== null && page.sync.transfers.count > 0

        Repeater {
            model: page.sync ? page.sync.transfers : null
            delegate: FormCard.AbstractFormDelegate {
                required property string name
                required property var done
                required property var total
                required property real fraction
                Layout.fillWidth: true
                background: null
                contentItem: ColumnLayout {
                    spacing: Kirigami.Units.smallSpacing
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: name
                        elide: Text.ElideMiddle
                    }
                    QQC2.ProgressBar {
                        Layout.fillWidth: true
                        from: 0
                        to: 1
                        value: fraction
                        indeterminate: total === 0
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: i18n("%1 of %2", Qt.locale().formattedDataSize(done), Qt.locale().formattedDataSize(total))
                        opacity: 0.7
                    }
                }
            }
        }
    }


    FormCard.FormHeader {
        visible: page.sync !== null && page.sync.uploads.count > 0
        title: i18nc("@title:group", "Uploading now")
    }
    FormCard.FormCard {
        objectName: "uploadingNow"
        visible: page.sync !== null && page.sync.uploads.count > 0

        Repeater {
            model: page.sync ? page.sync.uploads : null
            delegate: FormCard.AbstractFormDelegate {
                required property string name
                required property var done
                required property var total
                required property real fraction
                Layout.fillWidth: true
                background: null
                contentItem: ColumnLayout {
                    spacing: Kirigami.Units.smallSpacing
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: name
                        elide: Text.ElideMiddle
                    }
                    QQC2.ProgressBar {
                        Layout.fillWidth: true
                        from: 0
                        to: 1
                        value: fraction
                        indeterminate: total === 0
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: i18n("%1 of %2", Qt.locale().formattedDataSize(done), Qt.locale().formattedDataSize(total))
                        opacity: 0.7
                    }
                }
            }
        }
    }

    // The outbox: every change made here that has not gone up yet, as one
    // line; what is kept back is on the Not Uploaded page.
    FormCard.FormHeader {
        visible: page.waitingCount > 0 || page.keptBack
        title: i18nc("@title:group", "Waiting to upload")
    }
    FormCard.FormCard {
        objectName: "waitingToUpload"
        visible: page.waitingCount > 0 || page.keptBack

        FormCard.FormTextDelegate {
            objectName: "waitingLine"
            visible: page.waitingCount > 0
            text: page.waitingBytes > 0 ? i18np("1 change waits to upload (%2)", "%1 changes wait to upload (%2)", page.waitingCount, Qt.locale().formattedDataSize(page.waitingBytes))
                                        : i18np("1 change waits to upload", "%1 changes wait to upload", page.waitingCount)
            leading: Kirigami.Icon {
                source: "cloud-upload"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.FormButtonDelegate {
            objectName: "keptBackLink"
            visible: page.keptBack
            text: i18n("Some files are not uploaded")
            description: i18n("The Not Uploaded page says why, and what to do.")
            icon.name: "dialog-warning"
            onClicked: page.window.showPage("notUploaded")
        }
    }

    FormCard.FormHeader {
        title: i18nc("@title:group", "Recent")
    }
    FormCard.FormCard {
        FormCard.FormPlaceholderMessageDelegate {
            visible: page.sync === null || page.sync.activity.count === 0
            text: i18n("Nothing yet")
            explanation: i18n("Downloads, uploads, freed-up files and changes from OneDrive appear here.")
            icon.name: "view-history"
        }
        Repeater {
            model: page.sync ? page.sync.activity : null
            delegate: FormCard.FormButtonDelegate {
                required property string name
                required property string path
                required property string what
                required property string detailText
                required property string iconName
                required property var time
                text: name
                icon.name: iconName
                description: (detailText.length > 0 ? i18nc("@info what happened, detail, when", "%1: %2 · %3", what, detailText, page.window.when(time))
                                                 : i18nc("@info what happened, when", "%1 · %2", what, page.window.when(time)))
                onClicked: page.sync.showInFolder(path)
            }
        }
    }
}
