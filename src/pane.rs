use std::cell::Cell;
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{gdk, Frame};
use vte4::{prelude::*, PtyFlags, Terminal};

/// How often to re-check a pane's current directory. Cheap (a single
/// syscall pair per pane) so a short interval is fine.
const CWD_POLL_INTERVAL: Duration = Duration::from_millis(1000);

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

/// The last path component of `path` ("/" if the path itself is root), with
/// the kernel's " (deleted)" marker (present when the directory has been
/// removed out from under the process) stripped first so it never leaks
/// into the displayed name.
fn folder_name(path: &str) -> String {
    let path = path.strip_suffix(" (deleted)").unwrap_or(path);
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Modern dark "gunmetal" theme, applied to every terminal's own palette
/// (independent of the CSS around it, since VTE paints its own background/
/// text colors rather than taking them from GTK CSS). Loosely an "One Dark"
/// style 16-color ANSI palette so things like `ls --color`/git diffs still
/// read well against it.
fn rgba(hex: &str) -> gdk::RGBA {
    gdk::RGBA::parse(hex).expect("valid hex color")
}

fn apply_theme(terminal: &Terminal) {
    let foreground = rgba("#e6e6e6");
    let background = rgba("#1a1d22");
    let palette = [
        rgba("#1a1d22"), // black
        rgba("#e06c75"), // red
        rgba("#98c379"), // green
        rgba("#e5c07b"), // yellow
        rgba("#61afef"), // blue
        rgba("#c678dd"), // magenta
        rgba("#56b6c2"), // cyan
        rgba("#d6d9dd"), // white
        rgba("#5c6370"), // bright black
        rgba("#e06c75"), // bright red
        rgba("#98c379"), // bright green
        rgba("#e5c07b"), // bright yellow
        rgba("#61afef"), // bright blue
        rgba("#c678dd"), // bright magenta
        rgba("#56b6c2"), // bright cyan
        rgba("#ffffff"), // bright white
    ];
    let palette_refs: Vec<&gdk::RGBA> = palette.iter().collect();
    terminal.set_colors(Some(&foreground), Some(&background), &palette_refs);
}

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_YELLOW: &str = "\x1b[1;33m";
const BOLD_WHITE: &str = "\x1b[1;37m";
const BOLD_GREEN: &str = "\x1b[1;32m";

fn sgr(code: &str, s: &str) -> String {
    format!("{code}{s}{RESET}")
}

/// A `key   description` row, aligned to a fixed key-column width so every
/// row's description lines up regardless of key length.
fn row(keys: &str, desc: &str) -> String {
    format!("  {}  {}", sgr(BOLD_WHITE, &format!("{keys:<16}")), desc)
}

fn section(title: &str, rows: &[(&str, &str)]) -> String {
    let mut s = format!("  {}\r\n", sgr(BOLD_YELLOW, title));
    for (keys, desc) in rows {
        s.push_str(&row(keys, desc));
        s.push_str("\r\n");
    }
    s
}

fn help_text() -> String {
    let title = " AgentTileCLI \u{2014} dynamic tiling window manager for AI CLI sessions ";
    let box_width = title.chars().count();
    let top = format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(box_width));
    let mid = format!("\u{2502}{}\u{2502}", sgr(BOLD_CYAN, title));
    let bottom = format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(box_width));

    let getting_started = format!(
        "  {header}\r\n\r\n  \
         Press {key} (or click the {plus} button, bottom-right) to spawn\r\n  \
         a pane. A folder picker opens \u{2014} {pick} to work in, and\r\n  \
         claude launches right there. Cancel just reuses your last pick.",
        header = sgr(BOLD_GREEN, "\u{25b6} GETTING STARTED"),
        key = sgr(BOLD_WHITE, "Super+Alt+Return"),
        plus = sgr(BOLD_WHITE, "+"),
        pick = sgr(BOLD_GREEN, "choose the project folder"),
    );

    let panes = section(
        "PANES",
        &[
            ("Return", "spawn a new claude pane"),
            ("Shift+Return", "promote focused pane to master (zoom)"),
            ("j  /  k", "focus next / previous pane"),
            ("w", "close the focused pane"),
        ],
    );
    let layout = section(
        "LAYOUT",
        &[
            ("h  /  l", "shrink / grow the master column"),
            ("i  /  d", "more / fewer master panes"),
            ("m", "toggle monocle (focused pane fullscreen)"),
            ("Tab", "cycle layout mode: grid \u{2192} master-stack \u{2192} monocle"),
        ],
    );
    let text_size = section(
        "TEXT SIZE",
        &[
            ("=  /  -", "enlarge / shrink terminal text"),
            ("0", "reset text size"),
        ],
    );
    let help = section("HELP", &[("/", "toggle this help pane")]);
    let mouse = section(
        "MOUSE",
        &[
            ("click pane", "focus it"),
            ("drag a seam", "resize the panes on either side"),
            ("click \u{2715}", "close that pane"),
            ("click +", "spawn a new pane (bottom-right button)"),
        ],
    );

    format!(
        "{top}\r\n{mid}\r\n{bottom}\r\n\r\n\
         {getting_started}\r\n\r\n\
         {modifier}\r\n\r\n\
         {panes}\r\n{layout}\r\n{text_size}\r\n{help}\r\n{mouse}\r\n\
         {tip}\r\n",
        modifier = sgr(DIM, "  every keybinding above is held together with Super+Alt"),
        tip = sgr(
            DIM,
            "  tip: panes auto re-tile on spawn/close/promote. grid mode resets\r\n  \
             to equal sizes whenever a pane opens or closes; master-stack's\r\n  \
             divider position persists across pane changes instead. drag any\r\n  \
             seam anytime to nudge sizes. the folder picker remembers your\r\n  \
             last choice - press Escape/Cancel to reuse it without picking\r\n  \
             again. the corner label tracks the pane's real directory, so it\r\n  \
             won't follow claude's own /cd command (that's purely internal\r\n  \
             to claude, invisible outside its process). this pane has no\r\n  \
             process behind it; close it like any other with Super+Alt+w."
        ),
    )
}

