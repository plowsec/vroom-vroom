// GNOME 42 to 44 (installed as extension.js there). The vroom guest agent
// enables this extension while the host is in Coherence and disables it
// after: the top bar goes away and maximized windows get the whole screen
// (the top bar no longer keeps its strip).
const Main = imports.ui.main;

function track(struts) {
    const lm = Main.layoutManager;
    lm.removeChrome(lm.panelBox);
    lm.addChrome(lm.panelBox, {affectsStruts: struts, trackFullscreen: struts});
}

function init() {
}

function enable() {
    track(false);
    Main.layoutManager.panelBox.hide();
}

function disable() {
    track(true);
    Main.layoutManager.panelBox.show();
}
