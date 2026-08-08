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

/// The dot's class for a state. `pub(crate)` because the rack draws the same
/// dots for the same states - see `App::refresh_row_tally`. One function so the
/// two scales cannot disagree about which colour means what.
pub(crate) fn status_class(state: &PaneState) -> &'static str {
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

/// The same fact, short enough to sit in the head strip beside three others.
///
/// Lower case because the strip is set in caps by the stylesheet, and clipped
/// hard because this shares a row with a close button: "working · Read" is the
/// useful form and "working · MultiEditFileWithLongName" is not, so the tool
/// gets the room that's left rather than as much as it wants.
fn status_words(state: &PaneState) -> String {
    match state {
        PaneState::Starting => "starting".to_string(),
        PaneState::Working { tool: Some(tool) } => format!("working \u{b7} {tool}"),
        PaneState::Working { tool: None } => "working".to_string(),
        PaneState::Idle => "waiting for you".to_string(),
        PaneState::Waiting => "asking permission".to_string(),
        PaneState::Exited => "exited".to_string(),
    }
}

/// What the head strip says, and the facts it says it from.
///
/// Shared between the pane and the cwd poll, which is why it is an `Rc` of its
/// own rather than fields on `Pane`: the poll outlives nothing and owns nothing,
/// it just needs to be able to say "the folder changed" and have the strip work
/// out whether that is worth mentioning.
struct Head {
    label: gtk4::Label,
    /// The folder the pane was started in - which is the project's own, and
    /// therefore the one thing the strip should never bother saying. For an
    /// editor pane it is the file's name instead, and mutable because the
    /// file can change under the same strip - see `Pane::refresh_file_name`.
    root: RefCell<String>,
    /// The folder its foreground process is in now, once anything is known.
    cwd: RefCell<Option<String>>,
    state: RefCell<PaneState>,
    /// Whether anything will ever report a state for this pane.
    ///
    /// Only an agent does - the state arrives from claude's hooks over the
    /// socket (see `ipc`). A pane running the update script, or anything else
    /// `Pane::command` starts, has no hooks and so sits in `Starting` for as
    /// long as it lives. Saying "starting" under a command that has been
    /// running for ten minutes is worse than saying nothing, so those panes
    /// keep naming their folder, which is at least true.
    reports: bool,
}

