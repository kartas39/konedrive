import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.kde.quickcharts as Charts
import org.konedrive.app

/// How fast things move (two mini cards with the last two minutes), the
/// downloads and uploads under way, the changes waiting to be uploaded, then
/// the recent events; a click shows the file in the file manager.
FormCard.FormCardPage {
    id: page

    readonly property var sync: Current.sync
    readonly property var window: QQC2.ApplicationWindow.window

    objectName: "activityPage"
    title: window ? window.accountTitle(i18nc("@title", "Activity")) : i18nc("@title", "Activity")

    /// The outbox has no signal of its own: it is read whenever the page is
    /// shown, or shows another account (and after its counts change).
    function loadOutbox() {
        if (visible && sync) {
            sync.loadOutbox();
        }
    }

    onVisibleChanged: loadOutbox()
    onSyncChanged: loadOutbox()

    /// The charts show speed, or how many files move at once.
    property bool showFiles: false

    /// One direction's mini card: its speed, how many files at once, and a line of the
    /// last two minutes; dimmed while nothing moves.
    component TransferCard: Kirigami.AbstractCard {
        id: card
        required property string title
        required property real speed
        required property int active
        required property var speedHistory
        required property var activeHistory
        /// The pool's line: "N files at once (pool M: X down, Y up)".
        required property string poolText
        /// Whether the chart shows files at once rather than speed.
        required property bool showFiles
        readonly property bool idle: active === 0 && speed === 0

        Layout.fillWidth: true
        Layout.preferredWidth: 1
        opacity: idle ? 0.6 : 1

        contentItem: ColumnLayout {
            spacing: Kirigami.Units.smallSpacing
            QQC2.Label {
                text: card.title
                font.bold: true
            }
            Kirigami.Heading {
                level: 2
                text: card.idle ? i18nc("@info a transfer card while nothing moves", "no transfers")
                                : i18nc("@info bytes a second", "%1/s", Qt.locale().formattedDataSize(card.speed))
            }
            QQC2.Label {
                Layout.fillWidth: true
                elide: Text.ElideRight
                opacity: 0.7
                text: card.poolText
            }
            Charts.LineChart {
                Layout.fillWidth: true
                Layout.preferredHeight: Kirigami.Units.gridUnit * 3
                fillOpacity: 0.2
                lineWidth: 1
                yRange.automatic: true
                yRange.from: 0
                valueSources: Charts.ArraySource {
                    array: card.showFiles ? card.activeHistory : card.speedHistory
                }
                colorSource: Charts.SingleValueSource {
                    value: Kirigami.Theme.highlightColor
                }
            }
        }
    }

    RowLayout {
        objectName: "transferCards"
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        visible: page.sync !== null
        spacing: Kirigami.Units.largeSpacing

        /// The pool's line for a card whose direction has `active` files moving.
        function poolText(active) {
            return page.sync ? i18ncp("@info files moving in one direction; the account's transfer pool",
                                      "%1 file at once (pool %2: %3 down, %4 up)",
                                      "%1 files at once (pool %2: %3 down, %4 up)",
                                      active, page.sync.poolSize, page.sync.activeDownloads, page.sync.activeUploads)
                             : "";
        }

        TransferCard {
            title: i18nc("@title a mini card", "Downloading")
            speed: page.sync ? page.sync.downloadSpeed : 0
            active: page.sync ? page.sync.activeDownloads : 0
            speedHistory: page.sync ? page.sync.downloadSpeedHistory : []
            activeHistory: page.sync ? page.sync.activeDownloadsHistory : []
            poolText: parent.poolText(active)
            showFiles: page.showFiles
        }
        TransferCard {
            title: i18nc("@title a mini card", "Uploading")
            speed: page.sync ? page.sync.uploadSpeed : 0
            active: page.sync ? page.sync.activeUploads : 0
            speedHistory: page.sync ? page.sync.uploadSpeedHistory : []
            activeHistory: page.sync ? page.sync.activeUploadsHistory : []
            poolText: parent.poolText(active)
            showFiles: page.showFiles
        }
    }
    QQC2.Switch {
        Layout.leftMargin: Kirigami.Units.largeSpacing
        visible: page.sync !== null
        text: i18nc("@option:check the charts show files at once rather than speed", "Show files at once")
        checked: page.showFiles
        onToggled: page.showFiles = checked
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

    // The outbox: every change made here that has not gone up yet.
    FormCard.FormHeader {
        visible: page.sync !== null && page.sync.outbox.total > 0
        title: i18nc("@title:group", "Waiting to upload")
    }
    FormCard.FormCard {
        objectName: "waitingToUpload"
        visible: page.sync !== null && page.sync.outbox.total > 0

        Repeater {
            model: page.sync ? page.sync.outbox : null
            delegate: FormCard.FormButtonDelegate {
                required property string name
                required property string path
                required property string stateText
                required property string why
                required property string iconName
                text: name
                icon.name: iconName
                description: why.length > 0 ? i18nc("@info outbox row: where it stands, why", "%1: %2", stateText, why) : stateText
                onClicked: page.sync.showInFolder(path)
            }
        }
        FormCard.FormTextDelegate {
            visible: page.sync !== null && page.sync.outbox.total > page.sync.outbox.count
            text: page.sync ? i18np("and 1 more", "and %1 more", page.sync.outbox.total - page.sync.outbox.count) : ""
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
