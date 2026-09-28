use std::cell::{Cell, RefCell};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gdk, Frame};
use vte4::{prelude::*, PtyFlags, Terminal};

use crate::agent::{Kind, Launch};
use crate::model::PaneState;
use crate::palette;

/// How often to re-check a pane's current directory, in whole seconds. Cheap
/// (a single syscall pair per pane) so a short interval is fine.
const CWD_POLL_SECONDS: u32 = 1;

/// The shell one-liner an agent runs when it finishes a turn (`Stop`) or stops
/// to ask for something (`Notification`) - the two moments a watching human
/// would want to know about, and the two this app repaints a sidebar row for
/// (see `App::flash_row`). All it does is ring the pane's bell, which VTE
/// reports as the `bell` signal and `Tiler` forwards on as "this group wants
/// you".
///
/// It has to find the terminal the hard way, because both obvious routes are
/// closed: agents run hooks with *no controlling terminal* (`/dev/tty` there
/// is "No such device or address"), and it captures their stdout rather than
/// letting it through to the pane. What is still open is the agent's own stdin -
/// the pane's pty - so the hook reads its parent's fd 0 back out of /proc and
/// writes the bell byte straight to that device. Bytes written to a pty slave
/// surface on the master exactly as if the program had printed them, which is
/// precisely the thing the bell signal watches for.
///
/// POSIX sh, not the login shell: agents run hook commands through /bin/sh.
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
const STATUS_CLASSES: [&str; 6] = [
    "starting", "working", "idle", "waiting", "exited", "unsaved",
];

/// The dot's class for a state. `pub(crate)` because the rack draws the same
/// dots for the same states - see `App::refresh_row_tally`. One function so the
/// two scales cannot disagree about which colour means what.
pub(crate) fn status_class(state: &PaneState) -> &'static str {
    match state {
        PaneState::Starting => "starting",
        PaneState::Working { .. } => "working",
        PaneState::Idle => "idle",
        PaneState::Waiting { .. } => "waiting",
        PaneState::Exited => "exited",
    }
}

/// The class the whole tile wears for a state, so the stylesheet can light the
/// *tile* - an amber edge on one that is asking, a sweep across one that is
/// working - rather than only the dot in its corner. A dot is a thing you read;
/// a lit edge is a thing you see from across the room.
fn state_frame_class(state: &PaneState) -> String {
    format!("state-{}", status_class(state))
}

/// Puts `class`'s tile treatment on `frame`, taking any other state's off.
fn set_frame_state(frame: &Frame, class: &str) {
    for other in STATUS_CLASSES {
        frame.remove_css_class(&format!("state-{other}"));
    }
    frame.add_css_class(&format!("state-{class}"));
}