impl Head {
    /// Rewrites the strip from whichever of the two facts is worth reading.
    ///
    /// The strip used to show the folder unconditionally, which meant every
    /// pane in a project displayed that project's name - so a window with the
    /// name in its title bar, in its sidebar row, and on each of four panes
    /// said it six times and distinguished nothing. The folder is only news
    /// when the agent has moved somewhere else, and the rest of the time the
    /// strip has something better to say: what the agent is actually doing.
    fn refresh(&self) {
        let text = match self.cwd.borrow().as_deref() {
            // It has moved out of the project's folder, which is the one case
            // where naming a folder tells you something you didn't know.
            Some(cwd) if cwd != self.root.borrow().as_str() => cwd.to_string(),
            _ if self.reports => status_words(&self.state.borrow()),
            _ => self.root.borrow().clone(),
        };
        self.label.set_label(&text);
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

    // The background is the one colour here that may be translucent, and it is
    // also the one VTE will let go see-through: the terminal clears its own
    // surface, so the `.pane` fill underneath it never shows regardless. The
    // cursor's *foreground* is painted with the opaque form below for the same
    // reason it isn't `alpha`'d - it is the character under the block cursor,
    // which has to be legible against the cursor rather than through it.
    let pane_opacity = crate::appearance::get().pane_opacity;

    // VTE does not honour the alpha it is handed below, and this is the line that
    // works around it. Its GTK4 backend clears the terminal's own surface with
    // the background colour and throws the alpha away, so `set_colors` alone
    // produces a fully opaque terminal at every setting - which is what
    // `pane_opacity` did for its entire life before this.
    //
    // Told not to clear, VTE draws its text and its explicitly-coloured cells and
    // nothing else, and what shows behind them is `.pane`'s CSS fill - which
    // `appearance::content_css` writes at exactly this alpha. The alpha on the
    // colour below is still worth setting: it is what VTE would use if a future
    // version starts honouring it, and it costs nothing if it never does.
    //
    // Only when there is something to see through. At 1.0 the terminal clears its
    // own surface exactly as it always has, which keeps the common case on the
    // path VTE is best at rather than on this one.
    terminal.set_clear_background(pane_opacity >= 1.0);

    let background = theme.background.to_rgba_alpha(pane_opacity as f32);
    let opaque_background = theme.background.to_rgba();
    let foreground = theme.foreground.to_rgba();

    // ANSI 0 takes the surface's alpha, and only ANSI 0. `ansi_palette` defines
    // it as the surface rather than as black precisely so that a program painting
    // a "black" background lands on the pane's own colour instead of a foreign
    // grey - and the moment the pane is glass, opaque is a foreign colour too. A
    // `git log` drawing its own background would otherwise stamp solid rectangles
    // through the translucency, which is the same bug that comment exists to
    // prevent, one layer up.
    //
    // The other fifteen stay opaque. They are deliberate colours a program asked
    // for by name, not the surface, and text you can see the desktop through is
    // not what "red" means.
    let ansi = {
        let mut ansi = theme.ansi.map(|c| c.to_rgba());
        ansi[0] = theme.ansi[0].to_rgba_alpha(pane_opacity as f32);
        ansi
    };
    let ansi_refs: Vec<&gdk::RGBA> = ansi.iter().collect();
    terminal.set_colors(Some(&foreground), Some(&background), &ansi_refs);

    // The colours VTE does *not* take from the palette, and which otherwise
    // arrive from the ambient GTK theme - which is how a carefully built dark
    // palette ends up with a stock-blue selection and a white block cursor in
    // the middle of it.
    terminal.set_color_cursor(Some(&theme.cursor.to_rgba()));
    terminal.set_color_cursor_foreground(Some(&opaque_background));
    terminal.set_color_highlight(Some(&theme.selection.to_rgba()));
    terminal.set_color_highlight_foreground(Some(&foreground));
}

/// Writes the app's own words into a pane, for the two cases where there is no
/// agent to write anything and no state that will ever arrive.
///
/// Fed to the terminal rather than shown as a toast or a dialog, and that is the
/// point: the pane is where the user is already looking, a toast is gone in four
/// seconds, and both of the failures this reports leave a tile sitting in the
/// grid afterwards. A tile with the reason in it can be read whenever it is
/// noticed - which for an agent started in a project the user then walked away
/// from may be some time.
///
/// `\r\n` rather than `\n` because this goes to a terminal, where a bare newline
/// moves down a row without returning to column one, and the second line would
/// start under the end of the first.
///
/// Dim, and prefixed with a blank line, so it reads as the app talking rather
/// than as output from something that ran: SGR 2 is faint, 0 resets.
fn report_in_pane(terminal: &Terminal, message: &str) {
    terminal.feed(format!("\r\n\x1b[2m  {message}\x1b[0m\r\n").as_bytes());
}

/// Sets the terminal's font from the appearance, or leaves VTE on the desktop's
/// own monospace when no font is named.
///
/// Until now nothing set one at all, so every pane inherited whatever
/// `monospace` resolved to on the machine - which meant the one part of the app
/// made entirely of text was the one part nobody had chosen a typeface for. The
/// default names Fira Mono, the face Fira Sans was drawn as a companion to, so
/// the rack and the terminals it indexes speak one family.
///
/// An unparseable description is not an error worth reporting: Pango returns a
/// description with no family set, and VTE falls back to the default - which is
/// exactly what an empty setting asks for anyway.
fn apply_font(terminal: &Terminal) {
    let font = crate::appearance::get().font;
    if font.trim().is_empty() {
        terminal.set_font(None);
        return;
    }
    terminal.set_font(Some(&gtk4::pango::FontDescription::from_string(&font)));
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
    body: Body,
    pub close_button: gtk4::Button,
    /// The dot in the head strip, repainted by `set_state`.
    status: gtk4::Box,
    /// The strip's label and everything it is written from, including this
    /// pane's state - shared with the cwd poll, which also rewrites it.
    head: Rc<Head>,
    pid: Rc<Cell<Option<libc::pid_t>>>,
    /// What `apply_theme` was last called with, so `set_focused` can skip the
    /// repaint when nothing changed. `Tiler::update_focus_style` runs over
    /// every pane after any pane operation, and all but one of those panes
    /// were already in the state it's about to set them to.
    focused: Cell<bool>,
}

/// What fills the frame under the head strip.
///
/// Almost every pane is a terminal, and for a long time the terminal *was* a
/// field, which made "a pane is a tile with a PTY in it" true by construction.
/// The editor is the second thing a tile can hold, and an enum rather than an
/// `Option<Terminal>` because the two are not "a terminal, maybe": every
/// operation the tiler performs either means something to both (focus, close,
/// the head strip) or belongs to exactly one (broadcast and search to the
/// terminal, save to the editor), and a match is where that split is legible.
enum Body {
    Terminal(Terminal),
    Editor(crate::editor::Editor),
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
        // The inset - see `.pane-terminal` in style.css. A class rather than
        // `set_margin_*`, because VTE fills its CSS padding with the terminal's
        // own background while a margin would leave the frame's fill showing.
        terminal.add_css_class("pane-terminal");
        apply_theme(&terminal, false);
        apply_font(&terminal);
        // An agent's bell is this app's "the agent wants you" signal - it's
        // what lights up the group's sidebar row (see `App::flash_row`).
        // Turning the *audible* half off keeps that a visual notification
        // rather than a room-filling one, which matters when several agents
        // are working at once. VTE still emits the `bell` signal either way;
        // this only suppresses the beep.
        terminal.set_audible_bell(false);
        // Agents produce a great deal of output, and VTE's default scrollback is
        // not generous by the standards of a long tool-using turn.
        terminal.set_scrollback_lines(crate::config::get().scrollback as _);
        // VTE has no clipboard keybindings of its own, so without this a pane
        // can't be pasted into at all.
        crate::clipboard::install(&terminal);
        crate::links::install(&terminal);

