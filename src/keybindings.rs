use gtk4::prelude::*;
use gtk4::{gdk, glib, EventControllerKey, PropagationPhase};

use crate::app::App;

/// One binding, as the cheatsheet shows it.
pub struct Binding {
    /// A GTK accelerator string, e.g. `<Super><Alt>Return` - parsed by
    /// `GtkShortcutLabel` into drawn key caps rather than spelled out by hand.
    pub accelerator: &'static str,
    pub description: &'static str,
}

pub struct Section {
    pub title: &'static str,
    /// Shown under the section heading, for the one group whose keys don't
    /// follow the Super+Alt rule.
    pub note: Option<&'static str>,
    pub bindings: &'static [Binding],
}

/// Every binding this app installs, in the order the cheatsheet lists them.
///
/// Deliberately adjacent to `install` below, which is the matcher these
/// describe. The two are still two lists - this one holds accelerator strings
/// for display and that one matches `gdk::Key` values - and keeping them in the
/// same file is what makes a change to one visibly a change to the other. (The
/// real fix is a single keymap both are generated from, which is what makes the
/// bindings user-remappable; that arrives with the config file.)
pub const SECTIONS: &[Section] = &[
    Section {
        title: "Projects",
        note: None,
        bindings: &[
            Binding {
                accelerator: "<Super><Alt>Return",
                description: "Open a new project as a new group",
            },
            Binding {
                accelerator: "<Super><Alt>g",
                description: "Toggle the project sidebar",
            },
            Binding {
                accelerator: "<Super><Alt>bracketleft",
                description: "Switch to the previous project",
            },
            Binding {
                accelerator: "<Super><Alt>bracketright",
                description: "Switch to the next project",
            },
            Binding {
                accelerator: "<Super><Alt>braceleft",
                description: "Move this project up the sidebar",
            },
            Binding {
                accelerator: "<Super><Alt>braceright",
                description: "Move this project down the sidebar",
            },
        ],
    },
    Section {
        title: "Panes",
        note: None,
        bindings: &[
            Binding {
                accelerator: "<Super><Alt><Shift>Return",
                description: "Promote the focused pane to master",
            },
            Binding {
                accelerator: "<Super><Alt>j",
                description: "Focus the next pane",
            },
            Binding {
                accelerator: "<Super><Alt>k",
                description: "Focus the previous pane",
            },
            Binding {
                accelerator: "<Super><Alt>w",
                description: "Close the focused pane",
            },
        ],
    },
    Section {
        title: "Layout",
        note: None,
        bindings: &[
            Binding {
                accelerator: "<Super><Alt>Tab",
                description: "Cycle grid \u{2192} master-stack \u{2192} monocle",
            },
            Binding {
                accelerator: "<Super><Alt>m",
                description: "Toggle monocle (focused pane fullscreen)",
            },
            Binding {
                accelerator: "<Super><Alt>h",
                description: "Shrink the master column",
            },
            Binding {
                accelerator: "<Super><Alt>l",
                description: "Grow the master column",
            },
            Binding {
                accelerator: "<Super><Alt>i",
                description: "More master panes",
            },
            Binding {
                accelerator: "<Super><Alt>d",
                description: "Fewer master panes",
            },
        ],
    },
    Section {
        title: "Text size",
        note: Some("Applies to every pane and to the app's own controls together."),
        bindings: &[
            Binding {
                accelerator: "<Super><Alt>equal",
                description: "Enlarge text",
            },
            Binding {
                accelerator: "<Super><Alt>minus",
                description: "Shrink text",
            },
            Binding {
                accelerator: "<Super><Alt>0",
                description: "Reset text size",
            },
        ],
    },
    Section {
        title: "Clipboard",
        note: Some(
            "The terminal's own keys, so these are the only ones without Super+Alt. \
             Ctrl+C copies only when something is selected \u{2014} with nothing selected \
             it stays the interrupt that stops a running agent.",
        ),
        bindings: &[
            Binding {
                accelerator: "<Control>v",
                description: "Paste (an image, if one is copied)",
            },
            Binding {
                accelerator: "<Shift>Insert",
                description: "Paste the text, never the image",
            },
            Binding {
                accelerator: "<Control>c",
                description: "Copy the selection, or interrupt the agent",
            },
        ],
    },
    Section {
        title: "App",
        note: None,
        bindings: &[
            Binding {
                accelerator: "<Super><Alt>slash",
                description: "Show these keyboard shortcuts",
            },
            Binding {
                accelerator: "<Super><Alt>u",
                description: "Check for updates",
            },
        ],
    },
];

