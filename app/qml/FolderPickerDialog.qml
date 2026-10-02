import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.konedrive.app

/// "Folders on this computer": which folders of OneDrive are in the account's
/// folder. A tree of OneDrive's folders, read as branches open, with a check
/// mark each; "Files in the root" first; "Sync everything" above. Nothing
/// changes until Apply, which says first what leaves this computer.
Kirigami.Dialog {
    id: dialog

    /// The account it was opened for (an AccountItem); null once that account is gone.
    property QtObject item: null
    readonly property var picker: item ? item.sync.picker : null

    function openFor(account) {
        item = account;
        if (picker) {
            picker.open();
            open();
        }
    }

    objectName: "folderPickerDialog"
    title: i18nc("@title:window", "Folders on This Computer")
    preferredWidth: Kirigami.Units.gridUnit * 30
    padding: Kirigami.Units.largeSpacing
    standardButtons: Kirigami.Dialog.NoButton
    customFooterActions: [
        Kirigami.Action {
            objectName: "applySelection"
            text: i18nc("@action:button", "Apply")
            icon.name: "dialog-ok-apply"
            enabled: dialog.picker !== null && dialog.picker.ready && dialog.picker.modified && !dialog.picker.applying
            onTriggered: dialog.picker.apply()
        },
        Kirigami.Action {
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: dialog.close()
        }
    ]

    Connections {
        target: dialog.picker
        function onApplied() {
            dialog.close();
        }
    }
    // Another account shown, or this one gone: not the folders asked about.
    Connections {
        target: Current
        function onChanged() {
            dialog.close();
        }
    }

    // The dialog's content is the list itself, which scrolls: what goes above the
    // folders is its header, and what Apply would do stays in sight below them.
    ListView {
        id: tree
        objectName: "folderTree"

        readonly property bool editable: dialog.picker !== null && dialog.picker.ready && !dialog.picker.everything && !dialog.picker.applying

        implicitHeight: Kirigami.Units.gridUnit * 24
        clip: true
        model: dialog.picker
        reuseItems: false
        footerPositioning: ListView.OverlayFooter

        header: ColumnLayout {
            width: tree.width
            spacing: Kirigami.Units.smallSpacing

            QQC2.Switch {
                id: everythingSwitch
                objectName: "everythingSwitch"

                readonly property bool on: dialog.picker !== null && dialog.picker.everything

                Layout.fillWidth: true
                text: i18n("Sync everything")
                enabled: dialog.picker !== null && dialog.picker.ready && !dialog.picker.applying
                checked: on
                // A click writes `checked`, ending the binding: kept in step here.
                onOnChanged: checked = on
                onToggled: {
                    const wanted = checked;
                    checked = on;
                    dialog.picker.everything = wanted;
                }
            }
            QQC2.Label {
                Layout.fillWidth: true
                text: i18n("Only the checked folders are on this computer, with everything in them. A folder above a checked one holds only its checked sub-folders: the files directly in it are not synced.")
                wrapMode: Text.Wrap
                font: Kirigami.Theme.smallFont
                opacity: 0.8
            }
            QQC2.Label {
                objectName: "pickerIncomplete"
                Layout.fillWidth: true
                visible: dialog.picker !== null && dialog.picker.incomplete
                text: i18n("OneDrive is still being listed: folders may be missing here.")
                wrapMode: Text.Wrap
                color: Kirigami.Theme.neutralTextColor
            }
            QQC2.BusyIndicator {
                Layout.alignment: Qt.AlignHCenter
                running: visible
                visible: dialog.picker !== null && dialog.picker.loading
            }
            QQC2.CheckBox {
                objectName: "rootFilesBox"

                readonly property bool on: dialog.picker !== null && dialog.picker.rootFiles

                Layout.fillWidth: true
                leftPadding: Kirigami.Units.gridUnit * 2
                enabled: tree.editable
                text: i18n("Files in the root")
                checked: on
                onOnChanged: checked = on
                onToggled: {
                    const wanted = checked;
                    checked = on;
                    dialog.picker.rootFiles = wanted;
                }
            }
        }

        delegate: RowLayout {
            id: row

            required property int index
            required property string name
            required property int depth
            required property bool expandable
            required property bool expanded
            required property int check
            required property bool loading

            objectName: "folderRow"
            width: tree.width
            spacing: 0

            Item {
                implicitWidth: row.depth * Kirigami.Units.gridUnit
            }
            QQC2.ToolButton {
                implicitWidth: Kirigami.Units.gridUnit * 2
                icon.name: row.expanded ? "go-down" : "go-next"
                text: row.expanded ? i18nc("@action:button", "Close") : i18nc("@action:button", "Open")
                display: QQC2.AbstractButton.IconOnly
                opacity: row.expandable ? 1 : 0
                enabled: row.expandable && !row.loading
                onClicked: dialog.picker.toggle(row.index)
            }
            QQC2.CheckBox {
                id: box

                Layout.fillWidth: true
                enabled: tree.editable
                text: row.name
                tristate: true
                checkState: row.check
                // A click on an unchecked folder checks it; on a checked or partly
                // checked one, unchecks it (FolderPicker::click).
                nextCheckState: function () {
                    return row.check === Qt.Unchecked ? Qt.Checked : Qt.Unchecked;
                }
                onClicked: {
                    // The model's answer is what shows.
                    box.checkState = Qt.binding(function () {
                        return row.check;
                    });
                    dialog.picker.click(row.index);
                }
            }
        }

        footer: QQC2.Pane {
            width: tree.width
            z: 2
            padding: 0
            topPadding: Kirigami.Units.smallSpacing
            visible: dialog.picker !== null && (dialog.picker.summary.length > 0 || dialog.picker.problem.length > 0)

            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing

                // What Apply removes from this computer.
                QQC2.Label {
                    id: pickerSummary
                    objectName: "pickerSummary"
                    Layout.fillWidth: true
                    visible: text.length > 0
                    text: dialog.picker ? dialog.picker.summary : ""
                    wrapMode: Text.Wrap
                }
                // A refusal: nothing was changed, and the dialog stays open.
                Kirigami.InlineMessage {
                    id: pickerProblem
                    objectName: "pickerProblem"
                    Layout.fillWidth: true
                    type: Kirigami.MessageType.Error
                    text: dialog.picker ? dialog.picker.problem : ""
                    visible: text.length > 0
                }
            }
        }
    }
}
