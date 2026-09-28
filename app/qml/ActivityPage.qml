import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.kde.quickcharts as Charts
import org.kde.quickcharts.controls as ChartsControls
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

    /// One direction's mini card: its speed, how many files move that way at once, and one
    /// chart of the last two minutes with two lines on two scales — speed on the left axis,
    /// the files moving that way on the right — and a small legend. Dimmed while nothing moves.
    component TransferCard: Kirigami.AbstractCard {
        id: card
        required property string title
        required property real speed
        required property int active
        required property var speedHistory
        required property var activeHistory
        /// "N files downloading" or "N files uploading".
        required property string filesText
        /// The legend of the files line: "Files downloading" or "Files uploading".
        required property string filesLegend
        readonly property bool idle: active === 0 && speed === 0
        readonly property color speedColor: Kirigami.Theme.highlightColor
        readonly property color filesColor: Kirigami.Theme.neutralTextColor

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
                text: card.filesText
            }
            RowLayout {
                Layout.fillWidth: true
                Layout.preferredHeight: Kirigami.Units.gridUnit * 3
                spacing: Kirigami.Units.smallSpacing

                // Left axis: speed.
                ChartsControls.AxisLabels {
                    Layout.fillHeight: true
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 3
                    direction: ChartsControls.AxisLabels.VerticalBottomTop
                    source: Charts.ChartAxisSource {
                        chart: speedChart
                        axis: Charts.ChartAxisSource.YAxis
                        itemCount: 2
                    }
                    delegate: QQC2.Label {
                        font: Kirigami.Theme.smallFont
                        color: card.speedColor
                        text: Qt.locale().formattedDataSize(Number(ChartsControls.AxisLabels.label), 0) + "/s"
                    }
                }
                Item {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    Charts.LineChart {
                        id: speedChart
                        anchors.fill: parent
                        fillOpacity: 0.2
                        lineWidth: 1
                        yRange.from: 0
                        yRange.automatic: true
                        valueSources: Charts.ArraySource { array: card.speedHistory }
                        colorSource: Charts.SingleValueSource { value: card.speedColor }
                    }
                    Charts.LineChart {
                        id: filesChart
                        anchors.fill: parent
                        fillOpacity: 0
                        lineWidth: 1
                        yRange.from: 0
                        yRange.automatic: true
                        valueSources: Charts.ArraySource { array: card.activeHistory }
                        colorSource: Charts.SingleValueSource { value: card.filesColor }
                    }
                }
                // Right axis: the files moving.
                ChartsControls.AxisLabels {
                    Layout.fillHeight: true
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 1.5
                    direction: ChartsControls.AxisLabels.VerticalBottomTop
                    source: Charts.ChartAxisSource {
                        chart: filesChart
                        axis: Charts.ChartAxisSource.YAxis
                        itemCount: 2
                    }
                    delegate: QQC2.Label {
                        font: Kirigami.Theme.smallFont
                        color: card.filesColor
                        text: Math.round(Number(ChartsControls.AxisLabels.label))
                    }
                }
            }
            // The legend.
            RowLayout {
                spacing: Kirigami.Units.smallSpacing
                Rectangle {
                    implicitWidth: Kirigami.Units.gridUnit * 0.6
                    implicitHeight: 2
                    color: card.speedColor
                }
                QQC2.Label {
                    font: Kirigami.Theme.smallFont
                    text: i18nc("@info chart legend", "Speed")
                }
                Rectangle {
                    Layout.leftMargin: Kirigami.Units.smallSpacing
                    implicitWidth: Kirigami.Units.gridUnit * 0.6
                    implicitHeight: 2
                    color: card.filesColor
                }
                QQC2.Label {
                    font: Kirigami.Theme.smallFont
                    text: card.filesLegend
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

        TransferCard {
            id: downloading
            title: i18nc("@title a mini card", "Downloading")
            speed: page.sync ? page.sync.downloadSpeed : 0
            active: page.sync ? page.sync.activeDownloads : 0
            speedHistory: page.sync ? page.sync.downloadSpeedHistory : []
            activeHistory: page.sync ? page.sync.activeDownloadsHistory : []
            filesText: i18ncp("@info files downloading at once", "%1 file downloading", "%1 files downloading", downloading.active)
            filesLegend: i18nc("@info chart legend", "Files downloading")
        }
        TransferCard {
            id: uploading
            title: i18nc("@title a mini card", "Uploading")
            speed: page.sync ? page.sync.uploadSpeed : 0
            active: page.sync ? page.sync.activeUploads : 0
            speedHistory: page.sync ? page.sync.uploadSpeedHistory : []
            activeHistory: page.sync ? page.sync.activeUploadsHistory : []
            filesText: i18ncp("@info files uploading at once", "%1 file uploading", "%1 files uploading", uploading.active)
            filesLegend: i18nc("@info chart legend", "Files uploading")
        }
    }
    // The account's transfer pool, shared by both directions: shown once, with the large
    // transfers under way, and OneDrive's Retry-After counting down while it runs.
    QQC2.Label {
        objectName: "transferPool"
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        Layout.fillWidth: true
        wrapMode: Text.Wrap
        visible: page.sync !== null
        opacity: 0.7
        text: {
            if (!page.sync) {
                return "";
            }
            const pool = i18nc("@info the account's transfer pool: slots now, ceiling, large transfers now, their limit",
                               "Pool: %1 of %2 (large: %3 of %4)",
                               page.sync.poolSize, page.sync.poolCeiling, page.sync.largeTransfers, page.sync.largeLimit);
            return page.sync.retryAfter > 0
                ? i18nc("@info the pool line during OneDrive's Retry-After, seconds left", "%1 — OneDrive asked to wait %2 s", pool, page.sync.retryAfter)
                : pool;
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
