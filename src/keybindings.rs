//! Every command the app has, in one table, and the matcher that runs them.
//!
//! This used to be two lists that happened to sit next to each other: a
//! `SECTIONS` table of accelerator strings for the cheatsheet to draw, and a
//! `match` over `gdk::Key` values that actually did the work. Keeping them
//! adjacent was the best that could be done at the time, and the note left here
//! said what the real fix was - "a single keymap both are generated from".
//!
//! The command palette is what made it worth doing. A palette built from a
//! third hand-maintained list would have been a third thing to forget to update,
//! and the failure would have been silent in the way the old pair's was: a
//! binding that works and isn't advertised, or is advertised and doesn't work.
//!
//! So there is one `COMMANDS` table now, and three things read it. The matcher
//! below turns each accelerator into the key it listens for. `shortcuts` draws
//! the cheatsheet from the same rows. `commands` lists them in the palette. A
//! command that isn't in this table doesn't exist in any of the three.
//!
//! The old drift-guard - a test asserting the matcher's arm count by hand -
//! is gone, because the thing it was guarding against can no longer happen.
//! What replaced it is a conflict test, which guards the thing that *can*: two
//! rows quietly claiming the same key.

use gtk4::prelude::*;
use gtk4::{EventControllerKey, PropagationPhase, gdk, glib};

use crate::app::App;
use crate::layout::Mode;
use crate::tiler::Tiler;

/// What a command does when it runs.
///
/// The split is the one the old matcher made with two `match` blocks and a
/// `let ... else` between them: some commands act on the window and work
/// whatever is on screen, and some need a project with panes in it. A `Tiler`
/// command with no active project doesn't consume the keypress - it lets it
/// through to whatever is focused, exactly as before.
#[derive(Clone, Copy)]
pub enum Action {
    App(fn(&App)),
    Tiler(fn(&Tiler)),
}

/// Whether a command cares about Shift.
///
/// Only one pair in the app is told apart by it - `Return` opens a project and
/// `Shift+Return` promotes a pane - and modelling that as "the accelerator says
/// `<Shift>`" alone would break the other half of the problem. Some keyvals
/// *are* the shifted form of another key: `braceleft` only ever arrives with
/// Shift physically held, because it is what shifting `bracketleft` produces.
/// Comparing modifiers strictly would mean those never match, and ignoring them
/// entirely would mean `Return` and `Shift+Return` were the same command.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Shift {
    /// Matches only without Shift. For the plain half of a distinguished pair.
    Off,
    /// Matches only with Shift. For the shifted half.
    On,
    /// Matches either way - the common case, and the right one for keyvals that
    /// already carry the shifting in the value.
    Any,
}

impl Shift {
    fn allows(self, held: bool) -> bool {
        match self {
            Shift::Off => !held,
            Shift::On => held,
            Shift::Any => true,
        }
    }
}

/// One command: what it's called, how it's reached, and what it does.
pub struct Command {
    /// Which cheatsheet group this belongs to. Must name a `SECTIONS` entry.
    pub section: &'static str,
    /// The user-facing wording, used by both the cheatsheet and the palette.
    pub title: &'static str,
    /// A GTK accelerator string, or empty for a command with no key of its own.
    /// Empty means the palette lists it and the cheatsheet doesn't.
    pub accelerator: &'static str,
    pub shift: Shift,
    /// `None` for a binding this table documents but doesn't implement - the
    /// clipboard keys belong to the terminal and are installed by `clipboard`.
    pub run: Option<Action>,
}

impl Command {
    /// Whether this is one of the window-wide Super+Alt bindings the matcher
    /// below owns, as opposed to a terminal key it merely documents or a
    /// palette-only entry with no key at all.
    fn is_global(&self) -> bool {
        self.accelerator.contains("<Super>")
    }
}

/// The cheatsheet's groups, in the order it shows them.
///
/// Separate from `COMMANDS` because a group has things of its own to say - an
/// order, and for one of them a note - and hanging that off whichever command
/// happened to be listed first would make the group's identity an accident of
/// sorting.
pub struct Section {
    pub title: &'static str,
    pub note: Option<&'static str>,
}

