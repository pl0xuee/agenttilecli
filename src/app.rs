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
use gtk4::{gdk, gio, glib};

use crate::layout::Mode;
use crate::model::{self, ProjectId, ProjectStore, Removal};
use crate::pane::folder_name;
use crate::tiler::Tiler;
use crate::update;
use crate::updates::Updates;

/// Set on the sidebar row of a background project whose agent has asked for the
/// user, and on the sidebar toggle alongside it. Cleared the moment that project
/// is shown. Styled in `style.css` - a few pulses, then a quiet tint.
const ATTENTION_CLASS: &str = "needs-attention";

/// Set on the app-menu button once a check has found a newer version, so the
/// news survives dismissing the dialog even though the menu itself is shut.
const UPDATE_CLASS: &str = "update-available";

/// The app menu's resting caption for the update item, and where that item sits.
/// The two are kept together because relabelling a `gio::Menu` item means
/// replacing it by position, and a wrong index silently rewrites the wrong row.
const UPDATE_LABEL: &str = "Check for Updates";
const UPDATE_MENU_INDEX: i32 = 1;

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

/// The layout modes the header bar offers, with the icon and tooltip for each.
/// `view-dual` and `view-fullscreen` are in both Breeze and Adwaita, which is
/// the constraint every icon choice in this app answers to (see `sidebar_row`).
const MODE_BUTTONS: [(Mode, &str, &str); 3] = [
    (Mode::Grid, "view-grid-symbolic", "Grid \u{b7} equal cells"),
    (
        Mode::MasterStack,
        "view-dual-symbolic",
        "Master-stack \u{b7} one large pane and a column",
    ),
    (
        Mode::Monocle,
        "view-fullscreen-symbolic",
        "Monocle \u{b7} the focused pane, fullscreen",
    ),
];

/// The widgets belonging to one project. Parallel to `model::Project` rather
/// than inside it, because these are GTK objects and `model` is deliberately
/// GTK-free - which is what lets the ordering rules be tested without a display.
struct ProjectView {
    id: ProjectId,
    tiler: Tiler,
    row: gtk4::ListBoxRow,
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
        let split = adw::OverlaySplitView::builder()
            .show_sidebar(false)
            .min_sidebar_width(220.0)
            .max_sidebar_width(320.0)
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

    /// The rack: a header, the project rows, and the version beneath them.
    fn build_sidebar(&self) -> adw::ToolbarView {
        let header_label = gtk4::Label::builder()
            .label("Projects")
            .css_classes(["sidebar-header-label"])
            .build();
        let new_project = gtk4::Button::builder()
            .icon_name("list-add-symbolic")
            .css_classes(["flat", "circular"])
            .can_focus(false)
            .tooltip_text("Open a new project as a new group (Super+Alt+Return)")
            .build();
        let this = self.clone();
        new_project.connect_clicked(move |_| this.new_project());

        let header = adw::HeaderBar::builder()
            .title_widget(&header_label)
            // The content side carries the window controls; two sets of them,
            // one either side of the split, reads as two windows. Both ends have
            // to be turned off to mean that: the start side is where the window
            // menu / app icon lands, and left on it put a dim copy of the app's
            // own icon in the sidebar's top-left corner - close enough to a
            // disabled button to look like one, and attached to nothing.
            .show_end_title_buttons(false)
            .show_start_title_buttons(false)
            .build();
        header.pack_end(&new_project);

        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&self.0.list)
            .build();

        // What the update button is talking about, kept where it can be read
        // without pressing anything: the answer to "which build am I actually
        // running?", a question only asked right before or right after clicking
        // the button above it.
        let version = gtk4::Label::builder()
            .label(format!("AgentTileCLI {}", update::version()))
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .selectable(true)
            .css_classes(["sidebar-version"])
            .tooltip_text("The version and commit this build was made from")
            .build();

        // Just the version now. The update button that used to sit above it
        // moved into the app menu, where it is one item rather than a second
        // copy of one - and where it is reachable without opening the sidebar,
        // which was always the odd part of keeping it here.
        let footer = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .css_classes(["sidebar-footer"])
            .build();
        footer.append(&version);

