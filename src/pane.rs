use std::cell::{Cell, RefCell};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{gdk, Frame};
use vte4::{prelude::*, PtyFlags, Terminal};

use crate::model::PaneState;
use crate::palette;

/// How often to re-check a pane's current directory. Cheap (a single
/// syscall pair per pane) so a short interval is fine.
const CWD_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// The shell one-liner claude runs when it finishes a turn (`Stop`) or stops
/// to ask for something (`Notification`) - the two moments a watching human
/// would want to know about, and the two this app repaints a sidebar row for
/// (see `App::flash_row`). All it does is ring the pane's bell, which VTE
/// reports as the `bell` signal and `Tiler` forwards on as "this group wants
/// you".
///
/// It has to find the terminal the hard way, because both obvious routes are
/// closed: claude runs hooks with *no controlling terminal* (`/dev/tty` there
/// is "No such device or address"), and it captures their stdout rather than
/// letting it through to the pane. What is still open is claude's own stdin -
/// the pane's pty - so the hook reads its parent's fd 0 back out of /proc and
/// writes the bell byte straight to that device. Bytes written to a pty slave
/// surface on the master exactly as if the program had printed them, which is
/// precisely the thing the bell signal watches for.
///
/// POSIX sh, not the login shell: claude runs hook commands through /bin/sh.
const BELL_HOOK: &str = r#"PTY=$(readlink /proc/$PPID/fd/0 2>/dev/null); case "$PTY" in /dev/pts/*) printf '\a' > "$PTY" ;; esac"#;

/// The working directory of whichever process currently holds the
/// foreground process group of `terminal`'s PTY - the same technique real
/// terminal emulators use to track "current directory" for tab titles.
///
/// This is deliberately *not* the pid `spawn_async` handed back: that's
/// only the immediate child VTE forked (`$SHELL -lc claude`), and most
/// shells fork claude as a genuine subprocess rather than exec-replacing
/// themselves into it - so that pid's cwd is the shell's launch directory
/// forever, never claude's, and never whatever claude itself is running.
/// Reading the PTY's foreground group instead tracks whatever is actually
/// active in the pane at any moment.
fn foreground_cwd(terminal: &Terminal) -> Option<String> {
    let pty = terminal.pty()?;
    let pgrp = unsafe { libc::tcgetpgrp(pty.fd().as_raw_fd()) };
    if pgrp <= 0 {
        return None;
    }
    let link = std::fs::read_link(format!("/proc/{pgrp}/cwd")).ok()?;
    Some(folder_name(&link.to_string_lossy()))
}

/// Every class `set_state` might put on the dot, so it can take the previous
/// one off without knowing which it was.
const STATUS_CLASSES: [&str; 5] = [
    "starting", "working", "idle", "waiting", "exited",
];

fn status_class(state: &PaneState) -> &'static str {
    match state {
        PaneState::Starting => "starting",
        PaneState::Working { .. } => "working",
        PaneState::Idle => "idle",
        PaneState::Waiting => "waiting",
        PaneState::Exited => "exited",
    }
}

/// What the dot says when you rest on it. The tool name is the whole reason
/// `Working` carries one - "working" is a colour, "running Bash" is an answer.
fn status_tooltip(state: &PaneState) -> String {
    match state {
        PaneState::Starting => "Starting\u{2026}".to_string(),
        PaneState::Working { tool: Some(tool) } => format!("Working \u{b7} {tool}"),
        PaneState::Working { tool: None } => "Working".to_string(),
        PaneState::Idle => "Waiting for you".to_string(),
        PaneState::Waiting => "Asking for permission".to_string(),
        PaneState::Exited => "The agent has exited".to_string(),
    }
}