pub const SECTIONS: &[Section] = &[
    Section {
        title: "Projects",
        note: None,
    },
    Section {
        title: "Panes",
        note: None,
    },
    Section {
        title: "Layout",
        note: None,
    },
    Section {
        title: "Text size",
        note: Some("Applies to every pane and to the app's own controls together."),
    },
    Section {
        title: "Clipboard",
        note: Some(
            "The terminal's own keys, so these are the only ones without Super+Alt. \
             Ctrl+C copies only when something is selected \u{2014} with nothing selected \
             it stays the interrupt that stops a running agent.",
        ),
    },
    Section {
        title: "App",
        note: None,
    },
];

/// Every command, grouped by section in `SECTIONS` order.
///
/// The global ones all sit under Super+Alt so they never collide with what the
/// shell, claude or readline inside a pane wants to do with a bare key, and
/// (unlike plain Super+key) don't fight the desktop's own global shortcuts -
/// KDE's Super+L, for one.
pub const COMMANDS: &[Command] = &[
    // ── Projects ──────────────────────────────────────────────────────────
    Command {
        section: "Projects",
        title: "Open a new project as a new group",
        accelerator: "<Super><Alt>Return",
        shift: Shift::Off,
        run: Some(Action::App(App::new_project)),
    },
    Command {
        section: "Projects",
        title: "Toggle the project sidebar",
        accelerator: "<Super><Alt>g",
        shift: Shift::Any,
        run: Some(Action::App(App::toggle_sidebar)),
    },
    Command {
        section: "Projects",
        title: "Switch to the previous project",
        accelerator: "<Super><Alt>bracketleft",
        shift: Shift::Any,
        run: Some(Action::App(|app| app.cycle_project(-1))),
    },
    Command {
        section: "Projects",
        title: "Switch to the next project",
        accelerator: "<Super><Alt>bracketright",
        shift: Shift::Any,
        run: Some(Action::App(|app| app.cycle_project(1))),
    },
    // Shift+[ and Shift+] *move* the current project where plain [ and ] switch
    // to another - the same pairing dwm gives its tags. Matched as
    // `braceleft`/`braceright` rather than as the bracket keys with Shift,
    // because shifting a bracket doesn't produce a shifted bracket keyval: it
    // produces a brace. Which is exactly why their `Shift` is `Any`.
    Command {
        section: "Projects",
        title: "Move this project up the sidebar",
        accelerator: "<Super><Alt>braceleft",
        shift: Shift::Any,
        run: Some(Action::App(|app| app.move_active_project(-1))),
    },
    Command {
        section: "Projects",
        title: "Move this project down the sidebar",
        accelerator: "<Super><Alt>braceright",
        shift: Shift::Any,
        run: Some(Action::App(|app| app.move_active_project(1))),
    },
    // ── Panes ─────────────────────────────────────────────────────────────
    Command {
        section: "Panes",
        title: "Promote the focused pane to master",
        accelerator: "<Super><Alt><Shift>Return",
        shift: Shift::On,
        run: Some(Action::Tiler(Tiler::promote_focused_to_master)),
    },
    Command {
        section: "Panes",
        title: "Focus the next pane",
        accelerator: "<Super><Alt>j",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::focus_next)),
    },
    Command {
        section: "Panes",
        title: "Focus the previous pane",
        accelerator: "<Super><Alt>k",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::focus_prev)),
    },
    Command {
        section: "Panes",
        title: "Close the focused pane",
        accelerator: "<Super><Alt>w",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::close_focused)),
    },
    Command {
        section: "Panes",
        title: "Start another agent in this project",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::spawn_pane_here)),
    },
    // ── Layout ────────────────────────────────────────────────────────────
    Command {
        section: "Layout",
        title: "Cycle grid \u{2192} master-stack \u{2192} monocle",
        accelerator: "<Super><Alt>Tab",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::cycle_mode)),
    },
    Command {
        section: "Layout",
        title: "Toggle monocle (focused pane fullscreen)",
        accelerator: "<Super><Alt>m",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::toggle_monocle)),
    },
    Command {
        section: "Layout",
        title: "Shrink the master column",
        accelerator: "<Super><Alt>h",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::dec_master_ratio)),
    },
    Command {
        section: "Layout",
        title: "Grow the master column",
        accelerator: "<Super><Alt>l",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::inc_master_ratio)),
    },
    Command {
        section: "Layout",
        title: "More master panes",
        accelerator: "<Super><Alt>i",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::inc_master_count)),
    },
    Command {
        section: "Layout",
        title: "Fewer master panes",
        accelerator: "<Super><Alt>d",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::dec_master_count)),
    },
    // Reaching a mode directly rather than cycling to it. No keys, because
    // three more bindings to memorise is a worse deal than the one that
    // already cycles - but in a palette, where you read rather than recall,
    // naming the destination is better than naming the journey.
    Command {
        section: "Layout",
        title: "Use the grid layout",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::Tiler(|tiler| tiler.set_mode(Mode::Grid))),
    },
    Command {
        section: "Layout",
        title: "Use the master-stack layout",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::Tiler(|tiler| tiler.set_mode(Mode::MasterStack))),
    },
    Command {
        section: "Layout",
        title: "Use the monocle layout",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::Tiler(|tiler| tiler.set_mode(Mode::Monocle))),
    },
    // ── Text size ─────────────────────────────────────────────────────────
    Command {
        section: "Text size",
        title: "Enlarge text",
        accelerator: "<Super><Alt>equal",
        shift: Shift::Any,
        run: Some(Action::App(App::inc_font_scale)),
    },
    Command {
        section: "Text size",
        title: "Shrink text",
        accelerator: "<Super><Alt>minus",
        shift: Shift::Any,
        run: Some(Action::App(App::dec_font_scale)),
    },
    Command {
        section: "Text size",
        title: "Reset text size",
        accelerator: "<Super><Alt>0",
        shift: Shift::Any,
        run: Some(Action::App(App::reset_font_scale)),
    },
    // ── Clipboard ─────────────────────────────────────────────────────────
    // Documented here, implemented in `clipboard` on the terminal itself. They
    // carry no `run`: there is nothing sensible for a palette entry called
    // "Paste" to paste into, and the matcher below never sees them because they
    // aren't Super+Alt.
    Command {
        section: "Clipboard",
        title: "Paste (an image, if one is copied)",
        accelerator: "<Control>v",
        shift: Shift::Any,
        run: None,
    },
    Command {
        section: "Clipboard",
        title: "Paste the text, never the image",
        accelerator: "<Shift>Insert",
        shift: Shift::Any,
        run: None,
    },
    Command {
        section: "Clipboard",
        title: "Copy the selection, or interrupt the agent",
        accelerator: "<Control>c",
        shift: Shift::Any,
        run: None,
    },
    // ── App ───────────────────────────────────────────────────────────────
    Command {
        section: "App",
        title: "Show all commands",
        accelerator: "<Super><Alt>p",
        shift: Shift::Any,
        run: Some(Action::App(App::show_command_palette)),
    },
    Command {
        section: "App",
        title: "Show these keyboard shortcuts",
        accelerator: "<Super><Alt>slash",
        shift: Shift::Any,
        run: Some(Action::App(App::show_shortcuts)),
    },
    Command {
        section: "App",
        title: "Find in the focused pane",
        accelerator: "<Super><Alt>f",
        shift: Shift::Any,
        run: Some(Action::App(App::toggle_search)),
    },
    Command {
        section: "App",
        title: "Copy the focused pane's output",
        accelerator: "<Super><Alt>c",
        shift: Shift::Any,
        run: Some(Action::App(App::copy_focused_output)),
    },
    Command {
        section: "App",
        title: "Broadcast typing to every agent in this project",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::App(App::toggle_broadcast)),
    },
    Command {
        section: "App",
        title: "Preferences",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::App(App::show_preferences)),
    },
    Command {
        section: "App",
        title: "Check for updates",
        accelerator: "<Super><Alt>u",
        shift: Shift::Any,
        run: Some(Action::App(App::check_for_updates)),
    },
    Command {
        section: "App",
        title: "About AgentTileCLI",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::App(App::show_about)),
    },
];

