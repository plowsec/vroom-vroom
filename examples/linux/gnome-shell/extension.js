// GNOME 45 and later. The vroom guest agent enables this extension while
// the host is in Coherence and disables it after: the top bar goes away
// and maximized windows get the whole screen (the top bar no longer
// keeps its strip). GNOME 42 to 44: extension-legacy.js.
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

function track(struts) {
    const lm = Main.layoutManager;
    lm.removeChrome(lm.panelBox);
    lm.addChrome(lm.panelBox, {affectsStruts: struts, trackFullscreen: struts});
}

export default class VroomCoherence extends Extension {
    enable() {
        track(false);
        Main.layoutManager.panelBox.hide();
    }

    disable() {
        track(true);
        Main.layoutManager.panelBox.show();
    }
}
