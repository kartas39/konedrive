import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.kde.quickcharts as Charts
import org.kde.quickcharts.controls as ChartsControls
import org.konedrive.app

/// How fast things move and how much is left (two mini cards with the last two
/// minutes), the downloads and uploads under way, the way to what is kept back,
/// then the recent events; a click shows the file in the file manager.
FormCard.FormCardPage {
    id: page

    readonly property var sync: Current.sync
    readonly property var window: QQC2.ApplicationWindow.window

    objectName: "activityPage"
    title: window ? window.accountTitle(i18nc("@title", "Activity")) : i18nc("@title", "Activity")

    /// How many changes have not gone up yet — pending, blocked and held.
    readonly property int waitingCount: sync ? sync.pendingCount + sync.blockedCount + sync.heldCount : 0
    /// Whether the Not Uploaded page lists anything, and how many changes it counts.
    readonly property bool keptBack: sync !== null && sync.notUploadedSummary.length > 0
    readonly property int keptBackCount: sync ? sync.notUploadedSummary.reduce((n, row) => n + row.count, 0) : 0

    /// A queue's time left: "about 12 min".
    function timeLeftText(seconds) {
        const minutes = Math.ceil(seconds / 60);
        if (seconds < 60) {
            return i18nc("@info time left, seconds", "about %1 s", seconds);
        }
        if (minutes < 60) {
            return i18nc("@info time left, minutes", "about %1 min", minutes);
        }
        if (seconds < 86400) {
            return minutes % 60 === 0 ? i18nc("@info time left, hours", "about %1 h", Math.floor(minutes / 60))
                                      : i18nc("@info time left, hours and minutes", "about %1 h %2 min", Math.floor(minutes / 60), minutes % 60);
        }
        const hours = Math.ceil(seconds / 3600);
        return hours % 24 === 0 ? i18nc("@info time left, days", "about %1 d", Math.floor(hours / 24))
                                : i18nc("@info time left, days and hours", "about %1 d %2 h", Math.floor(hours / 24), hours % 24);
    }
    /// The counts the summary was last asked for at.
    property string shownCounts: ""

    /// The counts that move what is kept back: a full OneDrive and the files
    /// too big for it change reasons, not the pending count.
    function countsNow() {
        return sync ? waitingCount + "/" + sync.blockedCount + "/" + sync.quotaFull + "/" + sync.spaceWaitingCount + "/" + sync.tooBigCount : "";
    }

    /// What is kept back has no signal of its own: its summary is read when
    /// the page is shown, shows another account, or a count moves while it is.
    function loadWaiting() {
        if (visible && sync) {
            shownCounts = countsNow();
            sync.loadNotUploaded();
        }
    }

    onVisibleChanged: loadWaiting()
    onSyncChanged: loadWaiting()
    Connections {
        target: page.sync
        enabled: page.visible
        function onSyncChanged() {
            if (page.countsNow() !== page.shownCounts) {
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
        /// What is left that way (issue #16): how many, the bytes, about how long (empty:
        /// unknown), and the bytes done in this run.
        required property int leftCount
        required property real leftBytes
        required property string timeText
        required property real doneBytes
        /// "N files left" or "N changes left".
        required property string leftText
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
            // "1 234 files left · 48.2 GiB · about 12 min", and what this run has done.
            QQC2.Label {
                objectName: card.objectName + "Left"
                Layout.fillWidth: true
                elide: Text.ElideRight
                visible: card.leftCount > 0
                text: {
                    const parts = [card.leftText];
                    if (card.leftBytes > 0) {
                        parts.push(Qt.locale().formattedDataSize(card.leftBytes));
                    }
                    if (card.timeText.length > 0) {
                        parts.push(card.timeText);
                    }
                    return parts.join(" · ");
                }
            }
            QQC2.Label {
                objectName: card.objectName + "Done"
                Layout.fillWidth: true
                elide: Text.ElideRight
                visible: card.leftCount > 0
                font: Kirigami.Theme.smallFont
                opacity: 0.7
                text: i18nc("@info bytes moved in this run", "%1 done", Qt.locale().formattedDataSize(card.doneBytes))
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
            objectName: "downloadingCard"
            title: i18nc("@title a mini card", "Downloading")
            speed: page.sync ? page.sync.downloadSpeed : 0
            active: page.sync ? page.sync.activeDownloads : 0
            speedHistory: page.sync ? page.sync.downloadSpeedHistory : []
            activeHistory: page.sync ? page.sync.activeDownloadsHistory : []
            filesText: i18ncp("@info files downloading at once", "%1 file downloading", "%1 files downloading", downloading.active)
            filesLegend: i18nc("@info chart legend", "Files downloading")
            leftCount: page.sync ? page.sync.downloadLeftCount : 0
            leftBytes: page.sync ? page.sync.downloadLeftBytes : 0
            timeText: page.sync && page.sync.downloadTimeLeft > 0 ? page.timeLeftText(page.sync.downloadTimeLeft) : ""
            doneBytes: page.sync ? page.sync.downloadDoneBytes : 0
            leftText: i18ncp("@info files left to download", "%1 file left", "%1 files left", downloading.leftCount)
        }
        TransferCard {
            id: uploading
            objectName: "uploadingCard"
            title: i18nc("@title a mini card", "Uploading")
            speed: page.sync ? page.sync.uploadSpeed : 0
            active: page.sync ? page.sync.activeUploads : 0
            speedHistory: page.sync ? page.sync.uploadSpeedHistory : []
            activeHistory: page.sync ? page.sync.activeUploadsHistory : []
            filesText: i18ncp("@info files uploading at once", "%1 file uploading", "%1 files uploading", uploading.active)
            filesLegend: i18nc("@info chart legend", "Files uploading")
            leftCount: page.sync ? page.sync.uploadLeftCount : 0
            leftBytes: page.sync ? page.sync.uploadLeftBytes : 0
            timeText: page.sync && page.sync.uploadTimeLeft > 0 ? page.timeLeftText(page.sync.uploadTimeLeft) : ""
            doneBytes: page.sync ? page.sync.uploadDoneBytes : 0
            leftText: i18ncp("@info changes left to upload", "%1 change left", "%1 changes left", uploading.leftCount)
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
            const pool = i18nc("@info the account's transfer pool: %1 slots in use now (may be above %2), %2 the pool's size now, %3 large files moving now, %4 the streams of large files now, %5 how many such streams may run at once",
                               "Pool: %1 of %2 · large files: %3 (%4 of %5 streams)",
                               page.sync.poolInUse, page.sync.poolSize, page.sync.largeFiles, page.sync.largeTransfers, page.sync.largeLimit);
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

    // What is kept back is on the Not Uploaded page; what is left to upload is counted in
    // the Uploading card.
    FormCard.FormHeader {
        visible: page.keptBack
        title: i18nc("@title:group", "Waiting to upload")
    }
    FormCard.FormCard {
        objectName: "waitingToUpload"
        visible: page.keptBack

        FormCard.FormButtonDelegate {
            objectName: "keptBackLink"
            visible: page.keptBack
            text: i18ncp("@action:button", "1 change kept back — see Not Uploaded", "%1 changes kept back — see Not Uploaded", page.keptBackCount)
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