/// Some keyvals reach the matcher under more than one name, and mean the same
/// command in both.
///
/// `plus` is what many layouts send for Shift+equal, and "enlarge the text" is
/// the same request either way - this is what the old matcher's `equal | plus`
/// arm said, kept because dropping it would silently break the shifted form
/// people actually type.
fn normalize(key: gdk::Key) -> gdk::Key {
    match key {
        gdk::Key::plus => gdk::Key::equal,
        other => other,
    }
}

/// One row of the table, resolved to the key it actually listens for.
struct Bound {
    key: gdk::Key,
    shift: Shift,
    action: Action,
}

/// Resolves the global commands into what the matcher compares against.
///
/// Done once at install rather than per keystroke, and it is also where a
/// malformed accelerator stops being a silent no-op: `accelerator_parse`
/// returning `None` drops the row, which
/// `every_global_command_resolves_to_a_key` catches at `cargo test`.
fn bindings() -> Vec<Bound> {
    COMMANDS
        .iter()
        .filter(|command| command.is_global())
        .filter_map(|command| {
            let action = command.run?;
            let (key, _mods) = gtk4::accelerator_parse(command.accelerator)?;
            Some(Bound {
                key: normalize(key),
                shift: command.shift,
                action,
            })
        })
        .collect()
}

