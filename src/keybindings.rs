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

    /// Whether some keypress would satisfy both rules.
    fn overlaps(self, other: Shift) -> bool {
        [false, true].into_iter().any(|held| self.allows(held) && other.allows(held))
    }
}

/// One command: what it's called, how it's reached, and what it does.
pub struct Command {
    /// The name a config file uses for it - `[keys] find = "Super+Alt+R"`.
    /// Stable, lowercase and hyphenated, and never shown in the window: the
    /// title is what people read, this is what they type.
    pub id: &'static str,
    /// Which cheatsheet group this belongs to. Must name a `SECTIONS` entry.
    pub section: &'static str,
    /// The user-facing wording, used by both the cheatsheet and the palette.
    pub title: &'static str,
    /// The default GTK accelerator string, or empty for a command with no key
    /// of its own. Empty means the palette lists it and the cheatsheet doesn't.
    /// What the window actually listens for is `keys()`, which is this unless
    /// the config says otherwise.
    pub accelerator: &'static str,
    pub shift: Shift,
    /// `None` for a binding this table documents but doesn't implement - the
    /// clipboard keys belong to the terminal and are installed by `clipboard`.
    pub run: Option<Action>,
}

impl Command {
    /// Whether this is one of the window-wide bindings the matcher below owns,
    /// as opposed to a terminal key it merely documents or a palette-only entry
    /// with no key at all - by its default, which is what the tests hold to.
    #[cfg(test)]
    fn is_global(&self) -> bool {
        self.accelerator.contains("<Super>")
    }

    /// The accelerator this command is actually reached by: the config's
    /// `[keys]` entry for it if there is one, and the default otherwise. Empty
    /// when it has no key at all, including when the config took it away.
    ///
    /// Only the window's own bindings can be moved. The clipboard rows document
    /// keys that belong to the terminal, and a config line claiming to rebind
    /// paste would be a line that did nothing.
    pub fn keys(&self) -> String {
        if self.run.is_some()
            && let Some(configured) = crate::config::get().keys.get(self.id)
        {
            return normalize_accelerator(configured).unwrap_or_default();
        }
        self.accelerator.to_string()
    }
}

