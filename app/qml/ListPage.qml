import QtQuick
import org.kde.kirigami as Kirigami

/// A page that is one long list, its ListView child, drawn as a FormCardPage is: the same
/// background, and rows on card-coloured bands (FormCardRow). A FormCardPage's one column
/// would build every row; the ListView builds only those in sight.
Kirigami.ScrollablePage {
    // Kept out of the page stack the page takes its implicit height, which
    // would otherwise be the whole list's: the ListView would build every row
    // there and keep them once shown.
    implicitHeight: Kirigami.Units.gridUnit * 20

    topPadding: 0
    bottomPadding: 0
    leftPadding: 0
    rightPadding: 0

    // As FormCardPage's.
    background: Rectangle {
        Kirigami.Theme.colorSet: Kirigami.Theme.Window
        Kirigami.Theme.inherit: false

        Item {
            id: view

            Kirigami.Theme.colorSet: Kirigami.Theme.View
            Kirigami.Theme.inherit: false
        }

        color: Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, view.Kirigami.Theme.backgroundColor, 0.5)
    }
}
