import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

/// The space of one OneDrive in one bar: its whole length is the OneDrive's storage, the
/// bright part what is on this device, the pale part the rest of what is used in OneDrive,
/// and the empty part what is free. Under it the legend, with what is always kept on this
/// device. While the size of the storage is not known, the legend of this device alone.
ColumnLayout {
    id: bar

    /// What the folder's files take on this device (Folder.LocalBytes).
    required property real localBytes
    /// What is used in OneDrive and its whole storage (Account's QuotaUsed and QuotaTotal);
    /// 0 while not known.
    required property real usedBytes
    required property real totalBytes
    /// What is always kept on this device: the items with a pin, and the sizes of the files
    /// they hold (Folder's PinnedCount and PinnedBytes).
    required property int pinnedCount
    required property real pinnedBytes

    readonly property bool hasTotal: totalBytes > 0
    readonly property color localColor: Kirigami.Theme.highlightColor
    readonly property color usedColor: Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.highlightColor, 0.35)
    readonly property color freeColor: Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.12)

    spacing: Kirigami.Units.smallSpacing

    /// The part of the bar `bytes` fill.
    function part(bytes) {
        return hasTotal ? Math.max(0, Math.min(1, bytes / totalBytes)) : 0;
    }

    Rectangle {
        objectName: "storageBarTrack"
        Layout.fillWidth: true
        visible: bar.hasTotal
        implicitHeight: Kirigami.Units.gridUnit * 0.6
        radius: height / 2
        color: bar.freeColor

        // What is used in OneDrive, from the start; what is on this device lies over it. A
        // file not uploaded yet is on this device and not in OneDrive: then the bright part
        // is the longer.
        Rectangle {
            visible: width >= 1
            width: parent.width * bar.part(Math.max(bar.usedBytes, bar.localBytes))
            height: parent.height
            radius: parent.radius
            color: bar.usedColor
        }
        Rectangle {
            visible: width >= 1
            width: parent.width * bar.part(bar.localBytes)
            height: parent.height
            radius: parent.radius
            color: bar.localColor
        }
    }

    RowLayout {
        Layout.fillWidth: true
        spacing: Kirigami.Units.smallSpacing

        Rectangle {
            Layout.alignment: Qt.AlignTop
            // Beside the first line.
            Layout.topMargin: Math.max(0, (Kirigami.Units.gridUnit - implicitHeight) / 2)
            visible: bar.hasTotal
            implicitWidth: Kirigami.Units.gridUnit * 0.6
            implicitHeight: implicitWidth
            radius: 2
            color: bar.localColor
        }
        QQC2.Label {
            id: legendLocal
            objectName: "localBytesLine"
            Layout.fillWidth: true
            wrapMode: Text.Wrap
            text: bar.pinnedCount > 0
                  ? i18ncp("@info the space on this device, and what is always kept here",
                           "%2 on this device · 1 item always kept (%3)",
                           "%2 on this device · %1 items always kept (%3)",
                           bar.pinnedCount, Qt.locale().formattedDataSize(bar.localBytes), Qt.locale().formattedDataSize(bar.pinnedBytes))
                  : i18nc("@info the space on this device", "%1 on this device", Qt.locale().formattedDataSize(bar.localBytes))
        }
    }
    RowLayout {
        Layout.fillWidth: true
        visible: bar.hasTotal
        spacing: Kirigami.Units.smallSpacing

        Rectangle {
            Layout.alignment: Qt.AlignTop
            // Beside the first line.
            Layout.topMargin: Math.max(0, (Kirigami.Units.gridUnit - implicitHeight) / 2)
            implicitWidth: Kirigami.Units.gridUnit * 0.6
            implicitHeight: implicitWidth
            radius: 2
            color: bar.usedColor
        }
        QQC2.Label {
            id: legendUsed
            objectName: "quotaLine"
            Layout.fillWidth: true
            wrapMode: Text.Wrap
            text: i18nc("@info the space used in OneDrive, of all it has", "%1 used in OneDrive of %2",
                        Qt.locale().formattedDataSize(bar.usedBytes), Qt.locale().formattedDataSize(bar.totalBytes))
        }
    }
}