/// A single tile: a bordered frame containing a VTE terminal. Most panes run
/// `claude` via the user's login shell (so PATH/nvm/aliases resolve the same
/// way an interactive terminal would); the help pane instead just has static
/// text fed directly into it, with no child process at all.
pub struct Pane {
    pub frame: Frame,
    pub terminal: Terminal,
    pub close_button: gtk4::Button,
    pid: Rc<Cell<Option<libc::pid_t>>>,
    is_help: bool,
}

impl Pane {
    /// Builds the shared frame/terminal/overlay/close-button scaffold every
    /// pane needs. Returns the `Overlay` (rather than baking in every
    /// possible overlay child) so callers add only the extra chrome they
    /// actually need - e.g. `new()` adds a directory label but `help()`
    /// doesn't, instead of `help()` having to receive and discard one.
    fn bare() -> (Frame, Terminal, gtk4::Overlay, gtk4::Button) {
        let terminal = Terminal::new();
        terminal.set_hexpand(true);
        terminal.set_vexpand(true);
        apply_theme(&terminal);

        let close_button = gtk4::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "circular", "pane-close"])
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::Start)
            .can_focus(false)
            .build();

        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&terminal));
        overlay.add_overlay(&close_button);

        let frame = Frame::new(None);
        frame.add_css_class("pane");
        frame.set_overflow(gtk4::Overflow::Hidden);
        frame.set_child(Some(&overlay));

        (frame, terminal, overlay, close_button)
    }

    pub fn new(cwd: &str) -> Self {
        let (frame, terminal, overlay, close_button) = Self::bare();
        let pid = Rc::new(Cell::new(None));

        let dir_label = gtk4::Label::builder()
            .css_classes(["pane-dir"])
            .halign(gtk4::Align::Start)
            .valign(gtk4::Align::Start)
            .can_target(false)
            .label(folder_name(cwd))
            .build();
        overlay.add_overlay(&dir_label);

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let argv = [shell.as_str(), "-lc", "claude"];
        let pid_slot = pid.clone();
        terminal.spawn_async(
            PtyFlags::DEFAULT,
            Some(cwd),
            &argv,
            &[],
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
            frame,
            terminal,
            close_button,
            pid,
            is_help: false,
        }
    }

    /// A static cheatsheet pane: no PTY, no child process, just fed text.
    pub fn help() -> Self {
        let (frame, terminal, _overlay, close_button) = Self::bare();
        terminal.feed(help_text().as_bytes());
        Pane {
            frame,
            terminal,
            close_button,
            pid: Rc::new(Cell::new(None)),
            is_help: true,
        }
    }

    /// Whether this is the static help pane (as opposed to a real `claude`
    /// pane). Note this is *not* simply "no pid yet" - a freshly spawned
    /// claude pane also has no pid for a moment, since spawn_async's
    /// callback (which records it) runs asynchronously on the main loop.
    pub fn is_help(&self) -> bool {
        self.is_help
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
            unsafe {
                libc::kill(pid, libc::SIGHUP);
            }
        }
    }
}