/// A name for the next pane, unique within this process.
///
/// A counter rather than anything derived from the pty or the pid: the id has
/// to exist *before* the process it identifies does, because it goes into that
/// process's environment.
fn next_pane_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("p{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The last path component of `path` ("/" if the path itself is root), with
/// the kernel's " (deleted)" marker (present when the directory has been
/// removed out from under the process) stripped first so it never leaks
/// into the displayed name.
pub(crate) fn folder_name(path: &str) -> String {
    let path = path.strip_suffix(" (deleted)").unwrap_or(path);
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// A hex literal used by nothing but the terminal. Every colour the chrome also
/// uses comes from `palette` instead, so the two can't drift from each other;
/// these exist in one place already.
fn rgb(hex: &str) -> palette::Rgb {
    palette::Rgb::from_hex(hex).expect("valid hex colour")
}

/// The 16-colour ANSI palette for a pane painted in `surface`. Loosely "One
/// Dark", so `ls --color` and git diffs still read well against the gunmetal.
///
/// Split out from `apply_theme` so it can be checked without a display - it's
/// the only place the terminal-only hexes are written, and `rgb` panics on a
/// malformed one.
fn ansi_palette(surface: palette::Rgb) -> [palette::Rgb; 16] {
    // ANSI 0 and 7 sit on the gunmetal ramp rather than being literal black
    // and white: programs paint "black" backgrounds and "white" text far more
    // often than they mean the actual colours, so anything else leaves
    // rectangles of a foreign grey in the middle of the pane. 0 tracks the
    // surface itself, which is why it's a parameter - it has to keep matching
    // when the pane lightens under focus.
    //
    // Red, green and yellow are the app's own three signals rather than three
    // more literals, because the terminal means the same things by them that
    // the chrome does: red is something breaking, green is something landing,
    // yellow is something asking. A palette that said them in slightly
    // different hues inside the pane than outside it would be two palettes.
    [
        surface,                    // black - the surface itself
        palette::color("hangup"),   // red - the red the chrome destroys in
        palette::color("fresh"),    // green - the green news arrives in
        palette::color("tally"),    // yellow - the amber an agent calls in
        rgb("#74b8ea"),             // blue
        rgb("#bf93d6"),             // magenta
        rgb("#5cc4c0"),             // cyan
        rgb("#d7dde0"),             // white
        palette::color("faint"),    // bright black - the footnote grey
        rgb("#ef8a8a"),             // bright red
        rgb("#a8d795"),             // bright green
        rgb("#ecc07a"),             // bright yellow
        rgb("#96cbf0"),             // bright blue
        rgb("#d3ade4"),             // bright magenta
        rgb("#82d0cf"),             // bright cyan
        rgb("#f4f8f9"),             // bright white
    ]
}

/// Every colour one pane's terminal needs. VTE paints its own background,
/// foreground, cursor and selection rather than taking them from GTK CSS, so
/// none of this can be left to the stylesheet - but every colour shared with
/// the stylesheet is read back out of it (see `palette`) rather than copied,
/// which is what keeps the two in step.
struct Theme {
    foreground: palette::Rgb,
    background: palette::Rgb,
    cursor: palette::Rgb,
    selection: palette::Rgb,
    ansi: [palette::Rgb; 16],
}

/// The theme for a pane that has focus, or one that doesn't.
///
/// Resolving every colour here, away from the terminal it gets painted onto,
/// is what lets `every_colour_the_terminal_needs_resolves` check the lot on a
/// machine with no display: `palette::color` panics on a name the stylesheet
/// no longer defines, and a panic while building a pane is a crash on startup.
fn theme(focused: bool) -> Theme {
    // Matched to `.pane`'s own fill in style.css, so a pane is one continuous
    // surface rather than a terminal of one shade sitting in a frame of
    // another - the seam is visible at any size, and it's the thing that makes
    // a tiling app look assembled rather than designed.
    let background = palette::color(if focused { "tile-lit" } else { "tile" });
    Theme {
        foreground: palette::color("text"),
        background,
        // The same warm light the focused tile is edged in. A cursor is the
        // smallest possible statement of "the keyboard is here", which is the
        // one thing @filament is for.
        cursor: palette::color("filament"),
        selection: palette::selection(background),
        ansi: ansi_palette(background),
    }
}

/// Paints `terminal` in the surface a pane gets when it's `focused` or when it
/// isn't.
///
/// The surface is the whole reason this takes `focused`. `.pane.focused`'s
/// lighter fill is painted over by the terminal - the terminal fills the
/// frame's content box and clears its background opaquely - so the fill only
/// actually reaches the screen if VTE is the one drawing it.
fn apply_theme(terminal: &Terminal, focused: bool) {
    let theme = theme(focused);

    let background = theme.background.to_rgba();
    let foreground = theme.foreground.to_rgba();
    let ansi = theme.ansi.map(|c| c.to_rgba());
    let ansi_refs: Vec<&gdk::RGBA> = ansi.iter().collect();
    terminal.set_colors(Some(&foreground), Some(&background), &ansi_refs);

    // The colours VTE does *not* take from the palette, and which otherwise
    // arrive from the ambient GTK theme - which is how a carefully built dark
    // palette ends up with a stock-blue selection and a white block cursor in
    // the middle of it.
    terminal.set_color_cursor(Some(&theme.cursor.to_rgba()));
    terminal.set_color_cursor_foreground(Some(&background));
    terminal.set_color_highlight(Some(&theme.selection.to_rgba()));
    terminal.set_color_highlight_foreground(Some(&foreground));
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    /// Builds both themes, which resolves every `@define-color` name the
    /// terminal asks for and parses every terminal-only hex literal. Either
    /// one going wrong is a panic here rather than a crash on the first pane.
    ///
    /// No manually-kept list of names to fall out of date: this calls the same
    /// function the app calls, so a lookup added to `theme` or `ansi_palette`
    /// is covered the moment it's written.
    #[test]
    fn every_colour_the_terminal_needs_resolves() {
        for focused in [false, true] {
            let theme = theme(focused);
            assert_eq!(
                theme.ansi.len(),
                16,
                "VTE wants a full 16-colour ANSI palette",
            );
            // Text has to be legible on the surface it's drawn on, and both
            // are greys - so if they ever converge, the pane goes blank.
            assert!(
                theme.foreground.r.abs_diff(theme.background.r) > 100,
                "foreground and background have converged: {:?} on {:?}",
                theme.foreground,
                theme.background,
            );
        }
    }

    /// ANSI 0 is the surface, not literal black: programs paint "black"
    /// backgrounds far more often than they mean the colour, and a mismatch
    /// leaves rectangles of a foreign grey in the middle of the pane. It has
    /// to keep matching when the pane lightens under focus, which is the part
    /// a fixed hex would get wrong.
    #[test]
    fn ansi_black_tracks_the_surface_through_a_focus_change() {
        for focused in [false, true] {
            let theme = theme(focused);
            assert_eq!(
                theme.ansi[0], theme.background,
                "ANSI black left a seam against the surface (focused: {focused})",
            );
        }
    }

    /// The focused pane is painted in a lighter surface than an unfocused one,
    /// and everything mixed over that surface follows it. This is the fill
    /// that `.pane.focused` declares but can't deliver.
    #[test]
    fn focus_lightens_the_surface_and_everything_mixed_over_it() {
        let unfocused = theme(false);
        let focused = theme(true);

        assert!(
            focused.background.r > unfocused.background.r,
            "focus didn't lighten the surface: {:?} vs {:?}",
            unfocused.background,
            focused.background,
        );
        assert_ne!(
            focused.selection, unfocused.selection,
            "the selection tint ignored the surface it's mixed over",
        );
        // The accent-carried colours are the app's constants and shouldn't
        // drift with focus - only the greys under them move.
        assert_eq!(focused.cursor, unfocused.cursor);
        assert_eq!(focused.foreground, unfocused.foreground);
    }
}

/// The `--settings` layer every claude pane is launched with: `BELL_HOOK`,
/// wired to the two events worth interrupting someone for.
///
/// Written out as a file rather than passed inline (`--settings` takes either)
/// because an inline JSON argument would have to survive being quoted through
/// the user's login shell - and that shell can be fish, whose backslash rules
/// inside single quotes differ from POSIX sh's, which is precisely enough to
/// turn the hook's `printf '\a'` into a hook that prints the letter "a". A
/// file has no quoting layers to get wrong.
/// Writes the hook settings under the user's cache directory and returns
/// its path, or `None` if it couldn't be written - in which case panes fall
/// back to a plain, bell-less `claude` rather than failing to start.
///
/// Rewritten on every pane launch instead of only when absent, so a stale hook
/// left behind by an older AgentTileCLI can't outlive the version that wrote
/// it.
fn claude_settings_file() -> Option<String> {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?
        .join("agenttilecli");
    std::fs::create_dir_all(&dir).ok()?;

    let path = dir.join("claude-settings.json");
    let hook_bin = crate::update::exe().ok()?;
    std::fs::write(&path, crate::hooks::settings_json(&hook_bin, BELL_HOOK)).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// A single tile: a bordered frame containing a VTE terminal, running `claude`
/// (or, for the update pane, a build script) via the user's login shell - so
/// PATH/nvm/aliases resolve the same way an interactive terminal would.
pub struct Pane {
    /// What this pane's agent calls itself when it reports in. Unique for the
    /// life of the process, which is the life of the socket it reports over.
    pub id: String,
    pub frame: Frame,
    pub terminal: Terminal,
    pub close_button: gtk4::Button,
    /// The dot in the head strip, repainted by `set_state`.
    status: gtk4::Box,
    /// What this pane's agent is doing, as far as its hooks have said.
    state: RefCell<PaneState>,
    pid: Rc<Cell<Option<libc::pid_t>>>,
    /// What `apply_theme` was last called with, so `set_focused` can skip the
    /// repaint when nothing changed. `Tiler::update_focus_style` runs over
    /// every pane after any pane operation, and all but one of those panes
    /// were already in the state it's about to set them to.
    focused: Cell<bool>,
}

impl Pane {
    /// Builds the shared frame/head/terminal/close-button scaffold every pane
    /// needs, handing back the head strip so the caller can put whatever else
    /// belongs to this pane into it.
    ///
    /// The strip replaces a `GtkOverlay`. The folder label and the close button
    /// used to be laid *over* the terminal - top-left and top-right - which put
    /// two opaque chips on top of the first line the agent wrote and kept them
    /// there. It costs the pane a row of pixels to move them out, and it buys
    /// back the row of text they were sitting on, which is the better trade in
    /// a window whose whole job is showing agent output.
    ///
    /// It is also where a per-pane status dot lands once there is an agent
    /// state to drive it: a strip has somewhere to put one, and a floating chip
    /// does not.
    fn bare() -> (Frame, Terminal, gtk4::Box, gtk4::Box, gtk4::Button) {
        let terminal = Terminal::new();
        terminal.set_hexpand(true);
        terminal.set_vexpand(true);
        apply_theme(&terminal, false);
        // An agent's bell is this app's "the agent wants you" signal - it's
        // what lights up the group's sidebar row (see `App::flash_row`).
        // Turning the *audible* half off keeps that a visual notification
        // rather than a room-filling one, which matters when several agents
        // are working at once. VTE still emits the `bell` signal either way;
        // this only suppresses the beep.
        terminal.set_audible_bell(false);
        // VTE has no clipboard keybindings of its own, so without this a pane
        // can't be pasted into at all.
        crate::clipboard::install(&terminal);

        let close_button = gtk4::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "pane-close"])
            .can_focus(false)
            .tooltip_text("Close this pane")
            .build();

        // The slot the head strip was built for. It is the only thing in this
        // window that says what an agent is *doing* rather than that something
        // happened, and it wants to be first in the strip: the eye reads left
        // to right, and "is this one working" is the question you have before
        // "which folder is it in".
        // Painted with its starting class here rather than left to `set_state`,
        // which repaints only on a *change* - so a dot that never changed would
        // otherwise sit unstyled, and "nothing has reported yet" would look
        // exactly like "idle".
        let status = gtk4::Box::builder()
            .css_classes(["pane-status", status_class(&PaneState::Starting)])
            .valign(gtk4::Align::Center)
            .tooltip_text(&status_tooltip(&PaneState::Starting))
            .build();

        let head = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .css_classes(["pane-head"])
            .build();
        head.append(&status);
        head.append(&close_button);
        // Packed last and aligned right, so whatever the caller prepends flows
        // from the left and the button stays where a close button belongs.
        //
        // It must NOT be the one that expands, though. Anything prepended here
        // is a label, an ellipsizing label's *minimum* width is one ellipsis
        // wide, and a box hands its spare width to whoever asked to expand - so
        // a greedy button here squeezes the folder name down to "AGENTT…LECLI"
        // in a strip with room to spare. The label claims the slack instead.
        close_button.set_halign(gtk4::Align::End);
        close_button.set_hexpand(false);

        let body = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .build();
        body.append(&head);
        body.append(&terminal);

        let frame = Frame::new(None);
        frame.add_css_class("pane");
        frame.set_overflow(gtk4::Overflow::Hidden);
        frame.set_child(Some(&body));

        (frame, terminal, head, status, close_button)
    }

    /// The usual pane: `claude`, running in `cwd` - with `BELL_HOOK` installed,
    /// so a finished or waiting agent lights up its group's sidebar row.
    ///
    /// The hooks arrive via `--settings`, which layers over the user's own
    /// settings files rather than replacing them, and only for panes this app
    /// launches: nothing in ~/.claude is written to, and their claude in any
    /// other terminal is untouched. If the settings file can't be written for
    /// any reason, the pane still gets a perfectly good claude - just a silent
    /// one, which is exactly what it was before this existed.
    pub fn new(cwd: &str) -> Self {
        let command = match claude_settings_file() {
            Some(path) => format!("claude --settings {}", crate::update::sh_quote(&path)),
            None => "claude".to_string(),
        };
        Self::command(cwd, &command)
    }

    /// A pane running `command` instead of `claude` (via the same login
    /// shell, so it resolves against the same PATH) - used by the update
    /// button, which runs the pull-and-rebuild script in a pane so its
    /// output is visible rather than hidden behind a spinner.
    pub fn command(cwd: &str, command: &str) -> Self {
        let (frame, terminal, head, status, close_button) = Self::bare();
        let pid = Rc::new(Cell::new(None));
        let id = next_pane_id();

        let dir_label = gtk4::Label::builder()
            .css_classes(["pane-dir"])
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .can_target(false)
            .label(folder_name(cwd))
            .build();
        // After the dot, before the close button.
        head.insert_child_after(&dir_label, Some(&status));

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let argv = [shell.as_str(), "-lc", command];

        // What the agent's hooks need to find their way back here: which pane
        // is reporting, where to report it, and which binary to run to do so.
        // Absent when the socket couldn't be opened, in which case the hooks
        // find nothing, exit quietly, and the bell carries the signal as it
        // always did.
        //
        // VTE *adds* these to the environment the child would otherwise have
        // inherited rather than replacing it, so a pane still gets the user's
        // PATH, their editor and everything else their shell profile sets up.
        let mut env = Vec::new();
        if let (Some(socket), Ok(bin)) = (crate::ipc::socket(), crate::update::exe()) {
            env.push(format!("{}={id}", crate::ipc::ENV_PANE));
            env.push(format!("{}={socket}", crate::ipc::ENV_SOCKET));
            env.push(format!("{}={bin}", crate::ipc::ENV_BIN));
        }
        let envv: Vec<&str> = env.iter().map(String::as_str).collect();

        let pid_slot = pid.clone();
        terminal.spawn_async(
            PtyFlags::DEFAULT,
            Some(cwd),
            &argv,
            &envv,
            gtk4::glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            None::<&gtk4::gio::Cancellable>,
            move |result| {
                if let Ok(spawned_pid) = result {
                    pid_slot.set(Some(spawned_pid.0));
                }
            },
        );

        // Poll rather than rely on shell-side OSC7 "report my cwd" hooks
        // (not every shell config sources those) - reading the PTY's
        // foreground process group reflects reality regardless. Stops
        // itself once the label is destroyed (pane closed), since it only
        // holds weak references.
        let label_weak = dir_label.downgrade();
        let terminal_weak = terminal.downgrade();
        gtk4::glib::source::timeout_add_local(CWD_POLL_INTERVAL, move || {
            let (Some(label), Some(terminal)) = (label_weak.upgrade(), terminal_weak.upgrade())
            else {
                return gtk4::glib::ControlFlow::Break;
            };
            if let Some(name) = foreground_cwd(&terminal) {
                label.set_label(&name);
            }
            gtk4::glib::ControlFlow::Continue
        });

        Pane {
            id: id.clone(),
            frame,
            terminal,
            close_button,
            status,
            state: RefCell::new(PaneState::Starting),
            pid,
            focused: Cell::new(false),
        }
    }

    /// Repaints the terminal in the focused or unfocused surface, to match the
    /// What this pane's agent is doing.
    pub fn state(&self) -> PaneState {
        self.state.borrow().clone()
    }

    /// Moves the dot, and says whether anything actually changed.
    ///
    /// The answer matters to the caller: a turn produces a `PostToolUse` for
    /// every tool an agent runs, and repainting a sidebar tally on each of them
    /// is work nobody asked for. Only a state that moved is news.
    pub fn set_state(&self, state: PaneState) -> bool {
        if *self.state.borrow() == state {
            return false;
        }
        for class in STATUS_CLASSES {
            self.status.remove_css_class(class);
        }
        self.status.add_css_class(status_class(&state));
        self.status
            .set_tooltip_text(Some(&status_tooltip(&state)));
        *self.state.borrow_mut() = state;
        true
    }

    /// `.focused` CSS class `Tiler::update_focus_style` sets on the frame at
    /// the same moment.
    ///
    /// This is what actually puts the focused pane's lighter fill on screen:
    /// the stylesheet's `.pane.focused` background is covered by the terminal,
    /// which clears its own background across the whole content box. Without
    /// it, focus is carried entirely by the border and the ambient glow - and
    /// both of those need backdrop around the pane to land on, which a pane
    /// pushed flush against a screen edge or a neighbour doesn't have.
    pub fn set_focused(&self, focused: bool) {
        if self.focused.replace(focused) != focused {
            apply_theme(&self.terminal, focused);
        }
    }

    /// Politely ask the child (shell + claude) to exit, mirroring how a real
    /// terminal emulator closes a tab. Actual removal from the layout happens
    /// via the `child-exited` signal the caller wires up separately.
    ///
    /// Clears the recorded pid immediately (rather than waiting for
    /// `child-exited`) so the cwd-polling loop stops touching it right away
    /// - otherwise a pid the OS recycles for an unrelated process in the
    /// gap before `child-exited` fires could get its cwd read and briefly
    /// misattributed to this (closing) pane.
    pub fn hangup(&self) {
        if let Some(pid) = self.pid.take() {
            // The child's whole process group, not just the child. VTE starts
            // it as a session leader, so its pid doubles as the group id, and
            // the processes that actually matter are its descendants: closing
            // the update pane has to stop the `cargo build` underneath the
            // update script, which would otherwise run to completion and
            // replace the installed binary long after the user shut the pane
            // to call the whole thing off.
            //
            // Falls back to signalling the child alone if there turns out to
            // be no such group - better a leaked grandchild than a pane whose
            // shell never gets told to go away.
            unsafe {
                if libc::killpg(pid, libc::SIGHUP) != 0 {
                    libc::kill(pid, libc::SIGHUP);
                }
            }
        }
    }
}
