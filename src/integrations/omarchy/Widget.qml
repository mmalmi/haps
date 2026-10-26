import QtQuick
import Quickshell
import qs.Ui

BarWidget {
    id: root
    moduleName: "cc.hashtree.haps"

    implicitWidth: button.implicitWidth
    implicitHeight: button.implicitHeight

    function open() {
        const script = decodeURIComponent(Qt.resolvedUrl("../omarchy.py").toString().replace(/^file:\/\//, ""));
        Quickshell.execDetached(["python3", script, "--launch"]);
    }

    WidgetButton {
        id: button
        anchors.fill: parent
        bar: root.bar
        text: root.vertical ? "󰏗" : "Haps"
        tooltipText: "Haps · Find, install, and manage packages"
        onPressed: function(buttonCode) {
            if (buttonCode === Qt.LeftButton) root.open();
        }
    }
}