        let (frame, head, status, close_button) = Self::shell(&terminal);
        (frame, terminal, head, status, close_button)
    }

    /// The tile every body wears: a framed column of head strip over content.
    /// Split out of `bare` when the editor became the second thing a frame
    /// could hold, so both bodies get the same strip, the same dot slot and
    /// the same close button - a tile is a tile, whatever is in it.
    fn shell(content: &impl IsA<gtk4::Widget>) -> (Frame, gtk4::Box, gtk4::Box, gtk4::Button) {
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
            .tooltip_text(status_tooltip(&PaneState::Starting))
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
        body.append(content);

        let frame = Frame::new(None);
        frame.add_css_class("pane");
        frame.set_overflow(gtk4::Overflow::Hidden);
        frame.set_child(Some(&body));

        (frame, head, status, close_button)
    }

    /// A pane holding `path` open in the editor rather than an agent - the
    /// same tile, with a file where the terminal would be. `Err` is the
    /// editor's own refusal (not text, too big, unreadable), worded for a
    /// toast.
    ///
    /// The head strip does the same jobs it does over a terminal, translated:
    /// the label names the file (a folder would name what every neighbouring
    /// strip already says), the dot says whether there is unsaved work in the
    /// vocabulary the dots already speak - amber is "waiting on you", and a
    /// buffer that differs from its file is exactly that - and the editor's
    /// three verbs sit where a terminal pane keeps its close button company.
    pub fn open_file(path: &std::path::Path) -> Result<Self, String> {
        let editor = crate::editor::Editor::load(path)?;
        let (frame, head, status, close_button) = Self::shell(&editor.root);
        // A marker, not a style hook: `TilerLayout::allocate` reads this class
        // to dock the editor at the workspace's left edge rather than tiling
        // it with the agents, and `resize` reads it to measure seams against
        // the area the agents actually divide. The stylesheet deliberately has
        // no rule for it - the editor tile wears the same `.pane` costume as
        // every other tile.
        frame.add_css_class("editor-tile");

        let head_label = gtk4::Label::builder()
            .css_classes(["pane-head-label"])
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .can_target(false)
            .build();
        head.insert_child_after(&head_label, Some(&status));
        head.insert_child_after(&editor.controls, Some(&head_label));

        // `reports: false` and `root` = the file name: the strip shows the one
        // fact this pane has (which file), exactly as a command pane's shows
        // its folder, and nothing will ever arrive over the socket to rewrite
        // it.
        let head_state = Rc::new(Head {
            label: head_label,
            root: RefCell::new(editor.name()),
            cwd: RefCell::new(None),
            state: RefCell::new(PaneState::Idle),
            reports: false,
        });
        head_state.refresh();

        // The dot, driven by the buffer rather than by hooks: quiet grey while
        // the file matches the disk, the amber "waiting on you" while it
        // doesn't - which is what unsaved changes are.
        status.remove_css_class("starting");
        status.add_css_class("idle");
        status.set_tooltip_text(Some("Saved"));
        {
            let status = status.clone();
            editor.buffer.connect_modified_changed(move |buffer| {
                let modified = buffer.is_modified();
                for class in STATUS_CLASSES {
                    status.remove_css_class(class);
                }
                status.add_css_class(if modified { "waiting" } else { "idle" });
                status.set_tooltip_text(Some(if modified {
                    "Unsaved changes (Ctrl+S)"
                } else {
                    "Saved"
                }));
            });
        }

        Ok(Pane {
            id: next_pane_id(),
            frame,
            body: Body::Editor(editor),
            close_button,
            status,
            head: head_state,
            pid: Rc::new(Cell::new(None)),
            focused: Cell::new(false),
        })
    }

