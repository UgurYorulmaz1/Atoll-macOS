// Global shortcut ⌃⇧Space (Control + Shift + Space): open/close the island.
// Uses the Carbon hot-key API through tauri-plugin-global-shortcut: no Accessibility permission needed,
// and the key combination is only reserved while the setting is on.
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

fn shortcut() -> Shortcut { Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space) }

pub fn apply(on: bool) {
    let app = crate::util::app();
    let gs = app.global_shortcut();
    let sc = shortcut();
    let registered = gs.is_registered(sc);
    if on && !registered {
        let res = gs.on_shortcut(sc, |_app, _sc, ev| {
            if ev.state == ShortcutState::Pressed && crate::settings::get().global_shortcut {
                crate::util::emit("shortcut", serde_json::json!({ "action": "toggle-expand" }));
            }
        });
        if let Err(e) = res { eprintln!("global shortcut: {e}"); }
    } else if !on && registered {
        let _ = gs.unregister(sc);
    }
}
