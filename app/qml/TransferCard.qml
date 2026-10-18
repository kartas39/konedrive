import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.quickcharts as Charts
import org.kde.quickcharts.controls as ChartsControls

/// One direction's card on Status: its speed, how many files move that way at once, what is
/// left that way, and one chart of the last two minutes with two lines on two scales — speed
/// on the left axis, the files moving that way on the right — with a small legend; under it
/// the lines it is given (`lines`). Dimmed while nothing moves.
Kirigami.AbstractCard {
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
    /// What else moves or waits that way, under the legend.
    default property alias lines: lines.data
    readonly property bool idle: active === 0 && speed === 0
    readonly property color speedColor: Kirigami.Theme.highlightColor
    readonly property color filesColor: Kirigami.Theme.neutralTextColor

    Layout.fillWidth: true
    Layout.preferredWidth: 1
    // Both ways' cards as tall as the taller.
    Layout.fillHeight: true
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
            Layout.fillHeight: false
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
                    valueSources: Charts.ArraySource {
                        array: card.speedHistory
                    }
                    colorSource: Charts.SingleValueSource {
                        value: card.speedColor
                    }
                }
                Charts.LineChart {
                    id: filesChart
                    anchors.fill: parent
                    fillOpacity: 0
                    lineWidth: 1
                    yRange.from: 0
                    yRange.automatic: true
                    valueSources: Charts.ArraySource {
                        array: card.activeHistory
                    }
                    colorSource: Charts.SingleValueSource {
                        value: card.filesColor
                    }
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
        ColumnLayout {
            id: lines
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing
        }
        // What the taller card's height leaves over, under all of it.
        Item {
            Layout.fillHeight: true
        }
    }
}
