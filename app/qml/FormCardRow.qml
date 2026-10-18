import QtQuick
import org.kde.kirigami as Kirigami

/// A row of a ListPage's list, on a card-coloured band as wide as a FormCard. What it holds
/// is anchored across the band, and the row takes its height from it.
Item {
    default property alias content: band.data

    width: ListView.view ? ListView.view.width : 0

    Rectangle {
        id: band

        Kirigami.Theme.colorSet: Kirigami.Theme.View
        Kirigami.Theme.inherit: false
        anchors.horizontalCenter: parent.horizontalCenter
        width: Math.min(parent.width, Kirigami.Units.gridUnit * 30)
        height: parent.height
        color: Kirigami.Theme.backgroundColor
    }
}
