use gtk4::prelude::*;
use gtk4::{gdk, glib, EventControllerKey, PropagationPhase};

use crate::tiler::Tiler;

/// dwm-style global bindings, all under Super+Alt so they never collide with
/// whatever the shell/claude/readline inside a pane wants to do with a bare
/// key, and (unlike plain Super+key) don't fight the desktop environment's
/// own global Super+key shortcuts (e.g. KDE's Super+L lock screen). Installed
/// in the Capture phase on the window so they intercept before the focused
/// terminal ever sees the keypress.
pub fn install(window: &impl IsA<gtk4::Widget>, tiler: &Tiler) {
    let controller = EventControllerKey::new();
    controller.set_propagation_phase(PropagationPhase::Capture);

    let tiler = tiler.clone();
    controller.connect_key_pressed(move |_, keyval, _keycode, state| {
        let required = gdk::ModifierType::SUPER_MASK | gdk::ModifierType::ALT_MASK;
        if !state.contains(required) {
            return glib::Propagation::Proceed;
        }

        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        // Letter keys arrive as the uppercase keyval when Shift is held (e.g.
        // `Q`, not `q`), so normalize case and rely on `shift` alone to pick
        // between plain and Shift-modified bindings.
        let keyval = keyval.to_lower();

        match keyval {
            gdk::Key::Return if shift => tiler.promote_focused_to_master(),
            gdk::Key::Return => tiler.spawn_pane(),
            gdk::Key::j => tiler.focus_next(),
            gdk::Key::k => tiler.focus_prev(),
            gdk::Key::h => tiler.dec_master_ratio(),
            gdk::Key::l => tiler.inc_master_ratio(),
            gdk::Key::i => tiler.inc_master_count(),
            gdk::Key::d => tiler.dec_master_count(),
            gdk::Key::m => tiler.toggle_monocle(),
            gdk::Key::Tab => tiler.cycle_mode(),
            gdk::Key::w => tiler.close_focused(),
            gdk::Key::slash => tiler.toggle_help(),
            gdk::Key::equal | gdk::Key::plus => tiler.inc_font_scale(),
            gdk::Key::minus => tiler.dec_font_scale(),
            gdk::Key::_0 => tiler.reset_font_scale(),
            _ => return glib::Propagation::Proceed,
        }

        glib::Propagation::Stop
    });

    window.add_controller(controller);
}