/// dwm-style global bindings, all under Super+Alt so they never collide with
/// whatever the shell/claude/readline inside a pane wants to do with a bare
/// key, and (unlike plain Super+key) don't fight the desktop environment's own
/// global Super+key shortcuts (e.g. KDE's Super+L lock screen). Installed in the
/// Capture phase on the window so they intercept before the focused terminal
/// ever sees the keypress.
pub fn install(window: &impl IsA<gtk4::Widget>, app: &App) {
    let controller = EventControllerKey::new();
    controller.set_propagation_phase(PropagationPhase::Capture);

    let app = app.clone();
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

        // App-level actions: these don't need (and shouldn't require) a pane to
        // be focused in the active project.
        match keyval {
            gdk::Key::Return if !shift => {
                app.new_project();
                return glib::Propagation::Stop;
            }
            gdk::Key::g => {
                app.toggle_sidebar();
                return glib::Propagation::Stop;
            }
            gdk::Key::bracketleft => {
                app.cycle_project(-1);
                return glib::Propagation::Stop;
            }
            gdk::Key::bracketright => {
                app.cycle_project(1);
                return glib::Propagation::Stop;
            }
            // Shift+[ and Shift+] *move* the current project where plain [ and ]
            // switch to another - the same pairing dwm gives its tags. They're
            // matched as `braceleft`/`braceright` rather than as the bracket
            // keys with `shift`, because shifting a bracket doesn't produce a
            // shifted bracket keyval: it produces a brace.
            gdk::Key::braceleft => {
                app.move_active_project(-1);
                return glib::Propagation::Stop;
            }
            gdk::Key::braceright => {
                app.move_active_project(1);
                return glib::Propagation::Stop;
            }
            // Text size is a global setting across every project's panes and the
            // app's own chrome, not just the active project's - so it belongs
            // here too.
            gdk::Key::equal | gdk::Key::plus => {
                app.inc_font_scale();
                return glib::Propagation::Stop;
            }
            gdk::Key::minus => {
                app.dec_font_scale();
                return glib::Propagation::Stop;
            }
            gdk::Key::_0 => {
                app.reset_font_scale();
                return glib::Propagation::Stop;
            }
            gdk::Key::u => {
                app.check_for_updates();
                return glib::Propagation::Stop;
            }
            gdk::Key::slash => {
                app.show_shortcuts();
                return glib::Propagation::Stop;
            }
            _ => {}
        }

        let Some(tiler) = app.active_tiler() else {
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
            _ => return glib::Propagation::Proceed,
        }

        glib::Propagation::Stop
    });

    window.add_controller(controller);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cheatsheet is generated from `SECTIONS`, so a malformed accelerator
    /// there is a key cap that silently fails to draw - `GtkShortcutLabel`
    /// renders nothing at all for a string it can't parse, leaving a row that
    /// describes an action and shows no key for it.
    #[test]
    fn every_advertised_accelerator_actually_parses() {
        // `accelerator_parse` is a GTK function and wants GTK up, so this runs
        // on the one thread that owns it.
        crate::testing::gtk_test(|| {
            for section in SECTIONS {
                assert!(
                    !section.bindings.is_empty(),
                    "section {:?} lists no bindings",
                    section.title,
                );
                for binding in section.bindings {
                    let parsed = gtk4::accelerator_parse(binding.accelerator);
                    let (key, mods) = parsed.unwrap_or_else(|| {
                        panic!(
                            "{:?} ({:?}) is not an accelerator GtkShortcutLabel can draw",
                            binding.accelerator, binding.description,
                        )
                    });
                    assert!(
                        key != gdk::Key::VoidSymbol && !mods.is_empty(),
                        "{:?} parsed to nothing usable",
                        binding.accelerator,
                    );
                }
            }
        });
    }

    /// Every Super+Alt binding the matcher handles has to be advertised, or it
    /// exists only for whoever reads the source. This counts what the matcher
    /// claims against what the cheatsheet shows, which is the cheapest guard
    /// available while the two are still separate lists.
    #[test]
    fn the_cheatsheet_covers_every_super_alt_binding() {
        let advertised = SECTIONS
            .iter()
            .flat_map(|s| s.bindings)
            .filter(|b| b.accelerator.contains("<Super>"))
            .count();
        // Return, g, [, ], {, }, =, -, 0, u, /, Shift+Return, j, k, h, l, i, d,
        // m, Tab, w - the two match blocks in `install`, counted by hand because
        // a match arm isn't something a test can enumerate.
        assert_eq!(
            advertised, 21,
            "the matcher and the cheatsheet have drifted apart",
        );
    }
}