/// Turns what a person writes in a config file into a GTK accelerator.
///
/// Both spellings are taken: GTK's own (`<Super><Alt>r`), which is what this
/// file uses and what anyone copying from it will write, and the one people
/// actually say (`Super+Alt+R`). `none` and the empty string mean "no key",
/// which is how a binding is given up without being given to anything else.
///
/// `None` is returned only for "no key"; whether the result names a real key is
/// for GTK to say, and `bindings` asks it.
pub fn normalize_accelerator(written: &str) -> Option<String> {
    let written = written.trim();
    if written.is_empty() || written.eq_ignore_ascii_case("none") {
        return None;
    }
    if written.starts_with('<') {
        return Some(written.to_string());
    }
    let mut parts: Vec<&str> = written.split('+').map(str::trim).collect();
    // A trailing `+` is the plus key itself: "Super+Alt++".
    if written.ends_with("++") {
        parts.retain(|part| !part.is_empty());
        parts.push("plus");
    }
    let key = parts.pop().unwrap_or_default();
    let mut accelerator = String::new();
    for modifier in parts {
        let tag = match modifier.to_ascii_lowercase().as_str() {
            "super" | "win" | "meta" | "logo" => "<Super>",
            "alt" | "mod1" => "<Alt>",
            "shift" => "<Shift>",
            "ctrl" | "control" => "<Control>",
            _ => return Some(format!("{written} (unknown modifier {modifier:?})")),
        };
        accelerator.push_str(tag);
    }
    // A single letter is written in capitals by most people and is the same key
    // either way; GTK wants it lower-case, and Shift says itself separately.
    if key.chars().count() == 1 {
        accelerator.push_str(&key.to_lowercase());
    } else {
        accelerator.push_str(key);
    }
    Some(accelerator)
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
        note: Some(
            "Applies to every pane and to the app's own controls together. \
             Ctrl and the mouse wheel does the same.",
        ),
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

/// The agent-specific spawns, as free functions because `Action::Tiler`
/// holds a plain fn pointer and a closure carrying the `Kind` is not one.
///
/// The generic "start another agent" stays, and stays first: it is the one
/// most people want, and it starts whichever agent this project has been
/// using. These are for saying otherwise.
fn spawn_claude(tiler: &Tiler) {
    tiler.spawn_pane_of(crate::agent::Kind::Claude);
}

fn spawn_codex(tiler: &Tiler) {
    tiler.spawn_pane_of(crate::agent::Kind::Codex);
}

fn spawn_grok(tiler: &Tiler) {
    tiler.spawn_pane_of(crate::agent::Kind::Grok);
}

/// Every command, grouped by section in `SECTIONS` order.
///
/// The global ones all sit under Super+Alt so they never collide with what the
/// shell, claude or readline inside a pane wants to do with a bare key - and
/// the particular letters are the ones a stock Omarchy desktop leaves free.
///
/// That second constraint is new, and it moved half the table. Omarchy's
/// Hyprland config binds Super+Alt+Return to tmux, Super+Alt+G to ungrouping a
/// window, Super+Alt+Tab to cycling a group, K, F, `/`, `-`, `=`, `[` and `]`
/// to things of its own - and a compositor binding is taken before the window
/// ever sees the key. So on the desktop this app wears the theme of, ten of its
/// shortcuts did something else entirely: pressing the key to open a project
/// opened a tmux. The keys below were chosen against `hyprctl binds` on a stock
/// install, and `desktop_keys` checks the live desktop at startup for whatever
/// a particular one has added since.
///
/// Where a pair lost one half, Shift now reverses the other rather than a
/// neighbouring letter standing in: Super+Alt+J goes forward, Super+Alt+Shift+J
/// goes back - the Alt+Tab convention, and one letter to remember instead of
/// two. Every key here can be moved in `config.toml`'s `[keys]` table, by `id`.
pub const COMMANDS: &[Command] = &[
    // ── Projects ──────────────────────────────────────────────────────────
    Command {
        id: "open-project",
        section: "Projects",
        title: "Open a new project as a new group",
        accelerator: "<Super><Alt>o",
        shift: Shift::Any,
        run: Some(Action::App(App::new_project)),
    },
    Command {
        id: "toggle-drawer",
        section: "Projects",
        title: "Toggle the project drawer",
        accelerator: "<Super><Alt>b",
        shift: Shift::Any,
        run: Some(Action::App(App::toggle_sidebar)),
    },
    Command {
        id: "previous-project",
        section: "Projects",
        title: "Switch to the previous project",
        accelerator: "<Super><Alt>Page_Up",
        shift: Shift::Off,
        run: Some(Action::App(|app| app.cycle_project(-1))),
    },
    Command {
        id: "next-project",
        section: "Projects",
        title: "Switch to the next project",
        accelerator: "<Super><Alt>Page_Down",
        shift: Shift::Off,
        run: Some(Action::App(|app| app.cycle_project(1))),
    },
    // Shift *moves* the current project where the plain key switches to another
    // - the same pairing dwm gives its tags.
    Command {
        id: "move-project-up",
        section: "Projects",
        title: "Move this project up the rail",
        accelerator: "<Super><Alt><Shift>Page_Up",
        shift: Shift::On,
        run: Some(Action::App(|app| app.move_active_project(-1))),
    },
    Command {
        id: "move-project-down",
        section: "Projects",
        title: "Move this project down the rail",
        accelerator: "<Super><Alt><Shift>Page_Down",
        shift: Shift::On,
        run: Some(Action::App(|app| app.move_active_project(1))),
    },
    // ── Panes ─────────────────────────────────────────────────────────────
    Command {
        id: "go-to-waiting",
        section: "Panes",
        title: "Go to the agent that wants you",
        accelerator: "<Super><Alt>n",
        shift: Shift::Any,
        run: Some(Action::App(App::go_to_agent_that_wants_you)),
    },
    Command {
        id: "new-agent",
        section: "Panes",
        title: "Start another agent in this project",
        accelerator: "<Super><Alt>a",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::spawn_pane_here)),
    },
    Command {
        id: "promote",
        section: "Panes",
        title: "Promote the focused pane to master",
        accelerator: "<Super><Alt><Shift>Return",
        shift: Shift::On,
        run: Some(Action::Tiler(Tiler::promote_focused_to_master)),
    },
    Command {
        id: "focus-next",
        section: "Panes",
        title: "Focus the next pane",
        accelerator: "<Super><Alt>j",
        shift: Shift::Off,
        run: Some(Action::Tiler(Tiler::focus_next)),
    },
    Command {
        id: "focus-previous",
        section: "Panes",
        title: "Focus the previous pane",
        accelerator: "<Super><Alt><Shift>j",
        shift: Shift::On,
        run: Some(Action::Tiler(Tiler::focus_prev)),
    },
    Command {
        id: "close-pane",
        section: "Panes",
        title: "Close the focused pane",
        accelerator: "<Super><Alt>w",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::close_focused)),
    },
    Command {
        id: "new-worktree-agent",
        section: "Panes",
        title: "Start an agent in a new git worktree",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::App(App::spawn_in_worktree)),
    },
    Command {
        id: "new-claude",
        section: "Panes",
        title: "Start a claude agent in this project",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::Tiler(spawn_claude)),
    },
    Command {
        id: "new-codex",
        section: "Panes",
        title: "Start a codex agent in this project",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::Tiler(spawn_codex)),
    },
    Command {
        id: "new-grok",
        section: "Panes",
        title: "Start a grok agent in this project",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::Tiler(spawn_grok)),
    },
    Command {
        id: "resume-agents",
        section: "Panes",
        title: "Resume the agents this project had last time",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::App(App::resume_agents)),
    },
    // ── Layout ────────────────────────────────────────────────────────────
    Command {
        id: "cycle-layout",
        section: "Layout",
        title: "Cycle grid \u{2192} master-stack \u{2192} monocle",
        accelerator: "<Super><Alt>t",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::cycle_mode)),
    },
    Command {
        id: "monocle",
        section: "Layout",
        title: "Toggle monocle (focused pane fullscreen)",
        accelerator: "<Super><Alt>m",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::toggle_monocle)),
    },
    Command {
        id: "shrink-master",
        section: "Layout",
        title: "Shrink the master column",
        accelerator: "<Super><Alt>h",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::dec_master_ratio)),
    },
    Command {
        id: "grow-master",
        section: "Layout",
        title: "Grow the master column",
        accelerator: "<Super><Alt>l",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::inc_master_ratio)),
    },
    Command {
        id: "more-masters",
        section: "Layout",
        title: "More master panes",
        accelerator: "<Super><Alt>i",
        shift: Shift::Any,
        run: Some(Action::Tiler(Tiler::inc_master_count)),
    },
    Command {
        id: "fewer-masters",
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
        id: "grid",
        section: "Layout",
        title: "Use the grid layout",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::Tiler(|tiler| tiler.set_mode(Mode::Grid))),
    },
    Command {
        id: "master-stack",
        section: "Layout",
        title: "Use the master-stack layout",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::Tiler(|tiler| tiler.set_mode(Mode::MasterStack))),
    },
    Command {
        id: "monocle-layout",
        section: "Layout",
        title: "Use the monocle layout",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::Tiler(|tiler| tiler.set_mode(Mode::Monocle))),
    },
    // ── Text size ─────────────────────────────────────────────────────────
    // Z for zoom, and Shift to reverse it, because the keys every other app
    // uses here - `=` and `-` - are Hyprland's, shifted and unshifted alike.
    // Ctrl and the mouse wheel over any pane does the same thing, which is
    // what most people reach for anyway.
    Command {
        id: "enlarge-text",
        section: "Text size",
        title: "Enlarge text",
        accelerator: "<Super><Alt>z",
        shift: Shift::Off,
        run: Some(Action::App(App::inc_font_scale)),
    },
    Command {
        id: "shrink-text",
        section: "Text size",
        title: "Shrink text",
        accelerator: "<Super><Alt><Shift>z",
        shift: Shift::On,
        run: Some(Action::App(App::dec_font_scale)),
    },
    Command {
        id: "reset-text",
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
    // aren't the window's.
    Command {
        id: "paste",
        section: "Clipboard",
        title: "Paste (an image, if one is copied)",
        accelerator: "<Control>v",
        shift: Shift::Any,
        run: None,
    },
    Command {
        id: "paste-text",
        section: "Clipboard",
        title: "Paste the text, never the image",
        accelerator: "<Shift>Insert",
        shift: Shift::Any,
        run: None,
    },
    Command {
        id: "copy",
        section: "Clipboard",
        title: "Copy the selection, or interrupt the agent",
        accelerator: "<Control>c",
        shift: Shift::Any,
        run: None,
    },
    // ── App ───────────────────────────────────────────────────────────────
    Command {
        id: "commands",
        section: "App",
        title: "Show all commands",
        accelerator: "<Super><Alt>p",
        shift: Shift::Any,
        run: Some(Action::App(App::show_command_palette)),
    },
    // `?` rather than `/`, which Omarchy spends on monitor scaling. `?` is the
    // help key in half the web apps anyone uses, which makes it the better
    // answer anyway.
    Command {
        id: "shortcuts",
        section: "App",
        title: "Show these keyboard shortcuts",
        accelerator: "<Super><Alt>question",
        shift: Shift::Any,
        run: Some(Action::App(App::show_shortcuts)),
    },
    // R, as in the shell's Ctrl+R - F is Omarchy's "full width".
    Command {
        id: "find",
        section: "App",
        title: "Find in the focused pane",
        accelerator: "<Super><Alt>r",
        shift: Shift::Any,
        run: Some(Action::App(App::toggle_search)),
    },
    Command {
        id: "copy-output",
        section: "App",
        title: "Copy the focused pane's output",
        accelerator: "<Super><Alt>c",
        shift: Shift::Any,
        run: Some(Action::App(App::copy_focused_output)),
    },
    Command {
        id: "broadcast",
        section: "App",
        title: "Broadcast typing to every agent in this project",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::App(App::toggle_broadcast)),
    },
    Command {
        id: "preferences",
        section: "App",
        title: "Preferences",
        accelerator: "",
        shift: Shift::Any,
        run: Some(Action::App(App::show_preferences)),
    },
    Command {
        id: "updates",
        section: "App",
        title: "Check for updates",
        accelerator: "<Super><Alt>u",
        shift: Shift::Any,
        run: Some(Action::App(App::check_for_updates)),
    },
    Command {
        id: "about",
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

/// The modifiers a binding is told apart by. Shift is handled separately (see
/// `Shift`), and anything else a keymap reports - Num Lock, a Meta that some
/// keymaps set alongside Super - is not something a person pressed on purpose.
fn chord(state: gdk::ModifierType) -> gdk::ModifierType {
    state
        & (gdk::ModifierType::SUPER_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::CONTROL_MASK)
}

/// One row of the table, resolved to the key it actually listens for.
pub(crate) struct Bound {
    pub(crate) id: &'static str,
    pub(crate) key: gdk::Key,
    /// Super, Alt and Ctrl, as the accelerator names them - see `chord`.
    pub(crate) mods: gdk::ModifierType,
    pub(crate) shift: Shift,
    action: Action,
}

impl Bound {
    /// Whether this binding's *advertised* chord has Shift in it.
    ///
    /// A binding that doesn't care about Shift (`Shift::Any`) is advertised,
    /// and pressed, without it; that it also answers with Shift held is a
    /// courtesy to keys like `?` that come shifted anyway. So when something
    /// else takes the shifted chord, nothing anyone was told to press stops
    /// working - which is the test for whether a key has been taken.
    pub(crate) fn advertised_with_shift(&self) -> bool {
        self.shift == Shift::On
    }
}

/// Resolves the window's commands into what the matcher compares against, and
/// says what in the config's `[keys]` table could not be honoured.
///
/// Done once at install rather than per keystroke, and it is where a malformed
/// accelerator stops being a silent no-op: a default that fails to parse is
/// caught at `cargo test` (`every_global_command_resolves_to_a_key`), and one
/// somebody typed is reported back to them, because a key that silently does
/// nothing is how a config file earns its reputation for doing nothing.
pub(crate) fn resolve() -> (Vec<Bound>, Vec<String>) {
    let mut bound = Vec::new();
    let mut problems = Vec::new();
    let configured = &crate::config::get().keys;

    for id in configured.keys() {
        if !COMMANDS.iter().any(|c| c.id == id && c.run.is_some()) {
            problems.push(format!(
                "`{id}` isn't a command with a key to set. See the ids in the \
                 README's Keybindings table."
            ));
        }
    }

    for command in COMMANDS {
        let Some(action) = command.run else { continue };
        let keys = command.keys();
        if keys.is_empty() {
            continue;
        }
        let overridden = configured.contains_key(command.id);
        let Some((key, mods)) = gtk4::accelerator_parse(&keys) else {
            problems.push(format!(
                "`{} = \"{}\"` isn't a key combination GTK recognises.",
                command.id,
                configured.get(command.id).map_or(keys.as_str(), String::as_str),
            ));
            continue;
        };
        if chord(mods).is_empty() {
            // A bare key here would be taken from every terminal in the window:
            // `new-agent = "a"` and nobody could type the letter a again.
            problems.push(format!(
                "`{} = \"{}\"` needs Super, Alt or Ctrl held - a bare key would \
                 be taken away from every terminal in the window.",
                command.id, keys,
            ));
            continue;
        }
        // Shift with anything that isn't a letter never arrives as written: GTK
        // reports the character Shift *makes*, so `Shift+2` is `@` on the wire
        // and a binding waiting for `2` waits for ever. Said, rather than left
        // to find out by pressing it.
        if overridden
            && mods.contains(gdk::ModifierType::SHIFT_MASK)
            && key.to_lower() == key.to_upper()
            && key.to_unicode().is_some_and(|c| !c.is_alphanumeric() || c.is_ascii_digit())
        {
            problems.push(format!(
                "`{} = \"{}\"`: with Shift, write the character Shift makes \
                 (`Super+Alt+at` rather than `Super+Alt+Shift+2`), since that is \
                 the key GTK reports.",
                command.id,
                configured.get(command.id).map_or(keys.as_str(), String::as_str),
            ));
        }
        let shift = if !overridden {
            command.shift
        } else if mods.contains(gdk::ModifierType::SHIFT_MASK) {
            Shift::On
        } else {
            Shift::Any
        };
        bound.push(Bound {
            id: command.id,
            key: normalize(key.to_lower()),
            mods: chord(mods),
            shift,
            action,
        });
    }

    // A key that is bound both with and without Shift has to be told apart by
    // it: the unshifted row gives up the shifted chord to its partner. Only
    // needed for rows the config moved - the table's own rows say so already.
    let shifted: Vec<(gdk::Key, gdk::ModifierType)> = bound
        .iter()
        .filter(|b| b.shift == Shift::On)
        .map(|b| (b.key, b.mods))
        .collect();
    for binding in &mut bound {
        if binding.shift == Shift::Any && shifted.contains(&(binding.key, binding.mods)) {
            binding.shift = Shift::Off;
        }
    }

    // And two rows answering to one chord is the matcher silently giving it to
    // whichever is listed first.
    for (i, a) in bound.iter().enumerate() {
        for b in &bound[i + 1..] {
            if a.key == b.key && a.mods == b.mods && a.shift.overlaps(b.shift) {
                problems.push(format!(
                    "`{}` and `{}` are both on the same keys; `{}` wins.",
                    a.id, b.id, a.id,
                ));
            }
        }
    }

    (bound, problems)
}

/// Installs the window's bindings on `window`, and returns anything about the
/// config's `[keys]` table that could not be honoured.
///
/// Capture phase, so they intercept before the focused terminal ever sees the
/// keypress.
pub fn install(window: &impl IsA<gtk4::Widget>, app: &App) -> Vec<String> {
    let controller = EventControllerKey::new();
    controller.set_propagation_phase(PropagationPhase::Capture);

    install_wheel_zoom(window, app);
    let app = app.clone();
    let (bindings, problems) = resolve();
    controller.connect_key_pressed(move |_, keyval, _keycode, state| {
        let held = chord(state);
        if held.is_empty() {
            return glib::Propagation::Proceed;
        }

        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        // Letter keys arrive as the uppercase keyval when Shift is held (e.g.
        // `Q`, not `q`), so normalize case and rely on `shift` alone to pick
        // between plain and Shift-modified bindings.
        let key = normalize(keyval.to_lower());

        for binding in &bindings {
            if binding.key != key || binding.mods != held || !binding.shift.allows(shift) {
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
    problems
}

/// Ctrl and the mouse wheel, over anything in the window, sizes the text - the
/// gesture every terminal and browser has taught people, and the one they reach
/// for before any key.
///
/// Capture phase and consumed, for the same reason the keys are: a terminal
/// underneath would otherwise scroll its scrollback with the same turn of the
/// wheel. Discrete, so a touchpad's stream of tiny deltas arrives as the same
/// steps a notched wheel makes rather than as a zoom that races away.
fn install_wheel_zoom(window: &impl IsA<gtk4::Widget>, app: &App) {
    let scroll = gtk4::EventControllerScroll::new(
        gtk4::EventControllerScrollFlags::VERTICAL | gtk4::EventControllerScrollFlags::DISCRETE,
    );
    scroll.set_propagation_phase(PropagationPhase::Capture);
    let app = app.clone();
    scroll.connect_scroll(move |controller, _, dy| {
        let held = controller.current_event_state();
        if chord(held) != gdk::ModifierType::CONTROL_MASK || dy == 0.0 {
            return glib::Propagation::Proceed;
        }
        if dy < 0.0 {
            app.inc_font_scale();
        } else {
            app.dec_font_scale();
        }
        glib::Propagation::Stop
    });
    window.add_controller(scroll);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The palette is the only route to an agent this project's `+` isn't set
    /// to, for anyone who works from the keyboard. A `Kind` added to `agent`
    /// without an entry here would be an agent you could configure and never
    /// reach.
    #[test]
    fn every_agent_can_be_started_from_the_palette() {
        for kind in crate::agent::Kind::ALL {
            assert!(
                COMMANDS
                    .iter()
                    .any(|c| c.title.contains(kind.label()) && c.run.is_some()),
                "no palette entry starts a {} agent",
                kind.label(),
            );
        }
    }

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

    /// Every global command has to survive `resolve()`.
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
            let (bound, problems) = resolve();
            assert!(problems.is_empty(), "the defaults have problems: {problems:?}");
            assert_eq!(
                bound.len(),
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
            let (bound, _) = resolve();
            for (i, a) in bound.iter().enumerate() {
                for b in &bound[i + 1..] {
                    if a.key != b.key || a.mods != b.mods {
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

    /// The pairs Shift tells apart have to stay told apart. Both halves
    /// drifting to `Any` is a change that looks harmless in a diff and quietly
    /// makes Shift+J focus the *next* pane.
    ///
    /// Keyed off the accelerator rather than the wording, because the
    /// accelerator is the thing the rule has to agree with: `<Shift>` in the
    /// string and `Shift::On` in the field are two statements of one fact, and
    /// this is what stops them disagreeing.
    #[test]
    fn the_shift_reversed_pairs_stay_told_apart() {
        let rule = |accelerator: &str| {
            COMMANDS
                .iter()
                .find(|c| c.accelerator == accelerator)
                .unwrap_or_else(|| panic!("no command bound to {accelerator:?}"))
                .shift
        };
        for (plain, shifted) in [
            ("<Super><Alt>j", "<Super><Alt><Shift>j"),
            ("<Super><Alt>z", "<Super><Alt><Shift>z"),
            ("<Super><Alt>Page_Up", "<Super><Alt><Shift>Page_Up"),
            ("<Super><Alt>Page_Down", "<Super><Alt><Shift>Page_Down"),
        ] {
            assert_eq!(rule(plain), Shift::Off, "{plain}");
            assert_eq!(rule(shifted), Shift::On, "{shifted}");
        }
    }

    /// The keys a stock Omarchy desktop's Hyprland config takes for itself, as
    /// `hyprctl binds` listed them on 2026-09-28 - `(shift held, key)` under
    /// Super+Alt. A compositor binding is taken before the window sees the key,
    /// so any of these as a default is a shortcut that does something else on
    /// the desktop this app wears the theme of. Ten of them were, until this.
    const OMARCHY_SUPER_ALT: &[(bool, &str)] = &[
        (false, "Return"), (false, "g"), (false, "Tab"), (false, "k"), (false, "f"),
        (false, "s"), (false, "slash"), (false, "space"), (false, "comma"),
        (false, "Home"), (false, "minus"), (false, "equal"), (false, "bracketleft"),
        (false, "bracketright"), (false, "Left"), (false, "Right"), (false, "Up"),
        (false, "Down"), (false, "1"), (false, "2"), (false, "3"), (false, "4"),
        (false, "5"),
        (true, "Tab"), (true, "a"), (true, "b"), (true, "e"), (true, "f"), (true, "g"),
        (true, "m"), (true, "x"), (true, "comma"), (true, "minus"), (true, "equal"),
        (true, "Left"), (true, "Right"), (true, "Up"), (true, "Down"), (true, "1"),
        (true, "2"), (true, "3"), (true, "4"), (true, "5"), (true, "6"), (true, "7"),
        (true, "8"), (true, "9"), (true, "0"),
    ];

    #[test]
    fn no_default_is_a_key_a_stock_omarchy_desktop_keeps_for_itself() {
        crate::testing::gtk_test(|| {
            let super_alt = gdk::ModifierType::SUPER_MASK | gdk::ModifierType::ALT_MASK;
            let (bound, _) = resolve();
            for binding in bound.iter().filter(|b| b.mods == super_alt) {
                for (shifted, name) in OMARCHY_SUPER_ALT {
                    let taken = gdk::Key::from_name(*name).expect("a key name").to_lower();
                    assert!(
                        !(binding.key == taken && binding.advertised_with_shift() == *shifted),
                        "`{}` defaults to Super+Alt+{}{name}, which Omarchy's \
                         Hyprland takes before the window sees it",
                        binding.id,
                        if *shifted { "Shift+" } else { "" },
                    );
                }
            }
        });
    }

    /// Ids are what a config file names, so they have to be unique, and they
    /// have to be something a person can type without quoting.
    #[test]
    fn every_command_has_its_own_id() {
        for (i, a) in COMMANDS.iter().enumerate() {
            assert!(
                a.id.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{:?} is not a plain lowercase id",
                a.id,
            );
            for b in &COMMANDS[i + 1..] {
                assert_ne!(a.id, b.id, "two commands share an id");
            }
        }
    }

    /// A config line is written however a person writes a key - and GTK's own
    /// spelling, copied out of this file, has to work too.
    #[test]
    fn a_key_is_read_the_way_people_write_one() {
        let n = |written: &str| normalize_accelerator(written);
        assert_eq!(n("Super+Alt+R").as_deref(), Some("<Super><Alt>r"));
        assert_eq!(n(" super + alt + shift + k ").as_deref(), Some("<Super><Alt><Shift>k"));
        assert_eq!(n("Ctrl+Alt+Page_Up").as_deref(), Some("<Control><Alt>Page_Up"));
        assert_eq!(n("<Super><Alt>k").as_deref(), Some("<Super><Alt>k"));
        assert_eq!(n("Super+Alt++").as_deref(), Some("<Super><Alt>plus"));
        assert_eq!(n("none"), None, "a key can be given up");
        assert_eq!(n(""), None);
        assert!(
            n("Hyper+K").is_some_and(|a| a.contains("unknown modifier")),
            "a modifier nobody has is kept, so GTK can refuse it out loud",
        );
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
