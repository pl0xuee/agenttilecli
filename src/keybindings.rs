use gtk4::prelude::*;
use gtk4::{gdk, glib, EventControllerKey, PropagationPhase};

use crate::groups::Groups;

/// dwm-style global bindings, all under Super+Alt so they never collide with
/// whatever the shell/claude/readline inside a pane wants to do with a bare
/// key, and (unlike plain Super+key) don't fight the desktop environment's
/// own global Super+key shortcuts (e.g. KDE's Super+L lock screen). Installed
/// in the Capture phase on the window so they intercept before the focused
/// terminal ever sees the keypress.
pub fn install(window: &impl IsA<gtk4::Widget>, groups: &Groups) {
    let controller = EventControllerKey::new();
    controller.set_propagation_phase(PropagationPhase::Capture);

    let groups = groups.clone();
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

        // Group-level actions: these don't need (and shouldn't require) a
        // pane to be focused in the active group.
        match keyval {
            gdk::Key::Return if !shift => {
                groups.new_group();
                return glib::Propagation::Stop;
            }
            gdk::Key::g => {
                groups.toggle_sidebar();
                return glib::Propagation::Stop;
            }
            gdk::Key::bracketleft => {
                groups.cycle_group(-1);
                return glib::Propagation::Stop;
            }
            gdk::Key::bracketright => {
                groups.cycle_group(1);
                return glib::Propagation::Stop;
            }
            // Shift+[ and Shift+] *move* the current group where plain [ and ]
            // switch to another - the same pairing dwm gives its tags. They're
            // matched as `braceleft`/`braceright` rather than as the bracket
            // keys with `shift`, because shifting a bracket doesn't produce a
            // shifted bracket keyval: it produces a brace.
            gdk::Key::braceleft => {
                groups.move_active_group(-1);
                return glib::Propagation::Stop;
            }
            gdk::Key::braceright => {
                groups.move_active_group(1);
                return glib::Propagation::Stop;
            }
            // Text size is a global setting across every group's panes and
            // the app's own chrome (see `Groups::set_font_scale`), not just
            // the active group's - so it belongs here too.
            gdk::Key::equal | gdk::Key::plus => {
                groups.inc_font_scale();
                return glib::Propagation::Stop;
            }
            gdk::Key::minus => {
                groups.dec_font_scale();
                return glib::Propagation::Stop;
            }
            gdk::Key::_0 => {
                groups.reset_font_scale();
                return glib::Propagation::Stop;
            }
            gdk::Key::u => {
                groups.check_for_updates();
                return glib::Propagation::Stop;
            }
            _ => {}
        }

        let Some(tiler) = groups.active_tiler() else {
            return glib::Propagation::Proceed;
        };

        match keyval {
            gdk::Key::Return if shift => tiler.promote_focused_to_master(),
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
            _ => return glib::Propagation::Proceed,
        }

        glib::Propagation::Stop
    });

    window.add_controller(controller);
}