    /// The terminal, for the operations that only mean anything to one -
    /// broadcast, copy-output, search, the process signals. An editor pane
    /// answers `None` and those operations pass it by.
    pub fn terminal(&self) -> Option<&Terminal> {
        match &self.body {
            Body::Terminal(terminal) => Some(terminal),
            Body::Editor(_) => None,
        }
    }

    /// The editor, for the operations that only mean anything to one - the
    /// close flow's "anything unsaved?" question.
    pub fn editor(&self) -> Option<&crate::editor::Editor> {
        match &self.body {
            Body::Terminal(_) => None,
            Body::Editor(editor) => Some(editor),
        }
    }

    /// Puts the keyboard where typing lands in this pane.
    pub fn focus_input(&self) {
        match &self.body {
            Body::Terminal(terminal) => {
                terminal.grab_focus();
            }
            Body::Editor(editor) => {
                editor.view.grab_focus();
            }
        }
    }

    /// What the header's subtitle should call this pane, if anything: a
    /// terminal's own window title when it has set one, or the fact of the
    /// file for an editor.
    pub fn title(&self) -> Option<String> {
        match &self.body {
            Body::Terminal(terminal) => terminal.window_title().map(|t| t.to_string()),
            Body::Editor(editor) => Some(format!("editing {}", editor.name())),
        }
    }

    /// Re-reads the strip after the editor switched files - the one fact the
    /// strip shows for an editor pane is which file, and it just changed.
    /// Nothing to do for a terminal pane, whose strip answers to the cwd poll
    /// and the agent's state instead.
    pub fn refresh_file_name(&self) {
        if let Body::Editor(editor) = &self.body {
            *self.head.root.borrow_mut() = editor.name();
            self.head.refresh();
        }
    }

