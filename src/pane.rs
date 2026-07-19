use std::cell::Cell;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{gdk, Frame};
use vte4::{prelude::*, PtyFlags, Terminal};

/// How often to re-check a pane's current directory. Cheap (a single
/// syscall pair per pane) so a short interval is fine.
const CWD_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// The shell one-liner claude runs when it finishes a turn (`Stop`) or stops
/// to ask for something (`Notification`) - the two moments a watching human
/// would want to know about, and the two this app repaints a sidebar row for
/// (see `Groups::flash_row`). All it does is ring the pane's bell, which VTE
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

/// Modern dark "gunmetal" theme, applied to every terminal's own palette
/// (independent of the CSS around it, since VTE paints its own background/
/// text colors rather than taking them from GTK CSS). Loosely an "One Dark"
/// style 16-color ANSI palette so things like `ls --color`/git diffs still
/// read well against it.
fn rgba(hex: &str) -> gdk::RGBA {
    gdk::RGBA::parse(hex).expect("valid hex color")
}

fn apply_theme(terminal: &Terminal) {
    // Matched to `.pane`'s own fill in style.css (`@gm-surface`), so a pane is
    // one continuous surface rather than a terminal of one shade sitting in a
    // frame of another - the seam is visible at any size, and it's the thing
    // that makes a tiling app look assembled rather than designed.
    let foreground = rgba("#ebeef1");
    let background = rgba("#202428");
    // ANSI 0 and 7 are pulled onto the gunmetal ramp too: programs paint
    // "black" backgrounds and "white" text far more often than they mean the
    // literal colours, so anything else leaves rectangles of a foreign grey
    // sitting in the middle of the pane.
    let palette = [
        rgba("#202428"), // black - the surface itself
        rgba("#e06b6b"), // red
        rgba("#92c47f"), // green
        rgba("#d8a657"), // yellow - the amber the sidebar flashes in
        rgba("#6ab0de"), // blue - the accent used throughout the chrome
        rgba("#bf93d6"), // magenta
        rgba("#63bcbb"), // cyan
        rgba("#d1d6da"), // white
        rgba("#626870"), // bright black
        rgba("#ef8a8a"), // bright red
        rgba("#a8d795"), // bright green
        rgba("#ecc07a"), // bright yellow
        rgba("#8cc8ec"), // bright blue
        rgba("#d3ade4"), // bright magenta
        rgba("#82d0cf"), // bright cyan
        rgba("#f5f7f9"), // bright white
    ];
    let palette_refs: Vec<&gdk::RGBA> = palette.iter().collect();
    terminal.set_colors(Some(&foreground), Some(&background), &palette_refs);

    // The three colours VTE does *not* take from the palette, and which
    // otherwise arrive from the ambient GTK theme - which is how a carefully
    // built dark palette ends up with a stock-blue selection and a white block
    // cursor in the middle of it.
    terminal.set_color_cursor(Some(&rgba("#6ab0de")));
    terminal.set_color_cursor_foreground(Some(&background));
    // Selection as a tint rather than an inversion: the text stays its own
    // colour and keeps its syntax highlighting, which matters here because
    // selecting is now how you copy (see `clipboard`), so it happens over real
    // output rather than over a blank prompt.
    terminal.set_color_highlight(Some(&rgba("#2f4657")));
    terminal.set_color_highlight_foreground(Some(&foreground));
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
    format!("  {}  {}", sgr(BOLD_WHITE, &format!("{keys:<14}")), desc)
}

fn section(title: &str, rows: &[(&str, &str)]) -> String {
    let mut s = format!("  {}\r\n", sgr(BOLD_YELLOW, title));
    for (keys, desc) in rows {
        s.push_str(&row(keys, desc));
        s.push_str("\r\n");
    }
    s
}

