import QtQuick
import org.kde.kirigami as Kirigami
import org.konedrive.app

/// A question about the account shown whose answer leaves its folder behind: it says first
/// how many changes made here are not uploaded and stay on this device only (UnsentCount,
/// read as it opens), then `question`. It acts on the account it was opened for, and closes
/// when another is shown.
Kirigami.PromptDialog {
    id: dialog

    /// The account it was opened for (an AccountItem); null once that account is gone.
    property QtObject item: null
    /// How many changes made here are not uploaded, read when the question is asked; 0 until
    /// it is answered, and when it cannot be read.
    property double unsentCount: 0
    /// What the answer does, after the changes not uploaded.
    property string question

    subtitle: unsentCount > 0
              ? i18np("1 change made here is not uploaded yet: it stays on this device only, as a plain file.",
                      "%1 changes made here are not uploaded yet: they stay on this device only, as plain files.",
                      unsentCount) + "\n\n" + question
              : question
    onAboutToShow: {
        item = Current.item;
        unsentCount = 0;
        if (item) {
            item.sync.readUnsentCount();
        }
    }
    dialogType: Kirigami.PromptDialog.Warning
    maximumWidth: Math.min(absoluteMaximumWidth, Kirigami.Units.gridUnit * 30)
    standardButtons: Kirigami.Dialog.NoButton

    Connections {
        target: dialog.item ? dialog.item.sync : null
        function onUnsentCountRead(count) {
            dialog.unsentCount = count;
        }
    }
    // Another account shown, or this one gone: not the question asked.
    Connections {
        target: Current
        function onChanged() {
            dialog.close();
        }
    }
}