    /// What this pane's head strip is currently calling it.
    ///
    /// Read off the label rather than recomputed, so the drawer's agent row and
    /// the strip on the tile it points at cannot disagree - `Head::refresh` is
    /// the one place that decides between "the folder it has moved to", "what
    /// the agent is doing" and "the folder it started in", and that decision is
    /// subtle enough that a second implementation of it would be a second
    /// answer.
    pub fn head_label(&self) -> String {
        self.head.label.label().to_string()
    }

    /// This pane's agent state, or `None` for a pane no agent will ever speak
    /// for. The rack's dots and the "3 agents" tally read this rather than
    /// `state`, so an open editor is never counted as an agent - it is a file,
    /// not something working on your behalf.
    pub fn agent_state(&self) -> Option<PaneState> {
        match &self.body {
            Body::Terminal(_) => Some(self.state()),
            Body::Editor(_) => None,
        }
    }

    /// VTE's font scale, for the bodies that have VTE in them. The editor's
    /// text stays put for now, the way the sidebar's does: its type is chrome-
    /// sized, and scaling it means deciding how a source view should track the
    /// terminals - a decision worth making once, not implying here.
    pub fn set_font_scale(&self, scale: f64) {
        if let Body::Terminal(terminal) = &self.body {
            terminal.set_font_scale(scale);
        }
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
        let configured = &crate::config::get().command;
        let command = match claude_settings_file() {
            Some(path) => format!(
                "{configured} --settings {}",
                crate::update::sh_quote(&path)
            ),
            None => configured.clone(),
        };
        Self::spawn(cwd, &command, true)
    }

    /// A pane running `command` instead of `claude` (via the same login
    /// shell, so it resolves against the same PATH) - used by the update
    /// button, which runs the pull-and-rebuild script in a pane so its
    /// output is visible rather than hidden behind a spinner.
    pub fn command(cwd: &str, command: &str) -> Self {
        Self::spawn(cwd, command, false)
    }

