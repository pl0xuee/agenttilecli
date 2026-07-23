//! The window: a sidebar of projects beside a stack of tiled agent panes.
//!
//! This replaces what used to be `groups.rs`, and it is deliberately thinner
//! than that file was. Two things moved out from under it. The ordering rules -
//! which project is where, which one is visible, what "next" means - are in
//! `model`, where they can be tested without a display. The update control is in
//! `updates`, which never had anything to do with tiling and only lived here
//! because its button used to sit in the sidebar. What's left is the part
//! that genuinely is a window: widgets, and the wiring between them and the
//! model.
//!
//! The structure is libadwaita's rather than hand-built:
//!
//! ```text
//! AdwApplicationWindow
//! └── AdwToastOverlay
//!     └── AdwOverlaySplitView          collapses to an overlay on a narrow window
//!         ├── sidebar: AdwToolbarView  header / project rows / version footer
//!         └── content: AdwToolbarView  header / Stack of Tilers
//! ```
//!
//! The split view is worth the dependency on its own. What it replaces was a
//! `Revealer` inside a `Box`, plus three separate `hexpand(false)` workarounds
//! fighting GTK's expand propagation to stop a *collapsed* sidebar claiming
//! width against the panes. None of that survives here, because none of it is
//! this widget's problem.
//!
//! The header bar is new, and it exists to say things the app previously knew
//! but never told anyone: which project you're in, and which layout mode you're
//! in. The second was the worse gap - cycling modes changed the tiling and
//! nothing on screen said what it had changed to.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4::gdk;

use crate::layout::Mode;
use crate::model::{ProjectId, ProjectStore};
use crate::tiler::Tiler;
use crate::update;
use crate::updates::Updates;

mod header;
mod projects;
mod sidebar;

/// Set on the sidebar row of a background project whose agent has asked for the
/// user, and on the sidebar toggle alongside it. Cleared the moment that project
/// is shown. Styled in `style.css` - a few pulses, then a quiet tint.
const ATTENTION_CLASS: &str = "needs-attention";



/// Set on the row a reorder-drag is hovering, to draw the line the dragged
/// project would land on - above it or below it.
const DROP_ABOVE_CLASS: &str = "drop-above";
const DROP_BELOW_CLASS: &str = "drop-below";

/// How much each font-size keybinding press changes the UI's text scale - a
/// multiplier applied both to every pane's VTE `font-scale` and, via a dynamic
/// `window { font-size: {scale}em; }` CSS rule, to every chrome element sized in
/// `em` - so the whole program's text and the app's own controls grow together
/// instead of only the terminal contents.
///
/// The step is a *ratio*, not an addend: each press multiplies (or divides) the
/// scale by it. A flat additive step feels lumpy because a fixed 0.1 is a fifth
/// of the way up from 0.5 but a thirtieth of the way up from 2.9 - same key, a
/// huge jump when small and a barely-visible one when large. Multiplying keeps
/// every press the same *percentage* change, so the steps read as even the whole
/// way across the range.
///
/// The ratio is a twentieth, not a tenth. Even steps still jump if each one is
/// big, and a tenth of a ~11pt terminal font is over a point a press - enough
/// that VTE reflows the character grid into a visibly different shape each time.
const FONT_SCALE_STEP: f64 = 1.05;
const FONT_SCALE_MIN: f64 = 0.5;
const FONT_SCALE_MAX: f64 = 3.0;




/// Below this width the sidebar stops taking space from the panes and overlays
/// them instead. Four panes in a grid beside a 16.5em rack is already tight;
/// much under this and the rack is winning an argument it shouldn't be in.
const COLLAPSE_WIDTH_PX: i32 = 700;


/// The widgets belonging to one project. Parallel to `model::Project` rather
/// than inside it, because these are GTK objects and `model` is deliberately
/// GTK-free - which is what lets the ordering rules be tested without a display.
struct ProjectView {
    id: ProjectId,
    tiler: Tiler,
    row: gtk4::ListBoxRow,
    /// How many agents this project is running, shown on its sidebar row.
    count: gtk4::Label,
    /// Switches between the tiler and the empty state. A project with no panes
    /// used to be impossible (the app opened straight into a help pane), and
    /// now it's the *first* thing a new user sees - so it has to say what to do
    /// rather than show a blank rectangle.
    view: gtk4::Stack,
}