/// On-screen character count of a line that may contain ANSI SGR color
/// codes (`\x1b[...m`), which have zero display width. Lets `side_by_side`
/// pad columns to line up regardless of the color codes baked into them.
fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for esc in chars.by_ref() {
                if esc == 'm' {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

/// Lays two multi-line blocks side by side as one column pair, padding every
/// left-block line to the widest line in that block (plus `gap` spaces) so
/// the right block starts in a straight column regardless of left content.
/// Shorter block is padded with blank rows to match the taller one's height.
fn side_by_side(left: &str, right: &str, gap: usize) -> String {
    let left_lines: Vec<&str> = left.trim_end_matches("\r\n").split("\r\n").collect();
    let right_lines: Vec<&str> = right.trim_end_matches("\r\n").split("\r\n").collect();
    let col_width = left_lines.iter().map(|l| visible_len(l)).max().unwrap_or(0);
    let rows = left_lines.len().max(right_lines.len());
    let mut out = String::new();
    for i in 0..rows {
        let l = left_lines.get(i).copied().unwrap_or("");
        let r = right_lines.get(i).copied().unwrap_or("");
        out.push_str(l);
        if !r.is_empty() {
            out.push_str(&" ".repeat(col_width - visible_len(l) + gap));
            out.push_str(r);
        }
        out.push_str("\r\n");
    }
    out
}

/// A numbered "1. do the thing" step, indented to sit under its heading.
fn step(n: usize, text: &str) -> String {
    format!("  {} {}", sgr(BOLD_GREEN, &format!("{n}.")), text)
}

/// Blank-pads `block` out to `height` lines. Used to make the first section
/// in every help column the same height, so the second section in each
/// column starts on the same row across all three - otherwise each column's
/// second heading lands at a different y and the page reads as ragged.
fn pad_to(block: &str, height: usize) -> String {
    let lines = block.trim_end_matches("\r\n").split("\r\n").count();
    let mut out = block.trim_end_matches("\r\n").to_string();
    for _ in lines..height {
        out.push_str("\r\n");
    }
    out
}

/// A dim "· fact" bullet, one self-contained fact per line - short enough to
/// scan rather than hard-wrapped into a paragraph nobody reads.
fn bullet(text: &str) -> String {
    sgr(DIM, &format!("  \u{b7} {text}"))
}

fn help_text() -> String {
    let title = " AgentTileCLI \u{2014} dynamic tiling window manager for AI CLI sessions ";
    let box_width = title.chars().count();
    let top = format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(box_width));
    let mid = format!("\u{2502}{}\u{2502}", sgr(BOLD_CYAN, title));
    let bottom = format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(box_width));

    // Numbered steps rather than a wrapped paragraph: the reader wants to
    // know what to press first, not to read prose.
    let getting_started = format!(
        "  {header}\r\n\r\n{s1}\r\n{s2}\r\n{s3}\r\n\r\n{after}",
        header = sgr(BOLD_GREEN, "\u{25b6} GETTING STARTED"),
        s1 = step(
            1,
            &format!(
                "Press {key}  (or click {hamburger} top-left, then {plus} in the sidebar)",
                key = sgr(BOLD_WHITE, "Super+Alt+Return"),
                hamburger = sgr(BOLD_WHITE, "\u{2630}"),
                plus = sgr(BOLD_WHITE, "+"),
            )
        ),
        s2 = step(2, "Pick the project folder to work in"),
        s3 = step(
            3,
            &format!(
                "Choose how many agents to start with ({counts}) \u{2014} claude launches right there",
                counts = sgr(BOLD_WHITE, "1-4"),
            )
        ),
        after = sgr(
            DIM,
            "  Cancel either dialog and nothing is created. Once a project is open, the\r\n  \
             new-agent button (bottom-right) adds another agent to it \u{2014} no picker.",
        ),
    );

    let panes = section(
        "PANES",
        &[
            ("Shift+Return", "promote to master (zoom)"),
            ("j  /  k", "focus next / previous pane"),
            ("w", "close the focused pane"),
        ],
    );
    let text_size = section(
        "TEXT SIZE",
        &[
            ("=  /  -", "enlarge / shrink text (whole app)"),
            ("0", "reset text size"),
        ],
    );
    // The one section whose keys aren't Super+Alt anything - they're the
    // terminal's own clipboard keys, so they're spelled out in full and the
    // heading says so, rather than being silently exempted from the Super+Alt
    // line above them.
    let clipboard = section(
        "CLIPBOARD (no Super+Alt)",
        &[
            ("Ctrl+V", "paste (an image if one's copied)"),
            ("Shift+Insert", "paste the text, never the image"),
            (
                "Ctrl+C",
                "copy \u{2014} or interrupt, if nothing's selected",
            ),
        ],
    );
    let layout = section(
        "LAYOUT",
        &[
            ("h  /  l", "shrink / grow master column"),
            ("i  /  d", "more / fewer master panes"),
            ("m", "toggle monocle (fullscreen)"),
            ("Tab", "cycle grid \u{2192} master-stack \u{2192} monocle"),
        ],
    );
    let groups = section(
        "GROUPS",
        &[
            ("Return", "new project as a new group"),
            ("g", "toggle the project sidebar"),
            ("[  /  ]", "previous / next group"),
            ("{  /  }", "move this group up / down"),
        ],
    );
    let mouse = section(
        "MOUSE",
        &[
            ("click pane", "focus it"),
            ("drag a seam", "resize panes on either side"),
            ("click \u{2715}", "close that pane"),
            ("click \u{2630}", "toggle the project sidebar"),
            ("sidebar +", "open a new project as a new group"),
            ("sidebar row", "switch to that group"),
            ("drag a row", "reorder the projects"),
            ("sidebar \u{21bb}", "check GitHub for a newer version"),
            ("new-agent btn", "spawn another agent in this group"),
        ],
    );
    let help = section(
        "HELP",
        &[("/", "toggle this help pane"), ("u", "check for updates")],
    );

    // Built as three full-height columns fed through one `side_by_side`
    // chain, rather than as separate side-by-side *rows* stacked up: a row
    // pads only to its own widest line, so stacked rows would each land
    // their columns at a different x. One chain over whole columns keeps
    // every section's key/description gutters aligned down the whole page.
    // Each column's first section is padded to a common height (see
    // `pad_to`) so the second one starts on the same row in all three.
    let top_height = [&panes, &layout, &mouse]
        .iter()
        .map(|s| s.trim_end_matches("\r\n").split("\r\n").count())
        .max()
        .unwrap_or(0);
    let col_a = format!(
        "{}\r\n\r\n{text_size}\r\n{clipboard}",
        pad_to(&panes, top_height)
    );
    let col_b = format!("{}\r\n\r\n{groups}", pad_to(&layout, top_height));
    let col_c = format!("{}\r\n\r\n{help}", pad_to(&mouse, top_height));
    let keys = side_by_side(&side_by_side(&col_a, &col_b, 6), &col_c, 6);

    let tips = [
        bullet("Panes auto re-tile on spawn/close/promote, and every grid cell is the same size."),
        bullet("Drag any seam to size panes by hand; master-stack keeps its divider where you put it."),
        bullet("Adding panes never resizes the window \u{2014} they tile smaller inside the size you set."),
        bullet("A pane's corner label tracks its real directory, not claude's own /cd."),
        bullet("Ctrl+V saves a copied image as a PNG and types its short path in for you \u{2014} claude reads the picture from there."),
        bullet("Ctrl+C only copies when something is selected; with nothing selected it's the usual interrupt."),
        bullet("Switching groups doesn't stop the others' agents; closing a group's \u{2715} hangs up every agent in it."),
        bullet("This help pane has no process behind it \u{2014} close it like any other, with Super+Alt+w."),
    ]
    .join("\r\n");

    format!(
        "{top}\r\n{mid}\r\n{bottom}\r\n\r\n\
         {getting_started}\r\n\r\n\
         {modifier}\r\n\r\n\
         {keys}\r\n\
         {tips_header}\r\n{tips}\r\n",
        modifier = sgr(
            BOLD_YELLOW,
            "  \u{25b8} Every keybinding below is held together with Super+Alt",
        ),
        tips_header = sgr(BOLD_YELLOW, "  GOOD TO KNOW"),
    )
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
fn claude_settings_json() -> String {
    // The hook is a shell one-liner full of quotes and a backslash, going into
    // a JSON string: escape both, in that order.
    let hook = BELL_HOOK.replace('\\', r"\\").replace('"', "\\\"");
    let entry = format!(r#"[{{"hooks":[{{"type":"command","command":"{hook}"}}]}}]"#);
    format!(r#"{{"hooks":{{"Stop":{entry},"Notification":{entry}}}}}"#)
}

/// Writes `claude_settings_json` under the user's cache directory and returns
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
    std::fs::write(&path, claude_settings_json()).ok()?;
    Some(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    /// The hook is a shell one-liner embedded in a JSON string, so every
    /// character in it crosses two escaping layers. A single backslash lost on
    /// the way turns `printf '\a'` - the entire point of the hook - into one
    /// that prints the letter "a", and *nothing downstream would say so*:
    /// claude runs it happily, the pane quietly prints an "a", and the sidebar
    /// row just never lights up.
    #[test]
    fn bell_hook_survives_json_escaping() {
        let json = claude_settings_json();

        // The bell byte, still an escape sequence after JSON encoding rather
        // than a stray backslash that ate the "a".
        assert!(
            json.contains(r#"printf '\\a'"#),
            "bell byte mangled: {json}"
        );
        // The hook's own double quotes, escaped instead of ending the JSON
        // string early.
        assert!(json.contains(r#"\"$PTY\""#), "shell quotes mangled: {json}");

        // Both moments worth interrupting someone for: finished, and asking.
        assert!(json.contains(r#""Stop""#));
        assert!(json.contains(r#""Notification""#));
    }
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
        // An agent's bell is this app's "the agent wants you" signal - it's
        // what lights up the group's sidebar row (see `Groups::flash_row`).
        // Turning the *audible* half off keeps that a visual notification
        // rather than a room-filling one, which matters when several agents
        // are working at once. VTE still emits the `bell` signal either way;
        // this only suppresses the beep.
        terminal.set_audible_bell(false);
        // VTE has no clipboard keybindings of its own, so without this a pane
        // can't be pasted into at all. Installed here rather than per pane kind
        // so the help pane can be copied *out of* too.
        crate::clipboard::install(&terminal);

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
        let argv = [shell.as_str(), "-lc", command];
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