    /// The shared body of the two above. `reports` says whether an agent's
    /// hooks will ever speak for this pane, which is what its head strip is
    /// allowed to claim - see `Head::reports`.
    fn spawn(cwd: &str, command: &str, reports: bool) -> Self {
        let (frame, terminal, head, status, close_button) = Self::bare();
        let pid = Rc::new(Cell::new(None));
        let id = next_pane_id();

        let head_label = gtk4::Label::builder()
            .css_classes(["pane-head-label"])
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .can_target(false)
            .build();
        // After the dot, before the close button.
        head.insert_child_after(&head_label, Some(&status));

        let head_state = Rc::new(Head {
            label: head_label,
            root: RefCell::new(folder_name(cwd)),
            cwd: RefCell::new(None),
            state: RefCell::new(PaneState::Starting),
            reports,
        });
        head_state.refresh();

        // The dot has the same problem the label had, and needs the same
        // answer. `starting` means "not yet heard from", drawn hollow so that
        // "nothing known" doesn't read as a state of its own - which is right
        // for an agent in the second before its first hook arrives, and wrong
        // forever for a pane that has no hooks to arrive. Those get the plain
        // grey dot that means "a thing that is simply there", which is exactly
        // what a command running in a terminal is.
        if !reports {
            status.remove_css_class("starting");
            status.add_css_class("idle");
            status.set_tooltip_text(Some("Running"));
        }

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

        // A folder that isn't there any more, which a saved session makes ordinary:
        // quit with a project open on a worktree, remove the worktree, reopen. VTE
        // spawns into a missing cwd without complaining - it reports success, the
        // shell dies immediately, and what the user gets is a black tile with a
        // hollow "starting…" dot that stays that way for ever, because the state
        // only ever arrives from an agent's hooks and there is no agent.
        //
        // So this says so, in the one place the user is already looking: the pane.
        // Nothing is spawned, which means no `child-exited`, which means the pane
        // stays put with its explanation on screen rather than vanishing.
        let folder_is_there = std::path::Path::new(cwd).is_dir();
        if !folder_is_there {
            report_in_pane(
                &terminal,
                &format!(
                    "{cwd}\r\n\r\nThis folder no longer exists, so there is nowhere \
                     to start an agent.\r\nClose this pane and open the project \
                     again from wherever it went."
                ),
            );
        }

        let pid_slot = pid.clone();
        let failure_terminal = terminal.downgrade();
        if folder_is_there {
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
                match result {
                    Ok(spawned_pid) => pid_slot.set(Some(spawned_pid.0)),
                    // The other silent pane. A spawn that fails records no pid, so
                    // `hangup` has nothing to signal and `child-exited` never fires
                    // - the pane cannot report, cannot be closed by its agent
                    // ending, and holds its share of the tiling indefinitely with
                    // nothing drawn in it. Whatever VTE refused to do, the reason
                    // belongs on screen.
                    Err(e) => {
                        if let Some(terminal) = failure_terminal.upgrade() {
                            report_in_pane(
                                &terminal,
                                &format!("This pane could not be started.\r\n\r\n{e}"),
                            );
                        }
                    }
                }
            },
        );
        }

        // Poll rather than rely on shell-side OSC7 "report my cwd" hooks
        // (not every shell config sources those) - reading the PTY's
        // foreground process group reflects reality regardless. Stops
        // itself once the label is destroyed (pane closed), since it only
        // holds weak references.
        let head_weak = Rc::downgrade(&head_state);
        let terminal_weak = terminal.downgrade();
        gtk4::glib::source::timeout_add_local(CWD_POLL_INTERVAL, move || {
            let (Some(head), Some(terminal)) = (head_weak.upgrade(), terminal_weak.upgrade())
            else {
                return gtk4::glib::ControlFlow::Break;
            };
            let found = foreground_cwd(&terminal);
            // Only when it actually moved: this runs every second for the life
            // of every pane, and the strip usually has the state in it, which
            // must not be rewritten from under a reader once a second.
            if *head.cwd.borrow() != found {
                *head.cwd.borrow_mut() = found;
                head.refresh();
            }
            gtk4::glib::ControlFlow::Continue
        });

        let pane = Pane {
            id: id.clone(),
            frame,
            body: Body::Terminal(terminal),
            close_button,
            status,
            head: head_state,
            pid,
            focused: Cell::new(false),
        };

        // A pane with no folder to run in has already been told so above, in
        // words. The dot has to agree with them: left alone it reads "starting…"
        // for the life of the window, which is the one thing this pane is
        // definitely not doing, and it is the reading the rack repeats as well.
        if !folder_is_there {
            pane.set_state(PaneState::Exited);
        }

        pane
    }

    /// Repaints the terminal in the focused or unfocused surface, to match the
    /// What this pane's agent is doing.
    pub fn state(&self) -> PaneState {
        // A pane with no agent behind it has no state to report and never will,
        // so its stored `Starting` is not a state - it is the absence of one,
        // and it would otherwise be drawn as the hollow "not yet heard from"
        // dot forever, in the rack as well as on the pane. `Idle` is what the
        // palette already has for "a thing that is simply there", which is
        // exactly what a command running in a terminal is.
        if !self.head.reports {
            return PaneState::Idle;
        }
        self.head.state.borrow().clone()
    }

    /// Moves the dot, and says whether anything actually changed.
    ///
    /// The answer matters to the caller: a turn produces a `PostToolUse` for
    /// every tool an agent runs, and repainting a sidebar tally on each of them
    /// is work nobody asked for. Only a state that moved is news.
    pub fn set_state(&self, state: PaneState) -> bool {
        if *self.head.state.borrow() == state {
            return false;
        }
        for class in STATUS_CLASSES {
            self.status.remove_css_class(class);
        }
        self.status.add_css_class(status_class(&state));
        self.status
            .set_tooltip_text(Some(&status_tooltip(&state)));
        *self.head.state.borrow_mut() = state;
        // The strip carries the state in words whenever the folder isn't news,
        // so a state change is a change to what it reads.
        self.head.refresh();
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
        if self.focused.replace(focused) != focused
            && let Body::Terminal(terminal) = &self.body
        {
            // Editor bodies need nothing here: their focus treatment is the
            // frame's `.focused` ring and fill, which the tiler's CSS class
            // already carries, and there is no VTE clearing to keep in step.
            apply_theme(terminal, focused);
        }
    }

    /// Repaints the terminal from the current appearance - its surface alpha
    /// and its font - leaving its focus state alone.
    ///
    /// Separate from `set_focused` because that one repaints only when focus
    /// actually changed, which is the right guard for a focus change and the
    /// wrong one here: nothing about the pane has changed, the settings have,
    /// and every pane needs the new ones whatever it was doing.
    pub fn refresh_appearance(&self) {
        if let Body::Terminal(terminal) = &self.body {
            apply_theme(terminal, self.focused.get());
            apply_font(terminal);
        }
    }

    /// Politely ask the child (shell + claude) to exit, mirroring how a real
    /// terminal emulator closes a tab. Actual removal from the layout happens
    /// via the `child-exited` signal the caller wires up separately.
    ///
    /// Clears the recorded pid immediately (rather than waiting for
    /// `child-exited`) so the cwd-polling loop stops touching it right away.
    /// Otherwise a pid the OS recycles for an unrelated process in the gap
    /// before `child-exited` fires could get its cwd read and briefly
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