struct Inner {
    window: adw::ApplicationWindow,
    store: RefCell<ProjectStore>,
    views: RefCell<Vec<ProjectView>>,
    stack: gtk4::Stack,
    list: gtk4::ListBox,
    split: adw::OverlaySplitView,
    toasts: adw::ToastOverlay,
    updates: Updates,
    title: adw::WindowTitle,
    /// The three layout-mode toggles, in `MODE_BUTTONS` order.
    mode_buttons: RefCell<Vec<gtk4::ToggleButton>>,
    /// True while the mode toggles are being repainted *from* the tiler, so the
    /// `toggled` handlers know not to write the mode straight back and start a
    /// loop. Setting a `ToggleButton` active fires `toggled` exactly as a click
    /// does, and nothing in the signal says which it was.
    syncing_mode: Cell<bool>,
    /// Carries the attention signal out to where it can be seen when the sidebar
    /// is closed: it says "one of your projects wants you", and the row behind
    /// it says which.
    sidebar_toggle: gtk4::ToggleButton,
    /// The last folder a "new project" pick landed on, so the picker reopens
    /// pre-filled with it.
    last_dir: RefCell<String>,
    /// How many agents to start a newly-opened project with: however many the
    /// project you were last working in was running.
    ///
    /// Learned rather than asked (see `App::open_project`). It follows the
    /// *visible* project's pane count, so it tracks the size of workspace you
    /// actually keep rather than the largest you once opened, and it ignores
    /// zero - "I closed everything" is not a preference for starting nothing.
    /// Phase 3's config file is where this stops being per-run memory.
    last_agent_count: Cell<usize>,
    font_scale: Cell<f64>,
    /// "AgentTileCLI", plus a branch marker on builds that aren't off master.
    base_title: String,
    /// Holds just the one dynamic `window { font-size: ... }` rule that drives
    /// chrome scaling - reloaded in place on every scale change rather than
    /// recreated, so it keeps sitting at the priority it was added with.
    css_provider: gtk4::CssProvider,
}

/// The application window and everything in it.
#[derive(Clone)]
pub struct App(Rc<Inner>);

impl App {
    /// Builds the window and its first project, and returns without presenting
    /// it - `main` presents once keybindings are installed.
    ///
    /// The first project is named for what it holds rather than where it is, and
    /// deliberately gets *no* agent spawned into it: starting a claude in
    /// whatever directory the app happened to be launched from is a surprise,
    /// and the empty state is where the app explains itself instead.
    pub fn new(application: &adw::Application, cwd: &str, title: &str) -> Self {
        let stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::None)
            .hexpand(true)
            .vexpand(true)
            .build();

        let toasts = adw::ToastOverlay::new();
        // The bounds match the grip's, so the split view never clamps a drag
        // back to somewhere the grip was willing to go - two clamps disagreeing
        // reads as a rack that refuses to move for no stated reason.
        let split = adw::OverlaySplitView::builder()
            .show_sidebar(false)
            .min_sidebar_width(sidebar::SIDEBAR_MIN_PX)
            .max_sidebar_width(sidebar::SIDEBAR_MAX_PX)
            .build();
        toasts.set_child(Some(&split));

        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title(title)
            // A clean 16:10 aspect ratio (1488 / 930 = 1.6 exactly). This is
            // just the starting size - the app never resizes itself afterward
            // (adding panes tiles them smaller within whatever size the window
            // already is instead), so the user's own resize is the last word.
            .default_width(1488)
            .default_height(930)
            .content(&toasts)
            .build();