/// Installs the global bindings on `window`.
///
/// Capture phase, so they intercept before the focused terminal ever sees the
/// keypress.
pub fn install(window: &impl IsA<gtk4::Widget>, app: &App) {
    let controller = EventControllerKey::new();
    controller.set_propagation_phase(PropagationPhase::Capture);

    let app = app.clone();
    let bindings = bindings();
    controller.connect_key_pressed(move |_, keyval, _keycode, state| {
        let required = gdk::ModifierType::SUPER_MASK | gdk::ModifierType::ALT_MASK;
        if !state.contains(required) {
            return glib::Propagation::Proceed;
        }

        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        // Letter keys arrive as the uppercase keyval when Shift is held (e.g.
        // `Q`, not `q`), so normalize case and rely on `shift` alone to pick
        // between plain and Shift-modified bindings.
        let key = normalize(keyval.to_lower());

        for binding in &bindings {
            if binding.key != key || !binding.shift.allows(shift) {
                continue;
            }
            return match binding.action {
                Action::App(run) => {
                    run(&app);
                    glib::Propagation::Stop
                }
                // A pane command with no project open isn't an error and isn't
                // consumed - it goes on to whatever is focused, which is what
                // the old matcher's `let ... else` did.
                Action::Tiler(run) => match app.active_tiler() {
                    Some(tiler) => {
                        run(&tiler);
                        glib::Propagation::Stop
                    }
                    None => glib::Propagation::Proceed,
                },
            };
        }

        glib::Propagation::Proceed
    });

    window.add_controller(controller);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cheatsheet draws its keys with `GtkShortcutLabel`, which renders
    /// nothing at all for a string it can't parse - leaving a row that describes
    /// an action and shows no key for it.
    #[test]
    fn every_advertised_accelerator_actually_parses() {
        crate::testing::gtk_test(|| {
            for command in COMMANDS.iter().filter(|c| !c.accelerator.is_empty()) {
                let (key, mods) =
                    gtk4::accelerator_parse(command.accelerator).unwrap_or_else(|| {
                        panic!(
                            "{:?} ({:?}) is not an accelerator GtkShortcutLabel can draw",
                            command.accelerator, command.title,
                        )
                    });
                assert!(
                    key != gdk::Key::VoidSymbol && !mods.is_empty(),
                    "{:?} parsed to nothing usable",
                    command.accelerator,
                );
            }
        });
    }

    /// Every global command has to survive `bindings()`.
    ///
    /// A row dropped there is a key that silently does nothing: the cheatsheet
    /// still advertises it, because it reads the accelerator string, while the
    /// matcher never listens for it.
    #[test]
    fn every_global_command_resolves_to_a_key() {
        crate::testing::gtk_test(|| {
            let expected = COMMANDS
                .iter()
                .filter(|c| c.is_global() && c.run.is_some())
                .count();
            assert_eq!(
                bindings().len(),
                expected,
                "a global command was dropped while resolving its accelerator",
            );
        });
    }

    /// No two commands may claim the same keystroke.
    ///
    /// This replaces a test that counted the matcher's arms by hand and asserted
    /// the total, which could only catch a binding that was never advertised.
    /// One table makes that impossible and makes this possible instead: with the
    /// arms gone, the way to break the matcher is for two rows to answer to the
    /// same key, where the loop silently gives it to whichever is listed first.
    #[test]
    fn no_two_commands_answer_to_the_same_keystroke() {
        crate::testing::gtk_test(|| {
            let bound = bindings();
            for (i, a) in bound.iter().enumerate() {
                for b in &bound[i + 1..] {
                    if a.key != b.key {
                        continue;
                    }
                    // Same key is fine as long as Shift tells them apart, which
                    // is exactly the Return / Shift+Return pair.
                    let collides = matches!(
                        (a.shift, b.shift),
                        (Shift::Any, _)
                            | (_, Shift::Any)
                            | (Shift::Off, Shift::Off)
                            | (Shift::On, Shift::On)
                    );
                    assert!(
                        !collides,
                        "two commands both answer to {:?} (shift rules {:?} and {:?})",
                        a.key, a.shift, b.shift,
                    );
                }
            }
        });
    }

    /// A command naming a section that doesn't exist would be dropped by the
    /// cheatsheet, which walks `SECTIONS` and picks up the commands belonging to
    /// each - so the command would exist, run, and be documented nowhere.
    #[test]
    fn every_command_belongs_to_a_real_section() {
        for command in COMMANDS {
            assert!(
                SECTIONS.iter().any(|s| s.title == command.section),
                "{:?} is in section {:?}, which SECTIONS doesn't list",
                command.title,
                command.section,
            );
        }
    }

    /// Two rows advertising one accelerator is the cheatsheet promising a key
    /// does two things, and the matcher quietly picking whichever is listed
    /// first. The `Shift` rules are what make `Return` legitimately appear
    /// twice, so this compares the pair rather than the string alone.
    #[test]
    fn no_accelerator_is_advertised_twice() {
        let bound: Vec<_> = COMMANDS
            .iter()
            .filter(|c| !c.accelerator.is_empty())
            .collect();
        for (i, a) in bound.iter().enumerate() {
            for b in &bound[i + 1..] {
                assert!(
                    a.accelerator != b.accelerator || a.shift != b.shift,
                    "{:?} is advertised by both {:?} and {:?}",
                    a.accelerator,
                    a.title,
                    b.title,
                );
            }
        }
    }

    /// The plain and shifted halves of the one distinguished pair have to stay
    /// distinguished. Both drifting to `Any` is a change that looks harmless in
    /// a diff and quietly makes Shift+Return open a project.
    ///
    /// Keyed off the accelerator rather than the wording, because the
    /// accelerator is the thing the rule has to agree with: `<Shift>` in the
    /// string and `Shift::On` in the field are two statements of one fact, and
    /// this is what stops them disagreeing.
    #[test]
    fn return_and_shift_return_stay_told_apart() {
        let rule = |accelerator: &str| {
            COMMANDS
                .iter()
                .find(|c| c.accelerator == accelerator)
                .unwrap_or_else(|| panic!("no command bound to {accelerator:?}"))
                .shift
        };
        assert_eq!(rule("<Super><Alt>Return"), Shift::Off);
        assert_eq!(rule("<Super><Alt><Shift>Return"), Shift::On);
    }

    /// Every command whose accelerator names `<Shift>` must say so in its rule,
    /// and no command that doesn't may claim `On`. The generalisation of the
    /// pair above, so a future shifted binding can't be added with the field
    /// left on its `Any` default.
    #[test]
    fn the_shift_rule_agrees_with_the_accelerator() {
        for command in COMMANDS.iter().filter(|c| c.is_global()) {
            if command.accelerator.contains("<Shift>") {
                assert_eq!(
                    command.shift,
                    Shift::On,
                    "{:?} names <Shift> but doesn't require it",
                    command.title,
                );
            } else {
                assert_ne!(
                    command.shift,
                    Shift::On,
                    "{:?} requires Shift but doesn't advertise it",
                    command.title,
                );
            }
        }
    }
}