        let view = adw::ToolbarView::builder()
            .css_classes(["sidebar"])
            .content(&scrolled)
            .build();
        view.add_top_bar(&header);
        view.add_bottom_bar(&footer);
        view
    }

    /// The working half: a header bar that reports where you are and how the
    /// panes are arranged, above the stack of projects.
    fn build_content(
        &self,
        title: &adw::WindowTitle,
        sidebar_toggle: &gtk4::ToggleButton,
    ) -> adw::ToolbarView {
        // Bound both ways, so the button follows the sidebar however it was
        // opened - the keybinding, a breakpoint collapsing it, or the button.
        self.0
            .split
            .bind_property("show-sidebar", sidebar_toggle, "active")
            .bidirectional()
            .sync_create()
            .build();

        let header = adw::HeaderBar::builder()
            .title_widget(title)
            .show_start_title_buttons(false)
            .build();
        header.pack_start(sidebar_toggle);
        header.pack_start(&self.build_mode_switcher());

        let menu = gio::Menu::new();
        menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
        menu.append(Some(UPDATE_LABEL), Some("win.updates"));
        menu.append(Some("About AgentTileCLI"), Some("win.about"));
        let menu_button = gtk4::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .css_classes(["app-menu"])
            .can_focus(false)
            .menu_model(&menu)
            .tooltip_text("Main menu")
            .build();

        // The update control reports here now that it has no button of its own.
        // The menu is shut almost all the time, so the *item* carrying the news
        // isn't enough on its own - the button that opens the menu has to carry
        // it too, which is why both are painted from the one state.
        let menu_for_state = menu.clone();
        let button_for_state = menu_button.clone();
        self.0.updates.set_state_callback(move |state| {
            let label = if state.checking {
                "Checking for Updates\u{2026}"
            } else if state.available {
                "Update Available\u{2026}"
            } else {
                UPDATE_LABEL
            };
            // GMenu items are immutable once appended, so the way to relabel one
            // is to replace it in place. Position 1 is the update item.
            menu_for_state.remove(UPDATE_MENU_INDEX);
            menu_for_state.insert(UPDATE_MENU_INDEX, Some(label), Some("win.updates"));

            // Its own class, deliberately not `ATTENTION_CLASS`: that one means
            // "an agent wants you", and an available update is housekeeping. Two
            // different messages sharing one signal is how a signal stops
            // meaning anything.
            if state.available {
                button_for_state.add_css_class(UPDATE_CLASS);
            } else {
                button_for_state.remove_css_class(UPDATE_CLASS);
            }
        });

        let new_agent = gtk4::Button::builder()
            .icon_name("tab-new-symbolic")
            .can_focus(false)
            .tooltip_text("Spawn a new agent in this project")
            .build();
        let this = self.clone();
        new_agent.connect_clicked(move |_| {
            if let Some(tiler) = this.active_tiler() {
                tiler.spawn_pane_here();
            }
        });

        header.pack_end(&menu_button);
        header.pack_end(&new_agent);

        let view = adw::ToolbarView::builder().content(&self.0.stack).build();
        view.add_top_bar(&header);
        view
    }

    /// The three layout modes as one linked control.
    ///
    /// This is the header bar earning its place. The mode was previously
    /// invisible: `Super+Alt+Tab` cycled grid to master-stack to monocle and the
    /// only evidence of which one you'd landed in was the shape of the panes -
    /// readable with four panes open, and a guess with one.
    fn build_mode_switcher(&self) -> gtk4::Box {
        let row = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .css_classes(["linked", "mode-switcher"])
            .build();

        let mut buttons: Vec<gtk4::ToggleButton> = Vec::new();
        for (mode, icon, tooltip) in MODE_BUTTONS {
            let button = gtk4::ToggleButton::builder()
                .icon_name(icon)
                .can_focus(false)
                .tooltip_text(tooltip)
                .build();
            // Grouping makes these behave as radio buttons: exactly one active,
            // and clicking the active one doesn't turn the layout off.
            if let Some(first) = buttons.first() {
                button.set_group(Some(first));
            }
            let this = self.clone();
            button.connect_toggled(move |button| {
                if !button.is_active() || this.0.syncing_mode.get() {
                    return;
                }
                if let Some(tiler) = this.active_tiler() {
                    tiler.set_mode(mode);
                }
            });
            row.append(&button);
            buttons.push(button);
        }
        *self.0.mode_buttons.borrow_mut() = buttons;
        row
    }

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

    fn install_window_actions(&self) {
        let this = self.clone();
        let shortcuts = gio::SimpleAction::new("shortcuts", None);
        shortcuts.connect_activate(move |_, _| this.show_shortcuts());
        self.0.window.add_action(&shortcuts);

        let this = self.clone();
        let updates = gio::SimpleAction::new("updates", None);
        updates.connect_activate(move |_, _| this.check_for_updates());
        self.0.window.add_action(&updates);

        let this = self.clone();
        let about = gio::SimpleAction::new("about", None);
        about.connect_activate(move |_, _| this.show_about());
        self.0.window.add_action(&about);
    }

    // ── Projects ─────────────────────────────────────────────────────────

    /// Registers a project: a `Tiler`, a stack page and a sidebar row, switched
    /// to immediately.
    fn add_project(&self, path: &str, name: String, icon: &str) -> Tiler {
        let id = self.0.store.borrow_mut().add(path, name.clone(), icon);
        let page = id.as_name();

        let tiler = Tiler::new(path.to_string());
        tiler.set_font_scale(self.0.font_scale.get());

        let project_view = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .transition_duration(120)
            .build();
        project_view.add_named(&self.build_empty_state(), Some("empty"));
        project_view.add_named(&tiler, Some("panes"));
        self.0.stack.add_named(&project_view, Some(&page));

        let view_weak = project_view.downgrade();
        let weak = Rc::downgrade(&self.0);
        tiler.set_pane_count_callback(move |count| {
            if let Some(view) = view_weak.upgrade() {
                view.set_visible_child_name(if count == 0 { "empty" } else { "panes" });
            }
            // Only the visible project's count can speak for the header bar; a
            // background group closing its last pane must not dim the toggles
            // over the group you are actually looking at.
            if let Some(inner) = weak.upgrade() {
                let app = App(inner);
                if app.0.store.borrow().active() == Some(id) {
                    app.sync_mode_sensitivity(count);
                    // What the next project gets opened with - see
                    // `Inner::last_agent_count`.
                    if count > 0 {
                        app.0.last_agent_count.set(count);
                    }
                }
            }
        });

        // Weak, all three: the `Tiler` these are hung on is itself owned (via
        // `tilers`) by the `Inner` they would otherwise hold a strong reference
        // back to.
        let weak = Rc::downgrade(&self.0);
        let name_for_title = name.clone();
        tiler.set_title_callback(move |pane_title| {
            let Some(inner) = weak.upgrade() else { return };
            if inner.store.borrow().active() != Some(id) {
                return;
            }
            inner.title.set_title(&name_for_title);
            inner.title.set_subtitle(pane_title);
            // The header bar shows the project, because that's what you need
            // while working. The *window* title still leads with the app - it's
            // what the taskbar and the alt-tab switcher show, where "Getting
            // Started" on its own names nothing recognisable. It also carries
            // the branch marker for dev builds, which used to live in the WM
            // titlebar that client-side decorations have now replaced.
            let base = &inner.base_title;
            inner.window.set_title(Some(&if pane_title.is_empty() {
                format!("{base} \u{2014} {name_for_title}")
            } else {
                format!("{base} \u{2014} {name_for_title} \u{b7} {pane_title}")
            }));
        });

        let weak = Rc::downgrade(&self.0);
        tiler.set_attention_callback(move || {
            if let Some(inner) = weak.upgrade() {
                App(inner).flash_row(id);
            }
        });

        let weak = Rc::downgrade(&self.0);
        tiler.set_mode_callback(move |mode| {
            let Some(inner) = weak.upgrade() else { return };
            let app = App(inner);
            // Mirrored into the store so it survives a project switch, and
            // pushed at the header bar so the toggles show where the keyboard
            // just put us.
            if let Some(project) = app.0.store.borrow_mut().get_mut(id) {
                project.mode = mode;
            }
            if app.0.store.borrow().active() == Some(id) {
                app.sync_mode_buttons(mode);
            }
        });

        let weak = Rc::downgrade(&self.0);
        tiler.set_layout_callback(move |state| {
            let Some(inner) = weak.upgrade() else { return };
            // Nothing on screen reads these back yet - they're mirrored so the
            // model is a complete account of how a group is arranged rather
            // than a partial one, which is what phase 3 serialises. Mirroring
            // as it changes, rather than gathering it at save time, is what
            // keeps that true for a group that isn't the visible one.
            if let Some(project) = inner.store.borrow_mut().get_mut(id) {
                project.master_ratio = state.master_ratio;
                project.master_count = state.master_count;
                project.focus = state.focus;
            }
        });

        let row = self.build_row(id);
        self.0.views.borrow_mut().push(ProjectView {
            id,
            tiler: tiler.clone(),
            row: row.clone(),
            view: project_view,
        });
        self.0.list.append(&row);
        self.0.list.select_row(Some(&row));
        self.show_project(id);
        tiler
    }

    /// What a project with nothing running shows.
    ///
    /// This is the first screen of the app, so it has exactly one job: say what
    /// to press. The previous answer was a help pane listing all twenty-one
    /// bindings at once, which is a reference card handed to someone who has not
    /// yet done the one thing that makes any of them matter. The full list is
    /// still a keystroke away, in the menu and on `Super+Alt+/`.
    fn build_empty_state(&self) -> adw::StatusPage {
        let start = gtk4::Button::builder()
            .label("Open a project\u{2026}")
            .halign(gtk4::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        let this = self.clone();
        start.connect_clicked(move |_| this.new_project());

        let agent = gtk4::Button::builder()
            .label("Start an agent here")
            .halign(gtk4::Align::Center)
            .css_classes(["pill"])
            .tooltip_text("Run claude in this project's own folder")
            .build();
        let this = self.clone();
        agent.connect_clicked(move |_| {
            if let Some(tiler) = this.active_tiler() {
                tiler.spawn_pane_here();
            }
        });

        let buttons = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(10)
            .halign(gtk4::Align::Center)
            .build();
        buttons.append(&start);
        buttons.append(&agent);

        adw::StatusPage::builder()
            .icon_name("tab-new-symbolic")
            .title("No agents running")
            .description(
                "Open a project folder and choose how many agents to start it with. \
                 They tile themselves \u{2014} spawn, close or promote a pane and the rest re-arrange.",
            )
            .child(&buttons)
            .build()
    }

    /// Builds the sidebar row for a project, reading what it says off the model
    /// rather than off its caller.
    ///
    /// The arguments this used to take were the same strings that had just been
    /// handed to `ProjectStore::add`, which is one value with two owners and the
    /// shape of every drift bug this module was split up to prevent - a rename
    /// would have had to remember to touch both. The store is the only place a
    /// project's name, hue and icon are written, so it is the only place they
    /// are read.
    fn build_row(&self, id: ProjectId) -> gtk4::ListBoxRow {
        let store = self.0.store.borrow();
        let Some(project) = store.get(id) else {
            // Unreachable: the caller adds the project before building its row.
            // A blank row beats a panic in a UI callback either way.
            return gtk4::ListBoxRow::new();
        };
        let name = project.name.clone();
        let hue = project.hue.clone();
        let row_icon = gtk4::Image::builder()
            .icon_name(&project.icon)
            .css_classes(["sidebar-row-icon"])
            .build();
        drop(store);
        let label = gtk4::Label::builder()
            .label(&name)
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["sidebar-row-label"])
            .build();
        let close = gtk4::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "circular", "sidebar-row-close"])
            .can_focus(false)
            .tooltip_text("Close this project group")
            .build();
        let this = self.clone();
        close.connect_clicked(move |_| this.remove_project(id));

        let content = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .build();
        content.append(&row_icon);
        content.append(&label);
        content.append(&close);

        let row = gtk4::ListBoxRow::builder().child(&content).build();
        row.set_widget_name(&id.as_name());
        row.add_css_class("sidebar-row");
        row.add_css_class(&hue);
        row.set_tooltip_text(Some(&format!(
            "{name}\nDrag to reorder (or Super+Alt+Shift+[ / ])"
        )));
        self.install_reorder(&row, id);
        row
    }

    /// Makes `row` draggable onto its neighbours, so the sidebar's project order
    /// is the user's rather than the order they happened to open things in.
    ///
    /// The drag carries the project's id as a plain string, which means the row
    /// will also *accept* any old text dragged in from outside the app - a
    /// selection from a terminal pane, say. `ProjectId::from_widget_name`
    /// returning `None` is what declines those, rather than the payload being
    /// trusted because it arrived on the right widget.
    fn install_reorder(&self, row: &gtk4::ListBoxRow, id: ProjectId) {
        let drag = gtk4::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .build();
        let dragged = id.as_name();
        drag.connect_prepare(move |_, _, _| {
            Some(gdk::ContentProvider::for_value(&dragged.to_value()))
        });
        // Without an explicit icon the drag has no visible payload at all: the
        // row stays put and nothing follows the pointer, so the drag reads as
        // the app having ignored the gesture. A picture of the row itself is
        // both the obvious icon and a free one.
        let row_weak = row.downgrade();
        drag.connect_drag_begin(move |source, _| {
            if let Some(row) = row_weak.upgrade() {
                source.set_icon(Some(&gtk4::WidgetPaintable::new(Some(&row))), 0, 0);
            }
        });
        row.add_controller(drag);

        let drop = gtk4::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);

        // The insertion line, redrawn as the pointer crosses the row's midpoint:
        // a drop with no preview is a guess, and the guess is wrong half the
        // time by construction (each row is two targets, not one).
        let row_weak = row.downgrade();
        drop.connect_motion(move |_, _, y| {
            if let Some(row) = row_weak.upgrade() {
                let below = model::drops_below(y, f64::from(row.height()));
                set_class(&row, DROP_ABOVE_CLASS, !below);
                set_class(&row, DROP_BELOW_CLASS, below);
            }
            gdk::DragAction::MOVE
        });

        let row_weak = row.downgrade();
        drop.connect_leave(move |_| {
            if let Some(row) = row_weak.upgrade() {
                clear_drop_classes(&row);
            }
        });

        let this = self.clone();
        let row_weak = row.downgrade();
        drop.connect_drop(move |_, value, _, y| {
            let Some(row) = row_weak.upgrade() else {
                return false;
            };
            clear_drop_classes(&row);
            let Some(source) = value
                .get::<String>()
                .ok()
                .and_then(|s| ProjectId::from_widget_name(&s))
            else {
                return false;
            };
            let below = model::drops_below(y, f64::from(row.height()));
            let moved = this.0.store.borrow_mut().reorder_onto(source, id, below);
            if moved {
                // Outside the borrow above: sorting calls back into the store,
                // and a sort kicked off while it was still mutably borrowed
                // would panic.
                this.0.list.invalidate_sort();
            }
            moved
        });
        row.add_controller(drop);
    }

    /// Asks (via a folder picker) which project to open, then creates it,
    /// switches to it, and starts however many agents you last worked with.
    /// Cancelling the picker creates nothing at all, rather than falling back to
    /// a project nobody asked for.
    pub fn new_project(&self) {
        let dialog = gtk4::FileDialog::builder()
            .title("Open project as a new group")
            .accept_label("Open")
            .modal(true)
            .initial_folder(&gio::File::for_path(&*self.0.last_dir.borrow()))
            .build();

        let this = self.clone();
        let window = self.0.window.clone();
        dialog.select_folder(Some(&window), None::<&gio::Cancellable>, move |result| {
            let Some(dir) = result.ok().and_then(|file| file.path()) else {
                return;
            };
            let dir = dir.to_string_lossy().into_owned();
            this.0.last_dir.replace(dir.clone());
            this.open_project(dir);
        });
    }

    /// Opens `dir` as a new group and starts it with as many agents as the last
    /// project you worked in ended up running.
    ///
    /// This replaces a modal asking "how many agents?" with buttons for 1-4. The
    /// question was asked every single time a project was opened, and answered
    /// the same way almost every time - which is a dialog earning its place
    /// once and then costing a click forever after. The count you actually use
    /// is a habit, so it's remembered rather than re-asked, and a project that
    /// wants a different number is one spawn away (the + button, or
    /// `Super+Alt+Return`'s sibling `spawn_pane_here`).
    fn open_project(&self, dir: String) {
        let count = self.0.last_agent_count.get().max(1);
        let tiler = self.add_project(&dir, folder_name(&dir), "folder-symbolic");
        for _ in 0..count {
            tiler.spawn_pane_here();
        }
    }

    /// Closes every pane in a project and removes it from the stack and the
    /// sidebar. Refuses to remove the last one.
    fn remove_project(&self, id: ProjectId) {
        let outcome = self.0.store.borrow_mut().remove(id);
        let Removal::Removed { fallback } = outcome else {
            return;
        };

        let removed = {
            let mut views = self.0.views.borrow_mut();
            let Some(pos) = views.iter().position(|v| v.id == id) else {
                return;
            };
            views.remove(pos)
        };
        removed.tiler.close_all_panes();
        self.0.stack.remove(&removed.view);
        self.0.list.remove(&removed.row);
        // The closed project might have been the only one still asking for
        // attention, and it can't answer for itself now that it's gone - which
        // would leave the toggle lit for a project that no longer exists.
        self.refresh_attention();

        if let Some(fallback) = fallback {
            self.select(fallback);
        }
    }

    /// Makes a project the visible one, and answers its call for attention - the
    /// user has now seen whatever the agent rang about. This is the single choke
    /// point for that: every way of switching projects arrives here.
    fn show_project(&self, id: ProjectId) {
        self.0.store.borrow_mut().set_active(id);
        self.0.stack.set_visible_child_name(&id.as_name());

        if let Some(tiler) = self.tiler_for(id) {
            // Neither of these happens on its own while a `Tiler` sits hidden in
            // a background project.
            tiler.on_shown();
            self.sync_mode_buttons(tiler.mode());
            self.sync_mode_sensitivity(tiler.pane_count());
        }
        if let Some(row) = self.row_for(id) {
            row.remove_css_class(ATTENTION_CLASS);
        }
        self.refresh_attention();

        // On a narrow window the sidebar is covering the panes, so having picked
        // a project, get out of the way of it.
        if self.0.split.is_collapsed() {
            self.0.split.set_show_sidebar(false);
        }
    }

    /// Selects a row, which switches the stack through `connect_row_selected`.
    fn select(&self, id: ProjectId) {
        if let Some(row) = self.row_for(id) {
            self.0.list.select_row(Some(&row));
        }
    }

    // ── Attention ────────────────────────────────────────────────────────

    /// Flags a project as wanting the user: its sidebar row pulses a few times
    /// and then stays quietly tinted until the project is shown.
    ///
    /// A project the user is already looking at gets nothing. The agent that
    /// rang is on screen in front of them; a sidebar row lighting up to report
    /// what they can already see is just noise, and noise is what makes people
    /// stop reading notifications.
    fn flash_row(&self, id: ProjectId) {
        if self.0.store.borrow().active() == Some(id) {
            return;
        }
        let Some(row) = self.row_for(id) else { return };
        let toggle = self.0.sidebar_toggle.clone();

        // A CSS animation restarts only when the class is *newly* added, so
        // re-adding one the widget already carries would pulse nothing - which
        // is exactly the case that matters, a second agent finishing while the
        // first is still waiting. Dropping the class and restoring it once GTK
        // has had a frame to notice it gone replays the pulses from the top.
        row.remove_css_class(ATTENTION_CLASS);
        toggle.remove_css_class(ATTENTION_CLASS);
        glib::idle_add_local_once(move || {
            row.add_css_class(ATTENTION_CLASS);
            toggle.add_css_class(ATTENTION_CLASS);
        });
    }

    /// The sidebar toggle speaks for every project at once, so it goes quiet
    /// only once the *last* one still asking has been seen - or closed.
    fn refresh_attention(&self) {
        let still_waiting = self
            .0
            .views
            .borrow()
            .iter()
            .any(|v| v.row.has_css_class(ATTENTION_CLASS));
        if !still_waiting {
            self.0.sidebar_toggle.remove_css_class(ATTENTION_CLASS);
        }
    }

    // ── Lookups ──────────────────────────────────────────────────────────

    fn tiler_for(&self, id: ProjectId) -> Option<Tiler> {
        self.0
            .views
            .borrow()
            .iter()
            .find(|v| v.id == id)
            .map(|v| v.tiler.clone())
    }

    fn row_for(&self, id: ProjectId) -> Option<gtk4::ListBoxRow> {
        self.0
            .views
            .borrow()
            .iter()
            .find(|v| v.id == id)
            .map(|v| v.row.clone())
    }

    /// The `Tiler` for whichever project is currently visible.
    pub fn active_tiler(&self) -> Option<Tiler> {
        let id = self.0.store.borrow().active()?;
        self.tiler_for(id)
    }

    // ── Public actions, driven by the keybindings ────────────────────────

    pub fn toggle_sidebar(&self) {
        let shown = self.0.split.shows_sidebar();
        self.0.split.set_show_sidebar(!shown);
    }

    /// Switches to the next (`1`) or previous (`-1`) project, wrapping around.
    pub fn cycle_project(&self, delta: i32) {
        let next = self.0.store.borrow_mut().cycle(delta);
        if let Some(id) = next {
            self.select(id);
        }
    }

    /// Moves the visible project one place up (`-1`) or down (`1`) - the
    /// keyboard's way in to what a drag does with the mouse.
    pub fn move_active_project(&self, delta: i32) {
        self.0.store.borrow_mut().move_active(delta);
        self.0.list.invalidate_sort();
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

    /// Points the mode toggles at `mode` without letting their `toggled` signal
    /// write it straight back - see `Inner::syncing_mode`.
    fn sync_mode_buttons(&self, mode: Mode) {
        let Some(index) = MODE_BUTTONS.iter().position(|(m, _, _)| *m == mode) else {
            return;
        };
        self.0.syncing_mode.set(true);
        if let Some(button) = self.0.mode_buttons.borrow().get(index) {
            button.set_active(true);
        }
        self.0.syncing_mode.set(false);
    }

    /// Dims the mode toggles for a project with no panes.
    ///
    /// A layout mode is an answer to "how should these be arranged", and with
    /// nothing to arrange there is no answer for the control to give - it sits
    /// over an empty state reading "No agents running" and offers three ways to
    /// tile them anyway. The keybindings are left alone deliberately: they route
    /// through `Tiler::set_mode`, which is a no-op on an empty group, and taking
    /// a key away is a bigger claim than greying a button out.
    fn sync_mode_sensitivity(&self, pane_count: usize) {
        let usable = pane_count > 0;
        for button in self.0.mode_buttons.borrow().iter() {
            button.set_sensitive(usable);
        }
    }

    // ── Menu items ───────────────────────────────────────────────────────

    pub fn show_shortcuts(&self) {
        crate::shortcuts::present(&self.0.window);
    }

    fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("AgentTileCLI")
            .application_icon("agenttilecli")
            .version(update::version())
            .comments("A native Linux dynamic tiling window manager for AI CLI sessions.")
            .website("https://github.com/pl0xuee/agenttilecli")
            .license_type(gtk4::License::MitX11)
            .build();
        about.present(Some(&self.0.toasts));
    }

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

/// Takes the insertion line off a row - on leaving it, and on dropping onto it.
/// Both matter: a class left behind here is a line drawn under a drag that ended
/// somewhere else entirely.
fn clear_drop_classes(row: &gtk4::ListBoxRow) {
    row.remove_css_class(DROP_ABOVE_CLASS);
    row.remove_css_class(DROP_BELOW_CLASS);
}