/// What the dot says when you rest on it. The tool name is the whole reason
/// `Working` carries one - "working" is a colour, "running Bash" is an answer.
fn status_tooltip(state: &PaneState) -> String {
    match state {
        PaneState::Starting => "Starting\u{2026}".to_string(),
        PaneState::Working { tool: Some(tool) } => format!("Working \u{b7} {tool}"),
        PaneState::Working { tool: None } => "Working".to_string(),
        PaneState::Idle => "Waiting for you".to_string(),
        PaneState::Waiting { tool: Some(tool) } => format!("Asking permission to use {tool}"),
        PaneState::Waiting { tool: None } => "Asking for permission".to_string(),
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
        PaneState::Waiting { tool: Some(tool) } => format!("asking permission \u{b7} {tool}"),
        PaneState::Waiting { tool: None } => "asking permission".to_string(),
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

/// The same fact again, cut to fit a drawer row beside the agent's name.
///
/// The drawer is a column a few hundred pixels wide, and "asking permission ·
/// Bash" beside a name ellipsized to "asking permiss…" - the one word that
/// mattered, gone. So the drawer has its own, shorter vocabulary: the verb and
/// what it is about, nothing more.
pub(crate) fn brief_words(state: &PaneState) -> String {
    match state {
        PaneState::Starting => "starting".to_string(),
        PaneState::Working { tool: Some(tool) } => format!("working \u{b7} {tool}"),
        PaneState::Working { tool: None } => "thinking".to_string(),
        PaneState::Idle => "idle".to_string(),
        PaneState::Waiting { tool: Some(tool) } => format!("asking \u{b7} {tool}"),
        PaneState::Waiting { tool: None } => "asking".to_string(),
        PaneState::Exited => "exited".to_string(),
    }
}

/// What the strip says: the folder the agent has wandered into, if it has left
/// the project's own; otherwise what it is doing, for a pane an agent reports
/// for; otherwise the folder it started in, which is at least true.
///
/// Split out of `Head::refresh` because it is the part with a decision in it,
/// and a `gtk4::Label` is not something a unit test should have to own.
///
/// The agent's *name* is not part of it any more. It used to ride on the end of
/// this text - "working · Edit · codex" - and it was the least urgent word in
/// the strip competing for the room the most urgent one needed. It is a badge
/// of its own now, at the strip's far end (see `Pane::spawn`), where a glance
/// finds it without reading.
fn head_base(cwd: Option<&str>, root: &str, reports: bool, state: &PaneState) -> String {
    match cwd {
        Some(cwd) if cwd != root => cwd.to_string(),
        _ if reports => status_words(state),
        _ => root.to_string(),
    }
}

/// The badge an agent's tile wears: its name, set in the stylesheet as a small
/// monospaced tag. One word, and the same one the config file and the menu use.
fn badge_text(kind: Kind) -> &'static str {
    kind.label()
}

/// An editor's badge: the file's extension, or `txt` for a file with none.
fn file_badge(path: &std::path::Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 6)
        .unwrap_or_else(|| "txt".to_string())
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
        let text = head_base(
            self.cwd.borrow().as_deref(),
            &self.root.borrow(),
            self.reports,
            &self.state.borrow(),
        );
        // A label that is set queues a resize, and a resize climbs to the window
        // - so an unchanged strip is left alone rather than re-set, which is
        // most of the time a hook arrives.
        if self.label.label() != text {
            self.label.set_label(&text);
        }
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
    // The desktop's sixteen, when the desktop states them.
    //
    // This is the whole of what "put claude back to its default state" means.
    // The palette below substitutes this app's three signal colours into ANSI
    // 1, 2 and 3 - a deliberate choice, argued for at length, and the reason a
    // claude running here has always looked like this app rather than like the
    // terminal next to it. Under a theme that substitution is the wrong one to
    // make twice: the desktop already has a red, a green and a yellow, every
    // other window on it agrees about them, and claude launched with
    // `dark-ansi` (see `claude_settings_file`) draws its entire interface out
    // of these sixteen slots. Handing it the theme's own is what lets an
    // `omarchy theme set` reach inside a pane at all.
    if let Some(themed) = crate::omarchy::ansi(surface) {
        return themed;
    }

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

    // ANSI 0 asks for the surface's alpha, and does not get it. VTE discards the
    // alpha on palette entries exactly as it discards it on the background above,
    // and unlike the background there is no `set_clear_background` to opt out of:
    // a cell a program paints is a cell VTE fills, opaquely, whatever alpha the
    // palette entry carries.
    //
    // Measured, because the comment that used to sit here asserted the opposite
    // and had been believed for two releases. Three probe panes painting a full
    // screen of background cells at pane_opacity 0.93, sampled out of the
    // window's own render with its alpha channel intact:
    //
    //     ANSI 0 background      (31,40,47) alpha 255   opaque
    //     256-colour background  (48,48,48) alpha 255   opaque
    //     truecolor background   (30,30,30) alpha 255   opaque
    //
    // The request is kept rather than deleted for the reason the one on
    // `background` is: it costs nothing, and it is what VTE would use the day it
    // starts honouring it.
    //
    // What that measurement does *not* mean is that a TUI's pane goes opaque,
    // which is what this comment claimed for about an hour before the claim was
    // checked against the thing it was about. `pane_opacity` reaches every cell a
    // program leaves at the default background, and a full-screen TUI leaves
    // almost all of them there - painting a *layout* is not painting a
    // *background*. claude draws its whole interface and its pane is glass down
    // to the wallpaper; the probes above only went opaque because they were
    // written to paint explicit background cells, which is the uncommon case they
    // were built to isolate. The limitation is real and narrow: it costs
    // translucency exactly where a program asks for a background colour by name -
    // a selected row, a status bar, a diff hunk - and nowhere else.
    //
    // Setting it to the *surface* colour is therefore doing more work than the
    // alpha ever did: a program painting a "black" background lands on the pane's
    // own tone instead of a foreign grey, which is what keeps a painted pane
    // looking like this app rather than like a hole in it.
    //
    // The other fifteen stay opaque and unremapped. They are deliberate colours a
    // program asked for by name, not the surface, and text you can see the
    // desktop through is not what "red" means.
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
    let wanted = (!font.trim().is_empty())
        .then(|| gtk4::pango::FontDescription::from_string(&font));
    // Unchanged is left alone. Every appearance refresh - a theme change, a
    // slider step in Preferences - comes through here for every pane, and VTE
    // rebuilds its glyph cache and reflows the grid on any `set_font`, same
    // font or not.
    let current = terminal.font().map(|f| f.to_str().to_string());
    if current == wanted.as_ref().map(|f| f.to_str().to_string()) {
        return;
    }
    terminal.set_font(wanted.as_ref());
}