#[cfg(test)]
mod theme_tests {
    use super::*;

    /// What `alpha(tint, a)` laid over `base` comes out as.
    fn tinted(base: palette::Rgb, tint: palette::Rgb, a: f64) -> (i32, i32, i32) {
        let mix = |b: u8, t: u8| (f64::from(b) + a * (f64::from(t) - f64::from(b))).round() as i32;
        (
            mix(base.r, tint.r),
            mix(base.g, tint.g),
            mix(base.b, tint.b),
        )
    }

    /// The head strip's tints have to land on the ramp rungs they replaced.
    ///
    /// `.pane-head` cannot carry a fill: it is a child of `.pane`, so any alpha
    /// of its own composites to *more* opaque than the tile it recesses from, and
    /// a glass pane would get an opaque bar across the top of it. It darkens the
    /// tile instead - and the two numbers that does it with, 0.55 of @shadow and
    /// 0.14 of @text, are derived from @rack and @hairline rather than chosen.
    ///
    /// Which means they are a duplication of the ramp that nothing else would
    /// notice going stale: move @rack, and the strip quietly stops being one rung
    /// below the tile at *every* opacity, including the fully opaque one that
    /// every screenshot of this app is taken at. This is the only thing that says
    /// so.
    #[test]
    fn the_head_strip_still_reads_as_rack() {
        let tile = palette::color("tile");
        let rack = palette::color("rack");
        let hairline = palette::color("hairline");

        let strip = tinted(tile, palette::color("shadow"), 0.55);
        for (got, want, channel) in [
            (strip.0, i32::from(rack.r), "red"),
            (strip.1, i32::from(rack.g), "green"),
            (strip.2, i32::from(rack.b), "blue"),
        ] {
            assert!(
                (got - want).abs() <= 1,
                "the head strip's {channel} is {got}, @rack's is {want}: \
                 `alpha(@shadow, 0.55)` over @tile no longer reads as @rack, so \
                 the strip has stopped sitting one rung below the tile",
            );
        }

        // Four rather than one: @hairline is bluer than any tint of @text can
        // make @tile, so this one is a closest fit rather than an identity. It is
        // still worth pinning - the point is that the rule stays *lighter* than
        // the strip, which is what stops it inverting over a bright wallpaper.
        let rule = tinted(tile, palette::color("text"), 0.14);
        for (got, want, channel) in [
            (rule.0, i32::from(hairline.r), "red"),
            (rule.1, i32::from(hairline.g), "green"),
            (rule.2, i32::from(hairline.b), "blue"),
        ] {
            assert!(
                (got - want).abs() <= 4,
                "the strip's rule is {got} in {channel} where @hairline is {want}: \
                 `alpha(@text, 0.14)` over @tile no longer reads as @hairline",
            );
        }
    }

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