        let updates = Updates::new(&toasts);
        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::Single)
            .css_classes(["sidebar-list"])
            .build();
        let title_widget = adw::WindowTitle::new(title, "");
        let sidebar_toggle = gtk4::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .css_classes(["sidebar-toggle"])
            .can_focus(false)
            .tooltip_text("Toggle the project sidebar (Super+Alt+g)")
            .build();

        let css_provider = gtk4::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &css_provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }

        let this = App(Rc::new(Inner {
            window,
            store: RefCell::new(ProjectStore::new()),
            views: RefCell::new(Vec::new()),
            stack: stack.clone(),
            list: list.clone(),
            split: split.clone(),
            toasts,
            updates: updates.clone(),
            title: title_widget.clone(),
            mode_buttons: RefCell::new(Vec::new()),
            syncing_mode: Cell::new(false),
            sidebar_toggle: sidebar_toggle.clone(),
            last_dir: RefCell::new(cwd.to_string()),
            last_agent_count: Cell::new(1),
            font_scale: Cell::new(1.0),
            base_title: title.to_string(),
            css_provider,
        }));

        split.set_sidebar(Some(&this.build_sidebar()));
        split.set_content(Some(&this.build_content(&title_widget, &sidebar_toggle)));
        this.install_breakpoint();
        this.wire_signals();

        this.add_project(cwd, "Getting Started".to_string(), "help-about-symbolic");
        if std::env::var_os("ATC_SCREENSHOT").is_some() {
            this.0.split.set_show_sidebar(true);
            let tiler = this.add_project(cwd, "agenttilecli".to_string(), "folder-symbolic");
            for _ in 0..3 {
                tiler.spawn_command_pane(cwd, "sleep 600", |_| {});
            }
            this.add_project("/usr/share", "Offline_map".to_string(), "folder-symbolic");
            this.select_for_screenshot();
        }
        this
    }

    #[doc(hidden)]
    fn select_for_screenshot(&self) {
        let id = self.0.views.borrow()[1].id;
        self.select(id);
    }

    pub fn present(&self) {
        self.0.window.present();
    }

    pub fn window(&self) -> &adw::ApplicationWindow {
        &self.0.window
    }

    // ── Construction ─────────────────────────────────────────────────────





    /// Below `COLLAPSE_WIDTH_PX` the sidebar overlays the panes rather than
    /// taking width from them.
    fn install_breakpoint(&self) {
        let condition = adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            f64::from(COLLAPSE_WIDTH_PX),
            adw::LengthUnit::Px,
        );
        let breakpoint = adw::Breakpoint::new(condition);
        breakpoint.add_setter(&self.0.split, "collapsed", Some(&true.to_value()));
        self.0.window.add_breakpoint(breakpoint);
    }

    fn wire_signals(&self) {
        // `store` is the one place the project order lives, and this is what
        // makes the sidebar show it: rows sort by their project's position in
        // that vector, so a reorder is a reorder *of the store* and the list
        // follows. The alternative - removing and reinserting row widgets -
        // would have to keep two orders in step by hand, and the store's is the
        // one that already decides what "next" means.
        let this = self.clone();
        self.0.list.set_sort_func(move |a, b| {
            let store = this.0.store.borrow();
            let position = |row: &gtk4::ListBoxRow| {
                ProjectId::from_widget_name(&row.widget_name())
                    .and_then(|id| store.position(id))
                    // A row whose project isn't in the store sorts last rather
                    // than panicking; the only moment that happens is a row
                    // mid-removal.
                    .unwrap_or(usize::MAX)
            };
            position(a).cmp(&position(b)).into()
        });

        let this = self.clone();
        self.0.list.connect_row_selected(move |_, row| {
            let Some(id) = row.and_then(|r| ProjectId::from_widget_name(&r.widget_name())) else {
                return;
            };
            this.show_project(id);
        });

        let this = self.clone();
        self.0.updates.set_install_callback(move |command| {
            let Some(tiler) = this.active_tiler() else {
                return;
            };
            let after = this.clone();
            tiler.spawn_command_pane(update::repo_dir(), &command, move |succeeded| {
                // Only a clean exit means the new binary is actually on disk. A
                // failed update leaves its pane open with the reason, and leaves
                // the running app alone.
                if succeeded {
                    after.restart();
                }
            });
        });

        self.install_window_actions();
    }


    // ── Projects ─────────────────────────────────────────────────────────










    // ── Attention ────────────────────────────────────────────────────────



    // ── Lookups ──────────────────────────────────────────────────────────




    // ── Public actions, driven by the keybindings ────────────────────────

    pub fn toggle_sidebar(&self) {
        let shown = self.0.split.shows_sidebar();
        self.0.split.set_show_sidebar(!shown);
    }



    pub fn check_for_updates(&self) {
        self.0.updates.check();
    }

    pub fn inc_font_scale(&self) {
        let scale = (self.0.font_scale.get() * FONT_SCALE_STEP).min(FONT_SCALE_MAX);
        self.set_font_scale(scale);
    }

    pub fn dec_font_scale(&self) {
        let scale = (self.0.font_scale.get() / FONT_SCALE_STEP).max(FONT_SCALE_MIN);
        self.set_font_scale(scale);
    }

    pub fn reset_font_scale(&self) {
        self.set_font_scale(1.0);
    }

    /// Applies `scale` to every project's panes and to the chrome (sidebar,
    /// header bar, pane borders and labels) via the dynamic CSS provider.
    ///
    /// Global rather than per-project, so switching projects never shows a
    /// different zoom level.
    fn set_font_scale(&self, scale: f64) {
        self.0.font_scale.set(scale);
        for view in self.0.views.borrow().iter() {
            view.tiler.set_font_scale(scale);
        }
        self.0
            .css_provider
            .load_from_string(&format!("window {{ font-size: {scale}em; }}"));
    }

    // ── Header bar ───────────────────────────────────────────────────────




    // ── Menu items ───────────────────────────────────────────────────────



    /// Relaunches AgentTileCLI, which is how an update finishes: the new binary
    /// is on disk, but this process is still the old one, and only a fresh exec
    /// runs the new code.
    ///
    /// The binary is the one this process was launched from, remembered back at
    /// startup - whatever file that was is the file `install.sh` has just
    /// overwritten, so it's the one to run again. Asking for it *now* would get
    /// the wrong answer; `update::remember_exe` explains why.
    fn restart(&self) {
        let relaunch = update::exe()
            .and_then(|exe| update::spawn_relaunch(&update::relaunch_command(std::process::id(), &exe)));

        match relaunch {
            // Quitting is what actually hands over: the watcher is sitting on
            // this pid, and starts the new build the moment it's gone.
            Ok(()) => {
                if let Some(app) = self.0.window.application() {
                    app.quit();
                }
            }
            // The update installed but we can't bring the app back up - so say
            // so, and (pointedly) don't quit. A shutdown the user has to undo by
            // hand is a poor outcome; a silent one they don't see coming is
            // worse.
            Err(reason) => self.0.updates.alert(
                "Update installed, but couldn't restart",
                &format!("Quit and relaunch AgentTileCLI to run the new version.\n\n{reason}"),
            ),
        }
    }
}

fn set_class(widget: &impl IsA<gtk4::Widget>, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}