/// The `--settings` layer every claude pane is launched with: the hooks that
/// report each moment of its turn, and the bell on the ones worth interrupting
/// someone for. Returns its path, or `None` if it couldn't be written - in
/// which case panes fall back to a plain, silent `claude` rather than failing
/// to start.
///
/// Written out as a file rather than passed inline (`--settings` takes either)
/// because an inline JSON argument would have to survive being quoted through
/// the user's login shell - and that shell can be fish, whose backslash rules
/// inside single quotes differ from POSIX sh's, which is precisely enough to
/// turn the hook's `printf '\a'` into a hook that prints the letter "a". A
/// file has no quoting layers to get wrong.
///
/// Checked on every pane launch instead of only when absent, so a stale hook
/// left behind by an older AgentTileCLI can't outlive the version that wrote
/// it - and rewritten only when what it says has changed (see
/// `hooks::write_if_changed`).
fn claude_settings_file() -> Option<String> {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?
        .join("agenttilecli");
    std::fs::create_dir_all(&dir).ok()?;

    let path = dir.join("claude-settings.json");
    let hook_bin = crate::hooks::hook_bin().ok()?;
    // The theme rides along in the same file the hooks do, and reaches claude
    // the same way: `--settings` outranks `~/.claude/settings.json`, so a user
    // who has pinned `"theme": "dark"` there keeps it in every other terminal
    // and only the panes in this window follow the desktop. Nothing in
    // `~/.claude` is written, which is the promise this file has always kept.
    crate::hooks::write_if_changed(
        &path,
        &crate::hooks::settings_json(&hook_bin, BELL_HOOK, crate::omarchy::claude_theme()),
    )
    .ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// A single tile: a bordered frame containing a VTE terminal, running an agent
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
    /// How many times this pane has been asked to close, so that asking again
    /// asks harder - see `hangup`.
    hangups: Cell<u8>,
    /// What `apply_theme` was last called with, so `set_focused` can skip the
    /// repaint when nothing changed. `Tiler::update_focus_style` runs over
    /// every pane after any pane operation, and all but one of those panes
    /// were already in the state it's about to set them to.
    focused: Cell<bool>,
    /// Which agent this pane runs, or `None` for a pane running a command or
    /// holding a file. Read when a session is saved, so a project reopens with
    /// the agents it had rather than with claudes.
    kind: Option<Kind>,
    /// The agent's own id for the conversation it is having, as it last
    /// reported it (see `wire::Message::session`). Saved with the session, so a
    /// reopened project can hand each agent back the conversation it was in
    /// rather than a blank one - see `agent::Kind::resume_args`.
    session: RefCell<Option<String>>,
    /// Whether anything has been said in that conversation - see
    /// `resumable_session`.
    conversed: Cell<bool>,
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

        // The activity strip: a hairline under the head that the stylesheet
        // turns into a moving sweep while the agent works, and into nothing
        // the rest of the time. A widget of its own rather than a border on
        // the head, because a border can't be animated along its length and
        // this has to read as *motion* - four tiles all holding still look
        // exactly alike whether their agents are thinking or finished, and
        // that sameness is the thing a glance at the grid has to cut through.
        let activity = gtk4::Box::builder()
            .css_classes(["pane-activity"])
            .can_target(false)
            .build();

        let body = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .build();
        body.append(&head);
        body.append(&activity);
        body.append(content);

        let frame = Frame::new(None);
        frame.add_css_class("pane");
        frame.add_css_class(&state_frame_class(&PaneState::Starting));
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

        // An editor's badge is its file's kind rather than an agent's name -
        // it is deliberately not an agent, and the extension is the one fact
        // about a file its name in the strip might have ellipsized away.
        let badge = gtk4::Label::builder()
            .label(file_badge(path))
            .css_classes(["pane-kind", "pane-kind-file"])
            .valign(gtk4::Align::Center)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .can_target(false)
            .build();
        head.insert_child_after(&badge, Some(&head_state.label));

        // The dot, driven by the buffer rather than by hooks: quiet grey while
        // the file matches the disk, the amber "waiting on you" while it
        // doesn't - which is what unsaved changes are.
        status.remove_css_class("starting");
        status.add_css_class("idle");
        status.set_tooltip_text(Some("Saved"));
        set_frame_state(&frame, "idle");
        {
            let status = status.clone();
            let frame = frame.downgrade();
            editor.buffer.connect_modified_changed(move |buffer| {
                let modified = buffer.is_modified();
                for class in STATUS_CLASSES {
                    status.remove_css_class(class);
                }
                status.add_css_class(if modified { "unsaved" } else { "idle" });
                if let Some(frame) = frame.upgrade() {
                    set_frame_state(&frame, if modified { "unsaved" } else { "idle" });
                }
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
            hangups: Cell::new(0),
            focused: Cell::new(false),
            kind: None,
            session: RefCell::new(None),
            conversed: Cell::new(false),
        })
    }

    /// Which agent this pane runs, or `None` for a pane running a command or
    /// holding a file.
    pub fn kind(&self) -> Option<Kind> {
        self.kind
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
        // A terminal with no agent in it - the update script's pane - is not an
        // agent either. Counting it put an extra dot in the rack, made the next
        // project open with one more agent than you work with, and saved it into
        // the session as a claude, which `restore_agents` then started.
        self.kind?;
        match &self.body {
            Body::Terminal(_) => Some(self.state()),
            Body::Editor(_) => None,
        }
    }

    /// The conversation this pane's agent is in, if it has said.
    pub fn session(&self) -> Option<String> {
        self.session.borrow().clone()
    }

    /// Records the conversation, and says whether it is a new one to this pane.
    ///
    /// A different id is a different conversation - claude's `/clear` starts
    /// one mid-pane - and a new conversation has nothing in it yet.
    pub fn set_session(&self, session: &str) -> bool {
        if self.session.borrow().as_deref() == Some(session) {
            return false;
        }
        *self.session.borrow_mut() = Some(session.to_string());
        self.conversed.set(false);
        true
    }

    /// Notes that something has been said in this pane's conversation.
    pub fn mark_conversed(&self) {
        self.conversed.set(true);
    }

    /// The conversation to hand back on a resume - only once there is one.
    ///
    /// An agent names its conversation the moment it starts, and writes it to
    /// disk only once something is said in it. So the id of an agent that was
    /// opened and never spoken to names a conversation that doesn't exist, and
    /// resuming it failed: the agent printed "no conversation found", exited,
    /// and took its pane - and its place in the session - with it. Until then,
    /// the pane comes back as a new conversation instead.
    pub fn resumable_session(&self) -> Option<String> {
        self.session().filter(|_| self.conversed.get())
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

    /// The usual pane: an agent of `kind`, running in `cwd` - with `BELL_HOOK`
    /// installed, so a finished or waiting agent lights up its group's sidebar
    /// row.
    ///
    /// How the hooks get in front of the agent is the whole of what differs,
    /// and the whole reason `agent` exists. Claude takes a
    /// `--settings` file, which layers over the user's own settings rather than
    /// replacing them. Codex takes no such flag, so it is handed a `CODEX_HOME`
    /// built for the purpose instead. Grok likewise reads hooks from its home,
    /// so it gets a private `GROK_HOME`. All three routes keep the same promise:
    /// nothing in the user's agent homes is written merely to install our hook,
    /// and agents in other terminals are untouched.
    ///
    /// All are best-effort in the same way, too. If the hooks can't be
    /// installed for any reason the pane still gets a perfectly good agent -
    /// just a silent one, which is exactly what every pane was before any of
    /// this existed.
    ///
    /// `launch` says which conversation: a new one, a saved one picked up again,
    /// or a new one in a git worktree of its own - see `agent::Launch`.
    pub fn new(cwd: &str, kind: Kind, launch: &Launch) -> Self {
        // A new conversation is named here, where the agent allows it, so the
        // pane can offer it back later even if no hook ever reaches the window.
        // A resumed one already has its name.
        let session = match launch {
            Launch::Resume(id) => Some(id.clone()),
            Launch::Fresh | Launch::Worktree if kind.names_its_session() => {
                crate::agent::new_session_id()
            }
            Launch::Fresh | Launch::Worktree => None,
        };
        let mut configured = crate::config::get().command_for(kind);
        // Claude's hooks ride on the command line, and they go on *before* the
        // launch's own flags rather than after: `--worktree` takes an optional
        // name, and an optional value followed by `--settings <path>` is a
        // parse away from a worktree called `--settings` and a claude with no
        // hooks at all. Last is the one place an optional value is never
        // ambiguous.
        if kind == Kind::Claude
            && let Some(path) = claude_settings_file()
        {
            configured = format!("{configured} --settings {}", crate::update::sh_quote(&path));
        }
        let configured = kind.command_line(&configured, launch, session.as_deref());
        let (command, env) = match kind {
            Kind::Claude => (configured, Vec::new()),
            // Codex takes no flag for this. Its hooks come from its home, so
            // the home is what gets pointed somewhere else - see `codex_home`.
            //
            // In the environment rather than prefixed onto the command line:
            // it is per-pane state, it belongs beside the per-pane state
            // already going through VTE, and putting it there dodges a quoting
            // layer that has cost this file real bugs before.
            Kind::Codex => {
                let env = crate::hooks::hook_bin()
                    .ok()
                    .and_then(|bin| crate::codex_home::prepare(&bin, BELL_HOOK))
                    .map(|home| vec![format!("CODEX_HOME={}", home.display())])
                    .unwrap_or_default();
                (configured, env)
            }
            // Grok discovers global hooks under `$GROK_HOME/hooks/*.json`.
            // Pointing it at a mirrored private home lets this pane keep the
            // user's auth, config, sessions and hooks without installing our
            // hook into every Grok process they launch elsewhere.
            Kind::Grok => {
                let env = crate::hooks::hook_bin()
                    .ok()
                    .and_then(|bin| crate::grok_home::prepare(&bin, BELL_HOOK))
                    .map(|home| vec![format!("GROK_HOME={}", home.display())])
                    .unwrap_or_default();
                (configured, env)
            }
        };
        // A resumed conversation has been spoken in, by definition.
        let conversed = matches!(launch, Launch::Resume(_));
        Self::spawn(cwd, &command, true, Some(kind), env, session, conversed)
    }

    /// A pane running `command` instead of `claude` (via the same login
    /// shell, so it resolves against the same PATH) - used by the update
    /// button, which runs the pull-and-rebuild script in a pane so its
    /// output is visible rather than hidden behind a spinner.
    pub fn command(cwd: &str, command: &str) -> Self {
        Self::spawn(cwd, command, false, None, Vec::new(), None, false)
    }

    /// The shared body of the two above. `reports` says whether an agent's
    /// hooks will ever speak for this pane, which is what its head strip is
    /// allowed to claim - see `Head::reports`. `kind` is the agent the strip
    /// names, and `extra_env` whatever else that agent needs in its
    /// environment to be reachable - currently the private home used by Codex
    /// or Grok. `session` is the conversation it starts in, when known.
    fn spawn(
        cwd: &str,
        command: &str,
        reports: bool,
        kind: Option<Kind>,
        extra_env: Vec<String>,
        session: Option<String>,
        conversed: bool,
    ) -> Self {
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
        // The agent's name, as a badge at the strip's far end - see `head_base`
        // for why it stopped being the tail of the sentence. A class per kind
        // as well, so a stylesheet can tell them apart if it ever wants to.
        if let Some(kind) = kind {
            let badge = gtk4::Label::builder()
                .label(badge_text(kind))
                .css_classes(["pane-kind", &format!("pane-kind-{}", kind.label())])
                .valign(gtk4::Align::Center)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .can_target(false)
                .build();
            head.insert_child_after(&badge, Some(&head_label));
        }

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
            set_frame_state(&frame, "idle");
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
        if let (Some(socket), Ok(bin)) = (crate::ipc::socket(), crate::hooks::hook_bin()) {
            env.push(format!("{}={id}", crate::ipc::ENV_PANE));
            env.push(format!("{}={socket}", crate::ipc::ENV_SOCKET));
            env.push(format!("{}={bin}", crate::ipc::ENV_BIN));
        }
        // Whatever this particular agent needs beyond that. Added after the
        // three above rather than before, purely so a bug in one is never a
        // bug in the others.
        env.extend(extra_env);
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
        // Whole seconds, through GLib's own coalescing timer, so every pane's
        // poll lands in one wakeup a second rather than sixteen staggered ones.
        gtk4::glib::source::timeout_add_seconds_local(CWD_POLL_SECONDS, move || {
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
            hangups: Cell::new(0),
            focused: Cell::new(false),
            kind,
            session: RefCell::new(session),
            conversed: Cell::new(conversed),
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
        set_frame_state(&self.frame, status_class(&state));
        self.status
            .set_tooltip_text(Some(&status_tooltip(&state)));
        *self.head.state.borrow_mut() = state;
        // The strip carries the state in words whenever the folder isn't news,
        // so a state change is a change to what it reads.
        self.head.refresh();
        true
    }

    /// Repaints the terminal in the focused or unfocused surface, to match the
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

    /// Asks the child (shell + agent) to exit, mirroring how a real terminal
    /// emulator closes a tab - and asks harder each time it is asked again.
    /// Actual removal from the layout happens via the `child-exited` signal the
    /// caller wires up separately.
    ///
    /// Returns `false` when there is no process to ask: a pane whose folder had
    /// gone, or whose spawn failed, never had one - and no `child-exited` will
    /// ever come for it, so the caller has to take it down itself. Before this
    /// said so, those panes could not be closed at all: the ✕ faded them to a
    /// ghost that sat in the grid for the rest of the session, its own text
    /// still saying "close this pane".
    ///
    /// The first time is SIGHUP, which is what a closing terminal sends and what
    /// every agent treats as "you are done". An agent wedged badly enough to
    /// ignore that used to be unclosable too, since the pid was forgotten after
    /// the first try; now a second close is SIGTERM and a third SIGKILL, which
    /// nothing ignores.
    ///
    /// The pid is kept until the child is reaped (see `forget_process`) rather
    /// than dropped here. Until VTE reaps it, the kernel holds the pid as a
    /// zombie, so it cannot have been handed to another process - and VTE
    /// reaps and reports in one step, with no turn of the main loop between for
    /// a click to land in.
    pub fn hangup(&self) -> bool {
        let Some(pid) = self.pid.get() else {
            return false;
        };
        let signal = match self.hangups.replace(self.hangups.get().saturating_add(1)) {
            0 => libc::SIGHUP,
            1 => libc::SIGTERM,
            _ => libc::SIGKILL,
        };
        // The child's whole process group, not just the child. VTE starts it as
        // a session leader, so its pid doubles as the group id, and the
        // processes that actually matter are its descendants: closing the update
        // pane has to stop the `cargo build` underneath the update script, which
        // would otherwise run to completion and replace the installed binary long
        // after the user shut the pane to call the whole thing off.
        //
        // No fallback to signalling the pid alone. `killpg` failing means the
        // group is already gone, and a pid whose group is gone is exactly the
        // pid that may by now belong to somebody else.
        unsafe {
            libc::killpg(pid, signal);
        }
        true
    }

    /// Forgets the child, once it has been reaped - see `hangup`.
    pub fn forget_process(&self) {
        self.pid.set(None);
    }
}

#[cfg(test)]
mod head_tests {
    use super::*;

    /// The strip says what the agent is doing, and nothing else competes for
    /// its room: the agent's name is a badge now.
    #[test]
    fn the_strip_says_what_the_agent_is_doing() {
        let working = PaneState::Working { tool: Some("Edit".into()) };
        assert_eq!(head_base(None, "webapp", true, &working), "working \u{b7} Edit");
        assert_eq!(
            head_base(Some("webapp"), "webapp", true, &PaneState::Idle),
            "waiting for you",
            "the project's own folder is not news",
        );
    }

    /// An agent that has wandered out of the project's folder is the one case
    /// where naming a folder tells you something you didn't know.
    #[test]
    fn a_folder_is_named_only_when_it_is_news() {
        assert_eq!(
            head_base(Some("migrations"), "webapp", true, &PaneState::Idle),
            "migrations",
        );
    }

    /// A pane nothing reports for - the update script's - names its folder
    /// forever rather than claiming to be "starting" forever.
    #[test]
    fn a_pane_with_no_agent_names_its_folder() {
        assert_eq!(head_base(None, "agenttilecli", false, &PaneState::Starting), "agenttilecli");
    }

    #[test]
    fn every_agent_has_a_badge_and_every_file_a_kind() {
        for kind in Kind::ALL {
            assert_eq!(badge_text(kind), kind.label());
        }
        assert_eq!(file_badge(std::path::Path::new("src/main.RS")), "rs");
        assert_eq!(file_badge(std::path::Path::new("Makefile")), "txt");
        assert_eq!(file_badge(std::path::Path::new("x.averylongext")), "txt");
    }

    /// The asking tile names what it is asking about, first, where the eye
    /// already looks.
    #[test]
    fn an_asking_strip_leads_with_the_question() {
        let text = head_base(None, "p", true, &PaneState::Waiting { tool: Some("Bash".into()) });
        assert!(text.starts_with("asking permission"), "{text}");
        assert!(text.ends_with("Bash"), "{text}");
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

    /// The head strip is ink, not paint: no fill of its own, only a gradient
    /// and a rule in tints of @text.
    ///
    /// It used to be 0.55 of @shadow, a number derived to land on @rack over the
    /// stock ramp - and over a light Omarchy theme, where @shadow is a mid-grey,
    /// it laid a heavy grey bar across every tile. Tints of the ink are right in
    /// both directions (lighter on dark, darker on light) and add nothing a glass
    /// tile could turn into an opaque bar. This holds the rule to that shape, so
    /// the next change can't quietly bring the paint back.
    #[test]
    fn a_head_strip_is_ink_not_paint() {
        let css = include_str!("style.css");
        let start = css.find("\n.pane-head {").expect("a .pane-head rule");
        let body = &css[start..];
        let body = &body[..body.find('}').expect("an unterminated rule")];
        assert!(body.contains("background-color: transparent;"), "{body}");
        assert!(body.contains("linear-gradient(to bottom, alpha(@text"), "{body}");
        assert!(body.contains("border-bottom: 1px solid alpha(@text"), "{body}");
        assert!(!body.contains("@shadow"), "the strip is painted in @shadow again: {body}");
    }

    /// The tint that stands in for the strip's old fill must still separate it
    /// from the tile on the stock ramp: @text at the gradient's strongest is a
    /// visible step, not a rounding error.
    #[test]
    fn the_strip_still_reads_as_a_strip() {
        let tile = palette::color("tile");
        let lifted = tinted(tile, palette::color("text"), 0.07);
        assert!(
            lifted.0 - i32::from(tile.r) >= 10,
            "the strip's top is {lifted:?} over a tile of {tile:?} - no longer visibly a strip",
        );
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
