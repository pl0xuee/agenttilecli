use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};

use crate::pane::folder_name;
use crate::tiler::Tiler;
use crate::update;

/// The update button's resting label - restored after a check finishes,
/// having been swapped for a "Checking..." one while it ran.
const UPDATE_LABEL: &str = "Check for updates";

/// Set on the sidebar row of a background group whose agent has asked for the
/// user (see `Groups::flash_row`); cleared the moment that group is shown.
/// Styled in `style.css` - a few pulses, then a quiet tint that persists.
const ATTENTION_CLASS: &str = "needs-attention";

/// Set on the row a reorder-drag is hovering, to draw the line the dragged
/// project would land on - above it or below it. Cleared when the pointer
/// leaves the row, and when the drop happens.
const DROP_ABOVE_CLASS: &str = "drop-above";
const DROP_BELOW_CLASS: &str = "drop-below";

/// How much each font-size keybinding press changes the UI's text scale - a
/// multiplier applied both to every pane's VTE `font-scale` and, via a
/// dynamic `window { font-size: {scale}em; }` CSS rule, to every chrome
/// element sized in `em` (sidebar, floating buttons, pane borders/labels) -
/// so the whole program's text and the app's own controls grow together
/// instead of only the terminal contents.
const FONT_SCALE_STEP: f64 = 0.1;
const FONT_SCALE_MIN: f64 = 0.5;
const FONT_SCALE_MAX: f64 = 3.0;

/// What a finished check says about whether an update exists - or `None` when
/// it says nothing at all, because the check couldn't be *made*.
///
/// That third case is the whole reason this isn't a bool: a check that failed
/// (no network, GitHub down) hasn't discovered that a previously-found update
/// went away. Clearing the button on it would throw away a true answer and
/// replace it with no answer, so `Failed` leaves the last one standing.
fn update_available(status: &update::Status) -> Option<bool> {
    match status {
        update::Status::UpToDate => Some(false),
        update::Status::Available(_) => Some(true),
        update::Status::Failed(_) => None,
    }
}

/// Whether a drop at `y` within a row `height` tall means "below this row"
/// rather than "above" it. Splitting the row at its midpoint is what lets a
/// list of n rows offer all n+1 insertion points: without it the bottom slot
/// would be unreachable, since there's no row *after* the last one to aim at.
fn drops_below(y: f64, height: f64) -> bool {
    y > height / 2.0
}

/// Where the group currently at index `from` ends up when it's dropped on the
/// row at index `target` - `below` picking which side of that row it lands on.
///
/// The subtraction is the part worth stating: the dragged group is lifted out
/// of the list before it's put back, so every insertion point after the hole it
/// leaves behind has already shifted down by one by the time it's reinserted.
/// Without this, dragging a row downward always lands it one row short of where
/// it was dropped.
fn drop_index(from: usize, target: usize, below: bool) -> usize {
    let insert_at = if below { target + 1 } else { target };
    if from < insert_at {
        insert_at - 1
    } else {
        insert_at
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
/// Both matter: a class left behind here is a line drawn under a drag that
/// ended somewhere else entirely.
fn clear_drop_classes(row: &gtk4::ListBoxRow) {
    row.remove_css_class(DROP_ABOVE_CLASS);
    row.remove_css_class(DROP_BELOW_CLASS);
}

/// Starts the shell that relaunches us once we're gone - and, crucially, starts
/// it somewhere it will survive our own death.
///
/// Spawning it as a plain child looks like it should work (orphans are
/// reparented to init, so nothing is left to kill it) and doesn't, because a
/// desktop session doesn't launch a `.desktop` app as a bare process any more:
/// it launches it as a systemd user service, `app-<app-id>@<random>.service`.
/// Every child we fork lands in that unit's cgroup, and when our main process
/// exits systemd stops the unit and SIGTERMs whatever is left in it. The
/// watcher - and then the app it has just exec'd - is exactly what's left in it.
/// The user gets the shutdown and never gets the restart.
///
/// So the watcher has to be started by something that isn't us. `systemd-run
/// --user` asks the *user manager* to run it, in a transient unit of its own,
/// out of reach of ours as ours goes down. `--collect` has that unit tidied away
/// once the relaunched app eventually exits, rather than left behind.
///
/// The plain child stays as the fallback, for anywhere `systemd-run` isn't (a
/// session that isn't systemd-managed, a build run straight from a terminal):
/// there's no unit to be torn down there, so orphan-to-init is the whole
/// mechanism rather than a hope.
fn spawn_relaunch(command: &str) -> Result<(), String> {
    let unit = format!("agenttilecli-restart-{}", std::process::id());
    let handed_off = std::process::Command::new("systemd-run")
        .args(["--user", "--quiet", "--collect"])
        .arg(format!("--unit={unit}"))
        // The app keeps the directory it was launched from (`main` seeds the
        // first pane's cwd with it); a transient unit would otherwise start in
        // $HOME.
        .arg("--same-dir")
        .args(["--", "sh", "-c", command])
        .status();

    if matches!(&handed_off, Ok(status) if status.success()) {
        return Ok(());
    }

    std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("couldn't start the relaunch: {e}"))
}

#[derive(Clone)]
struct GroupEntry {
    /// The `Stack` page name for this group's `Tiler`, and the
    /// `widget-name` of its sidebar row - the thread linking the two so a
    /// row click or a stack switch can look up the other side.
    id: String,
    tiler: Tiler,
    row: gtk4::ListBoxRow,
}

struct GroupsInner {
    root: gtk4::Box,
    stack: gtk4::Stack,
    sidebar_list: gtk4::ListBox,
    revealer: gtk4::Revealer,
    entries: RefCell<Vec<GroupEntry>>,
    next_id: Cell<u32>,
    /// The last folder a "new group" pick landed on, so the picker reopens
    /// pre-filled with it - same courtesy `Tiler::spawn_pane` used to offer
    /// for panes before groups existed.
    last_dir: RefCell<String>,
    /// Shared with every group's own `Tiler::set_title_callback` closure
    /// (each of which only forwards through it while its group is the
    /// visible one) - set once via `Groups::set_title_callback`.
    window_title_cb: Rc<RefCell<Option<Box<dyn Fn(&str)>>>>,
    /// The global text-size multiplier - shared across every group so
    /// switching groups never shows a different zoom level, and applied to
    /// the chrome via `css_provider` alongside every pane's VTE terminal.
    font_scale: Cell<f64>,
    /// Holds just the one dynamic `window { font-size: ... }` rule that
    /// drives chrome scaling (see `FONT_SCALE_STEP`'s doc comment) -
    /// reloaded in place on every scale change rather than recreated, so it
    /// keeps sitting at the priority it was added to the display with.
    css_provider: gtk4::CssProvider,
    /// The sidebar's "check for updates" button - kept here so a check in
    /// flight can desensitize it (one at a time), and so a check that finds
    /// something can leave it highlighted afterward.
    update_button: gtk4::Button,
    /// That button's caption, which doubles as the check's only progress
    /// indicator ("Checking...") and its only lasting result ("Update
    /// available") - so it's swapped, not static.
    update_label: gtk4::Label,
    /// What the last *conclusive* check found - the single source of truth
    /// behind `refresh_update_button`, so the button's caption and its green
    /// highlight are always painted from the same fact and can't drift apart.
    update_available: Cell<bool>,
    /// Whether an update check is already running, so a second one can't be
    /// started on top of it (the keybinding doesn't go through the button,
    /// so the button's own sensitivity can't be what enforces this).
    update_in_flight: Cell<bool>,
    /// The sidebar-toggle button, kept here because it doubles as the
    /// notification of last resort: the sidebar starts *collapsed*, so a row
    /// lighting up behind it is a notification nobody can see. The hamburger
    /// is what's on screen at that moment, so it carries the same signal out
    /// to where it can be - see `flash_row`.
    hamburger: gtk4::Button,
}

/// A hamburger-toggled sidebar of project groups, each holding its own
/// `Tiler` (and therefore its own independent set of agent panes, layout
/// mode, and focus). Exactly one group's `Tiler` is visible at a time
/// (backed by a `Stack`); the others keep running in the background -
/// closing/hiding a group's widget doesn't touch its panes' processes.
#[derive(Clone)]
pub struct Groups(Rc<GroupsInner>);

impl Groups {
    /// Builds the sidebar/stack scaffold and creates the first group from
    /// `initial_cwd` (the app's own launch directory). Does *not* toggle the
    /// help pane on it - callers that want that (as `main.rs` does, for
    /// parity with the pre-groups startup sequence) call
    /// `active_tiler().unwrap().toggle_help()` themselves, after wiring up
    /// `set_title_callback` so the resulting title-change actually lands
    /// somewhere.
    pub fn new(initial_cwd: &str) -> Self {
        let stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::None)
            .hexpand(true)
            .vexpand(true)
            .build();

        let sidebar_list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::Single)
            .css_classes(["sidebar-list"])
            .build();

        let header_label = gtk4::Label::builder()
            .label("Projects")
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .css_classes(["sidebar-header-label"])
            .build();
        let new_group_button = gtk4::Button::builder()
            .icon_name("list-add-symbolic")
            .css_classes(["flat", "circular"])
            .can_focus(false)
            .tooltip_text("Open a new project as a new group")
            .build();
        let header = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .css_classes(["sidebar-header"])
            .build();
        header.append(&header_label);
        header.append(&new_group_button);

        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&sidebar_list)
            .build();

        // The update control lives here, in the sidebar's footer, rather than
        // floating over the panes: checking GitHub for a new release is a
        // rare, app-level housekeeping action, and a permanent button in the
        // corner of the workspace gives it far more prominence than it earns.
        // Tucked behind the hamburger it's still one click away, and it sits
        // with the other app-level control (the sidebar's "+") instead of
        // competing with the per-project new-agent button.
        //
        // `view-refresh` rather than the more on-the-nose
        // `software-update-available`: the latter isn't in Breeze (KDE's
        // default, and so a very common GTK icon theme on the desktops this
        // app is built for), where it renders as a broken-image glyph. This
        // one is in both Breeze and Adwaita.
        let update_icon = gtk4::Image::from_icon_name("view-refresh-symbolic");
        update_icon.add_css_class("sidebar-footer-icon");
        // Deliberately not `hexpand`: a hexpanding child here would propagate
        // computed-hexpand up through the sidebar and make the Revealer claim
        // width against the Stack even while collapsed (see the Revealer's own
        // `hexpand(false)`). The icon and label pack from the start of the row
        // anyway, so there's nothing to gain by it.
        let update_label = gtk4::Label::builder()
            .label(UPDATE_LABEL)
            .halign(gtk4::Align::Start)
            .build();
        let update_content = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(10)
            .build();
        update_content.append(&update_icon);
        update_content.append(&update_label);

        let update_button = gtk4::Button::builder()
            .css_classes(["flat", "sidebar-footer-button"])
            .can_focus(false)
            .child(&update_content)
            .tooltip_text("Check GitHub for a newer version (Super+Alt+u)")
            .build();
        // What the update button is talking about, kept where it can be read
        // without pressing anything. Dim and small: it's the answer to a
        // question ("which build am I actually running?") that only gets asked
        // occasionally, usually right before or right after clicking the
        // button above it - so it belongs next to that button, not competing
        // with it. `halign` rather than `hexpand`, for the same reason the
        // update label avoids hexpand (see above).
        let version_label = gtk4::Label::builder()
            .label(format!("AgentTileCLI {}", update::version()))
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .selectable(true)
            .css_classes(["sidebar-version"])
            .tooltip_text("The version and commit this build was made from")
            .build();

        let footer = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .css_classes(["sidebar-footer"])
            .build();
        footer.append(&update_button);
        footer.append(&version_label);

        let sidebar_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .css_classes(["sidebar"])
            .build();
        sidebar_box.append(&header);
        // `scrolled` is the only vexpand child, so the footer is pushed to
        // the bottom of the sidebar however few groups the list holds.
        sidebar_box.append(&scrolled);
        sidebar_box.append(&footer);

        // `hexpand` explicitly pinned `false`: without it, the row label's
        // own `hexpand(true)` below (needed to push each row's close button
        // to the right edge) propagates all the way up through the
        // otherwise-hexpand-silent ancestors between it and here, making
        // the Revealer itself computed-hexpand and so claim equal leftover
        // width against the Stack - even while collapsed.
        let revealer = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideRight)
            .transition_duration(180)
            .reveal_child(false)
            .hexpand(false)
            .child(&sidebar_box)
            .build();

        // The sidebar-toggle and new-agent buttons float over the *content*
        // overlay (just the stack), not the whole root - overlaying them on
        // the root instead would pin them to the window's own top-left/
        // bottom-right corners, so opening the sidebar would slide it out
        // right underneath the hamburger button and cover its own header.
        // Anchoring to the stack's overlay instead means they shift right
        // along with the stack as the revealer claims space.
        let hamburger_button = gtk4::Button::builder()
            .icon_name("open-menu-symbolic")
            .css_classes(["circular", "add-pane", "floating-top-left"])
            .can_focus(false)
            .halign(gtk4::Align::Start)
            .valign(gtk4::Align::Start)
            .tooltip_text("Toggle the project sidebar (Super+Alt+g)")
            .build();
        let new_agent_button = gtk4::Button::builder()
            .icon_name("tab-new-symbolic")
            .css_classes(["circular", "add-pane", "floating-bottom-right"])
            .can_focus(false)
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::End)
            .tooltip_text("Spawn a new agent in the current project")
            .build();

        let content = gtk4::Overlay::new();
        content.set_child(Some(&stack));
        content.add_overlay(&new_agent_button);
        content.add_overlay(&hamburger_button);

        let root = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .build();
        root.append(&revealer);
        root.append(&content);

        let css_provider = gtk4::CssProvider::new();
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &css_provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }

        let this = Groups(Rc::new(GroupsInner {
            root,
            stack,
            sidebar_list,
            revealer,
            entries: RefCell::new(Vec::new()),
            next_id: Cell::new(0),
            last_dir: RefCell::new(initial_cwd.to_string()),
            window_title_cb: Rc::new(RefCell::new(None)),
            font_scale: Cell::new(1.0),
            css_provider,
            update_button: update_button.clone(),
            update_label,
            update_available: Cell::new(false),
            update_in_flight: Cell::new(false),
            hamburger: hamburger_button.clone(),
        }));

        // `entries` is the one place the project order lives, and this is what
        // makes the sidebar show it: the rows sort by their group's position in
        // that vec, so a reorder is a reorder *of the vec* (see `move_group_to`)
        // and the list follows. The alternative - removing and reinserting the
        // row widget - would have to keep two orders in step by hand, and the
        // vec's is the one that already decides what `cycle_group` calls "next".
        //
        // A row whose group isn't in `entries` sorts last rather than panicking;
        // the only moment that can happen is a row mid-removal.
        let this_weak = Rc::downgrade(&this.0);
        this.0.sidebar_list.set_sort_func(move |a, b| {
            let Some(inner) = this_weak.upgrade() else {
                return gtk4::Ordering::Equal;
            };
            let entries = inner.entries.borrow();
            let position = |row: &gtk4::ListBoxRow| {
                entries
                    .iter()
                    .position(|e| e.id == row.widget_name())
                    .unwrap_or(usize::MAX)
            };
            position(a).cmp(&position(b)).into()
        });

        // A row click (or arrow-key navigation within the sidebar) switches
        // the stack; a stack switch (from here, from `cycle_group`, or from
        // the initial `add_group`) re-focuses and re-titles the newly
        // visible group.
        let this_weak = Rc::downgrade(&this.0);
        this.0.sidebar_list.connect_row_selected(move |_, row| {
            let (Some(this), Some(row)) = (this_weak.upgrade(), row) else {
                return;
            };
            let name = row.widget_name();
            if !name.is_empty() {
                this.stack.set_visible_child_name(&name);
            }
        });

        // Showing a group is also what answers its call for attention: the
        // user has now seen whatever the agent rang about, so the row stops
        // saying so. This is the single choke point for that - every way of
        // switching groups (row click, `cycle_group`, a group being added or
        // removed) goes through the stack.
        let this_weak = Rc::downgrade(&this.0);
        this.0.stack.connect_visible_child_notify(move |stack| {
            if let Some(tiler) = stack
                .visible_child()
                .and_then(|w| w.downcast::<Tiler>().ok())
            {
                tiler.on_shown();
            }
            if let (Some(inner), Some(id)) = (this_weak.upgrade(), stack.visible_child_name()) {
                Groups(inner).clear_attention(&id);
            }
        });

        let this_clone = this.clone();
        new_group_button.connect_clicked(move |_| this_clone.new_group());

        let this_clone = this.clone();
        hamburger_button.connect_clicked(move |_| this_clone.toggle_sidebar());

        let this_clone = this.clone();
        new_agent_button.connect_clicked(move |_| {
            if let Some(tiler) = this_clone.active_tiler() {
                tiler.spawn_pane_here();
            }
        });

        let this_clone = this.clone();
        update_button.connect_clicked(move |_| this_clone.check_for_updates());

        // The initial group is the one that opens straight to the help pane
        // (see `add_group_named`'s doc comment) - labeled and iconed to
        // match, rather than after whatever folder the app happened to
        // launch from.
        //
        // `help-about` rather than the more obvious `dialog-information`, for
        // the same reason the update button avoids `software-update-available`
        // (see above): Breeze ships no `dialog-information-symbolic`, so GTK
        // falls back to the full-colour `dialog-information`, which ignores
        // `.sidebar-row-icon`'s colour and leaves one loud blue icon sitting
        // among the monochrome ones. This one is symbolic in both themes.
        this.add_group_named(
            initial_cwd,
            "Getting Started".to_string(),
            "help-about-symbolic",
        );
        this
    }

    /// The widget to embed in the rest of the UI.
    pub fn widget(&self) -> &gtk4::Box {
        &self.0.root
    }

    pub fn toggle_sidebar(&self) {
        let revealed = self.0.revealer.reveals_child();
        self.0.revealer.set_reveal_child(!revealed);
    }

    fn row_for(&self, id: &str) -> Option<gtk4::ListBoxRow> {
        self.0
            .entries
            .borrow()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.row.clone())
    }

    /// Flags group `id` as wanting the user: its sidebar row pulses a few
    /// times and then stays quietly tinted until the group is shown. Driven by
    /// `Tiler::set_attention_callback` - an agent that rang the bell (it
    /// finished a turn, or it's stopped to ask something) or one whose process
    /// exited.
    ///
    /// A group the user is already looking at gets nothing. The agent that
    /// rang is on screen in front of them; a sidebar row lighting up to report
    /// what they can already see is just noise, and noise is what makes people
    /// stop reading a notification.
    fn flash_row(&self, id: &str) {
        if self.0.stack.visible_child_name().as_deref() == Some(id) {
            return;
        }
        let Some(row) = self.row_for(id) else {
            return;
        };

        // The hamburger flashes alongside the row, because most of the time the
        // row isn't on screen to flash: the sidebar starts collapsed, and a
        // notification behind a collapsed sidebar notifies nobody. The
        // hamburger is the one thing that's always visible, so it says "one of
        // your projects wants you" and the row behind it says which.
        let hamburger = self.0.hamburger.clone();

        // A CSS animation restarts only when the class is *newly* added, so
        // re-adding one the widget already carries would pulse nothing - which
        // is exactly the case that matters, a second agent finishing while the
        // first is still waiting. Dropping the class and restoring it once GTK
        // has had a frame to notice it gone replays the pulses from the top.
        row.remove_css_class(ATTENTION_CLASS);
        hamburger.remove_css_class(ATTENTION_CLASS);
        glib::idle_add_local_once(move || {
            row.add_css_class(ATTENTION_CLASS);
            hamburger.add_css_class(ATTENTION_CLASS);
        });
    }

    /// Answers group `id`'s call for attention - it's been looked at.
    fn clear_attention(&self, id: &str) {
        if let Some(row) = self.row_for(id) {
            row.remove_css_class(ATTENTION_CLASS);
        }
        self.refresh_hamburger();
    }

    /// The hamburger speaks for every group at once, so it goes quiet only once
    /// the *last* group still asking for attention has been seen - or closed.
    fn refresh_hamburger(&self) {
        let still_waiting = self
            .0
            .entries
            .borrow()
            .iter()
            .any(|e| e.row.has_css_class(ATTENTION_CLASS));
        if !still_waiting {
            self.0.hamburger.remove_css_class(ATTENTION_CLASS);
        }
    }

    /// The `Tiler` for whichever group is currently visible - `None` only
    /// transiently, if ever, since a group always exists once `new` returns.
    pub fn active_tiler(&self) -> Option<Tiler> {
        self.0
            .stack
            .visible_child()
            .and_then(|w| w.downcast::<Tiler>().ok())
    }

    /// Registers the callback invoked with the combined "group name [·
    /// pane-title]" string whenever the *visible* group's title-worthy state
    /// changes (focus move, foreground process title change, or a group
    /// switch) - mirrors `Tiler::set_title_callback`, just scoped across
    /// groups instead of panes.
    pub fn set_title_callback(&self, f: impl Fn(&str) + 'static) {
        *self.0.window_title_cb.borrow_mut() = Some(Box::new(f));
    }

    /// Applies `scale` to every group's panes and to the chrome (sidebar,
    /// floating buttons, pane borders/labels) via the dynamic CSS provider.
    fn set_font_scale(&self, scale: f64) {
        self.0.font_scale.set(scale);
        for entry in self.0.entries.borrow().iter() {
            entry.tiler.set_font_scale(scale);
        }
        self.0
            .css_provider
            .load_from_string(&format!("window {{ font-size: {scale}em; }}"));
    }

    pub fn inc_font_scale(&self) {
        let scale = (self.0.font_scale.get() + FONT_SCALE_STEP).min(FONT_SCALE_MAX);
        self.set_font_scale(scale);
    }

    pub fn dec_font_scale(&self) {
        let scale = (self.0.font_scale.get() - FONT_SCALE_STEP).max(FONT_SCALE_MIN);
        self.set_font_scale(scale);
    }

    pub fn reset_font_scale(&self) {
        self.set_font_scale(1.0);
    }

    /// Checks `origin/master` for a newer release and reports back: you're
    /// up to date, here's what's new (with an offer to install it), or here's
    /// why the check couldn't be made.
    ///
    /// The git work runs on Gio's blocking-IO pool rather than the main loop:
    /// it fetches over the network, and a UI frozen for however long GitHub
    /// takes to answer - or for however long it takes to *not* answer, on a
    /// flaky connection - isn't something to inflict on someone who clicked
    /// a button out of idle curiosity.
    pub fn check_for_updates(&self) {
        // The button desensitizes itself below, but the keybinding path
        // doesn't go through the button at all - so the guard, not the
        // button, is what actually stops two overlapping checks.
        if self.0.update_in_flight.get() {
            return;
        }
        self.0.update_in_flight.set(true);
        self.0.update_button.set_sensitive(false);
        self.0.update_label.set_label("Checking\u{2026}");

        let this = self.clone();
        glib::spawn_future_local(async move {
            let status = gio::spawn_blocking(update::check).await;

            this.0.update_in_flight.set(false);
            this.0.update_button.set_sensitive(true);

            match status {
                Ok(status) => this.show_update_status(status),
                // `check` has no panicking path, but a button stuck on
                // "Checking..." forever is the one outcome worse than a
                // dialog saying so.
                Err(_) => {
                    this.refresh_update_button();
                    this.alert(
                        "Couldn't check for updates",
                        "The update check crashed unexpectedly.",
                    );
                }
            }
        });
    }

    /// Repaints the button from `update_available` - the one place its caption
    /// and its green highlight are set, so the two can never disagree (a green
    /// button captioned "Check for updates" says nothing to anyone).
    ///
    /// Also what clears the "Checking..." caption once a check finishes,
    /// whatever it found.
    fn refresh_update_button(&self) {
        if self.0.update_available.get() {
            // Left saying so, in green, after the dialog closes - so "not now"
            // doesn't also mean "and never mention it again". It's the only
            // trace an available update leaves once the dialog is gone.
            self.0.update_button.add_css_class("update-available");
            self.0.update_label.set_label("Update available");
        } else {
            self.0.update_button.remove_css_class("update-available");
            self.0.update_label.set_label(UPDATE_LABEL);
        }
    }

    fn show_update_status(&self, status: update::Status) {
        if let Some(available) = update_available(&status) {
            self.0.update_available.set(available);
        }
        self.refresh_update_button();

        match status {
            update::Status::UpToDate => self.alert(
                "You're up to date",
                &format!("AgentTileCLI {}", update::version()),
            ),
            update::Status::Failed(reason) => self.alert("Couldn't check for updates", &reason),
            update::Status::Available(update) => self.offer_update(update),
        }
    }

    /// The "here's what's new" dialog. Installing runs the pull and rebuild
    /// in a *pane* rather than behind a spinner: it's a `cargo build` of a
    /// GTK app, it takes a while, and watching it is both more reassuring
    /// and more useful than a frozen dialog if it goes wrong.
    fn offer_update(&self, update: update::Update) {
        let repo = update::repo_dir();
        let plural = if update.commits == 1 {
            "commit"
        } else {
            "commits"
        };
        let mut detail = format!(
            "origin/master is {} {plural} ahead of this build ({}):\n",
            update.commits,
            update::version(),
        );
        for subject in &update.subjects {
            detail.push_str(&format!("\n  \u{2022} {subject}"));
        }
        if update.commits > update.subjects.len() {
            let rest = update.commits - update.subjects.len();
            detail.push_str(&format!("\n  \u{2022} \u{2026}and {rest} more"));
        }

        if let Some(reason) = &update.blocked {
            detail.push_str(&format!(
                "\n\nThis build's checkout can't be updated for you, because {reason}:\n\n\
                 {repo}\n\n\
                 Sort that out and update it by hand - nothing there has been touched.",
            ));
            self.alert("Update available", &detail);
            return;
        }

        // The warning about the agents is the point of this paragraph. Updating
        // ends in a restart, and a restart hangs up every agent in every group
        // - which is a fine trade if you know it's coming, and an unpleasant
        // surprise if you don't. This dialog is the last moment it can be said,
        // so it gets said plainly, next to the button that does it.
        detail.push_str(&format!(
            "\n\nUpdating fast-forwards {repo} to origin/master and re-runs ./install.sh, \
             in a new pane so you can watch it. AgentTileCLI then restarts itself into the \
             new version - which closes every agent you have running, in every group.\n\n\
             If the update fails, nothing is restarted and the pane stays open with the \
             reason.",
        ));

        let dialog = gtk4::AlertDialog::builder()
            .message("Update available")
            .detail(&detail)
            .buttons(["Not now", "Update"])
            .cancel_button(0)
            .default_button(1)
            .build();

        let this = self.clone();
        dialog.choose(
            self.window().as_ref(),
            None::<&gio::Cancellable>,
            move |result| {
                // Anything but the Update button (including Escape and a
                // dismissed dialog, which report `Err`) leaves the checkout
                // exactly as it is.
                if !matches!(result, Ok(1)) {
                    return;
                }
                match update::command() {
                    Ok(command) => {
                        if let Some(tiler) = this.active_tiler() {
                            let this = this.clone();
                            tiler.spawn_command_pane(
                                update::repo_dir(),
                                &command,
                                // Only a clean exit means the new binary is
                                // actually on disk (see `update::command`). A
                                // failed update leaves its pane open with the
                                // reason, and leaves the running app alone.
                                move |succeeded| {
                                    if succeeded {
                                        this.restart();
                                    }
                                },
                            );
                        }
                    }
                    Err(reason) => this.alert("Couldn't start the update", &reason),
                }
            },
        );
    }

    /// Relaunches AgentTileCLI, which is how an update finishes: the new binary
    /// is on disk, but this process is still the old one, and only a fresh
    /// exec runs the new code.
    ///
    /// The relaunch can't simply be spawned and left to race us - it has to
    /// wait for this process to be gone first (see `update::relaunch_command`),
    /// and it has to outlive it, which is the hard half: see `spawn_relaunch`.
    ///
    /// The binary is the one this process was launched from, remembered back at
    /// startup - whatever file that was is the file `install.sh` has just
    /// overwritten, so it's the one to run again. Asking for it *now* would get
    /// the wrong answer; `update::remember_exe` explains why.
    fn restart(&self) {
        let exe = match update::exe() {
            Ok(exe) => exe,
            Err(e) => return self.cant_restart(&e),
        };

        if let Err(e) = spawn_relaunch(&update::relaunch_command(std::process::id(), &exe)) {
            return self.cant_restart(&e);
        }

        // Quitting is what actually hands over: the watcher above is sitting on
        // this pid, and starts the new build the moment it's gone.
        if let Some(app) = self.window().and_then(|w| w.application()) {
            app.quit();
        }
    }

    /// The update installed but we can't bring the app back up - so say so, and
    /// (pointedly) don't quit. A shutdown the user has to undo by hand is a poor
    /// outcome, but a silent one they don't see coming is a worse one.
    fn cant_restart(&self, reason: &str) {
        self.alert(
            "Update installed, but couldn't restart",
            &format!("Quit and relaunch AgentTileCLI to run the new version.\n\n{reason}"),
        );
    }

    /// A one-button informational dialog, parented on the app window.
    fn alert(&self, message: &str, detail: &str) {
        gtk4::AlertDialog::builder()
            .message(message)
            .detail(detail)
            .buttons(["OK"])
            .cancel_button(0)
            .default_button(0)
            .build()
            .show(self.window().as_ref());
    }

    /// The window this sidebar is in, to parent modal dialogs on - `None`
    /// only before the widget tree has been rooted in one.
    fn window(&self) -> Option<gtk4::Window> {
        self.0
            .root
            .root()
            .and_then(|r| r.downcast::<gtk4::Window>().ok())
    }

    /// Asks (via a folder picker) which project to open, then how many
    /// agents to start it with, then creates a new group for it and
    /// switches to it. The folder picker opens pre-filled with the last
    /// directory used (or the app's own launch directory, the very first
    /// time). Cancelling either dialog creates nothing.
    pub fn new_group(&self) {
        let last_dir = self.0.last_dir.borrow().clone();

        let dialog = gtk4::FileDialog::builder()
            .title("Open project as a new group")
            .accept_label("Open")
            .modal(true)
            .initial_folder(&gio::File::for_path(&last_dir))
            .build();

        let this = self.clone();
        let parent = self.window();
        let parent_for_count = parent.clone();
        dialog.select_folder(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
            let Some(dir) = result.ok().and_then(|file| file.path()) else {
                return;
            };
            let dir = dir.to_string_lossy().into_owned();
            this.0.last_dir.replace(dir.clone());

            // Buttons rather than a spinner/entry - every other dialog in
            // this app (including the folder picker just above) is a click
            // or two, never typed input, and 1-4 covers every layout mode
            // (grid, master-stack, monocle) without the picker itself
            // needing scroll or validation. More agents can always be added
            // afterward with `new-agent`.
            //
            // Index 0 is Cancel (and is registered as `cancel_button`, so
            // Escape / closing the dialog reports it too) - so button index
            // *is* the agent count for every non-cancel choice.
            let count_dialog = gtk4::AlertDialog::builder()
                .message("How many agents?")
                .detail(folder_name(&dir))
                .buttons(["Cancel", "1", "2", "3", "4"])
                .cancel_button(0)
                .default_button(1)
                .build();

            let this = this.clone();
            let parent_for_count = parent_for_count.clone();
            count_dialog.choose(
                parent_for_count.as_ref(),
                None::<&gio::Cancellable>,
                move |result| {
                    // Cancelling (button 0, Escape, or the dialog being
                    // dismissed - which reports `Err`) creates nothing at
                    // all, rather than falling back to a group nobody asked
                    // for: backing out here should leave the app exactly as
                    // it was before the folder picker opened.
                    let Ok(count @ 1..=4) = result else {
                        return;
                    };
                    let tiler = this.add_group(&dir);
                    for _ in 0..count {
                        tiler.spawn_pane_here();
                    }
                },
            );
        });
    }

    /// Registers a new group for `cwd`, named after its folder (a fresh
    /// `Tiler`, a stack page, and a sidebar row) and switches to it - see
    /// `add_group_named` for the details `new_group` and `Groups::new` both
    /// build on top of.
    fn add_group(&self, cwd: &str) -> Tiler {
        self.add_group_named(cwd, folder_name(cwd), "folder-symbolic")
    }

    /// Registers a new group for `cwd` under an explicit sidebar label and
    /// icon (a fresh `Tiler`, a stack page, and a sidebar row) and switches
    /// to it, returning the new `Tiler` so the caller can decide whether to
    /// spawn a pane into it - `new_group` always does, but `Groups::new`'s
    /// initial group deliberately doesn't, so startup still shows only the
    /// help pane rather than surprising the user with an agent already
    /// running in whatever directory the app happened to launch from.
    fn add_group_named(&self, cwd: &str, name: String, icon: &str) -> Tiler {
        let id = self.0.next_id.get().to_string();
        self.0.next_id.set(self.0.next_id.get() + 1);

        let tiler = Tiler::new(cwd.to_string());
        tiler.set_font_scale(self.0.font_scale.get());
        self.0.stack.add_named(&tiler, Some(&id));

        let stack_weak = self.0.stack.downgrade();
        let title_cb = self.0.window_title_cb.clone();
        let id_for_cb = id.clone();
        let name_for_cb = name.clone();
        tiler.set_title_callback(move |pane_title| {
            let Some(stack) = stack_weak.upgrade() else {
                return;
            };
            if stack.visible_child_name().as_deref() != Some(id_for_cb.as_str()) {
                return;
            }
            let combined = if pane_title.is_empty() {
                name_for_cb.clone()
            } else {
                format!("{name_for_cb} \u{b7} {pane_title}")
            };
            if let Some(cb) = title_cb.borrow().as_ref() {
                cb(&combined);
            }
        });

        // Weak, like the title callback above: the `Tiler` this closure is
        // being hung on is itself owned (via `entries`) by the `GroupsInner`
        // it would otherwise hold a strong reference back to.
        let inner_weak = Rc::downgrade(&self.0);
        let id_for_attention = id.clone();
        tiler.set_attention_callback(move || {
            if let Some(inner) = inner_weak.upgrade() {
                Groups(inner).flash_row(&id_for_attention);
            }
        });

        let row_icon = gtk4::Image::builder()
            .icon_name(icon)
            .css_classes(["sidebar-row-icon"])
            .build();
        let row_label = gtk4::Label::builder()
            .label(&name)
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["sidebar-row-label"])
            .build();
        let close_button = gtk4::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "circular", "sidebar-row-close"])
            .can_focus(false)
            .tooltip_text("Close this project group")
            .build();
        let row_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .build();
        row_box.append(&row_icon);
        row_box.append(&row_label);
        row_box.append(&close_button);

        let row = gtk4::ListBoxRow::builder().child(&row_box).build();
        row.set_widget_name(&id);
        row.add_css_class("sidebar-row");
        row.set_tooltip_text(Some(&format!(
            "{name}\nDrag to reorder (or Super+Alt+Shift+[ / ])"
        )));

        let this = self.clone();
        let id_for_close = id.clone();
        close_button.connect_clicked(move |_| this.remove_group(&id_for_close));

        self.install_reorder(&row, &id);

        // Registered in `entries` *before* the row is appended, because
        // appending runs the ListBox's sort func (see `new`), which asks
        // `entries` where this group goes - and a group it can't find there
        // sorts to the very end, which is only coincidentally right.
        self.0.entries.borrow_mut().push(GroupEntry {
            id: id.clone(),
            tiler: tiler.clone(),
            row: row.clone(),
        });
        self.0.sidebar_list.append(&row);

        self.0.stack.set_visible_child_name(&id);
        self.0.sidebar_list.select_row(Some(&row));

        tiler
    }

    /// Makes `row` draggable onto its neighbours, so the sidebar's project
    /// order is the user's rather than the order they happened to open things
    /// in.
    ///
    /// The drag carries the group's id as a plain string. That means the row
    /// will also *accept* any old text dragged in from outside the app - a
    /// selection from a terminal pane, say - so the drop handler treats a
    /// string that names no group as "not mine" and declines it, rather than
    /// trusting the payload because it arrived on the right widget.
    fn install_reorder(&self, row: &gtk4::ListBoxRow, id: &str) {
        let drag = gtk4::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .build();
        let dragged_id = id.to_string();
        drag.connect_prepare(move |_, _, _| {
            Some(gdk::ContentProvider::for_value(&dragged_id.to_value()))
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
                let below = drops_below(y, f64::from(row.height()));
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
        let target_id = id.to_string();
        let row_weak = row.downgrade();
        drop.connect_drop(move |_, value, _, y| {
            let Some(row) = row_weak.upgrade() else {
                return false;
            };
            clear_drop_classes(&row);
            let Ok(source_id) = value.get::<String>() else {
                return false;
            };
            this.reorder_onto(
                &source_id,
                &target_id,
                drops_below(y, f64::from(row.height())),
            )
        });
        row.add_controller(drop);
    }

    /// Answers a drop of group `source_id` onto the row of `target_id`.
    /// `false` means the drop wasn't ours to take - the payload named no group
    /// of this app's (see `install_reorder`).
    fn reorder_onto(&self, source_id: &str, target_id: &str, below: bool) -> bool {
        let to = {
            let entries = self.0.entries.borrow();
            let find = |id: &str| entries.iter().position(|e| e.id == id);
            let (Some(from), Some(target)) = (find(source_id), find(target_id)) else {
                return false;
            };
            drop_index(from, target, below)
        };
        self.move_group_to(source_id, to);
        true
    }

    /// Moves group `id` to index `to` in the project order, clamped to the
    /// list. The sidebar is repainted from `entries` rather than reshuffled
    /// widget by widget - see the sort func in `new`.
    fn move_group_to(&self, id: &str, to: usize) {
        {
            let mut entries = self.0.entries.borrow_mut();
            let Some(from) = entries.iter().position(|e| e.id == id) else {
                return;
            };
            let to = to.min(entries.len().saturating_sub(1));
            if from == to {
                return;
            }
            let entry = entries.remove(from);
            entries.insert(to, entry);
        }
        // Outside the borrow above: sorting calls back into `entries`, and a
        // sort kicked off while it was still mutably borrowed would panic.
        self.0.sidebar_list.invalidate_sort();
    }

    /// Moves the *visible* group one place up (`delta = -1`) or down
    /// (`delta = 1`) - the keyboard's way in to what a drag does with the
    /// mouse. A no-op with a single group.
    ///
    /// Clamped rather than wrapped, unlike `cycle_group`: moving focus off the
    /// end of the list and round to the top costs nothing, but a project that
    /// silently teleports from the bottom of the list to the top because a key
    /// was pressed once too often is a reorder the user now has to undo.
    pub fn move_active_group(&self, delta: i32) {
        let (id, to) = {
            let entries = self.0.entries.borrow();
            let len = entries.len();
            if len < 2 {
                return;
            }
            let current = self.0.stack.visible_child_name();
            let Some(from) = entries
                .iter()
                .position(|e| Some(e.id.as_str()) == current.as_deref())
            else {
                return;
            };
            let to = (from as i32 + delta).clamp(0, len as i32 - 1) as usize;
            (entries[from].id.clone(), to)
        };
        self.move_group_to(&id, to);
    }

    /// Closes every pane in the group `id` and removes it from both the
    /// stack and the sidebar. Refuses to remove the last remaining group -
    /// there's always at least one project open. If the removed group was
    /// the visible one, falls back to a neighboring group.
    fn remove_group(&self, id: &str) {
        let removed_was_active = self.0.stack.visible_child_name().as_deref() == Some(id);
        let (removed, fallback) = {
            let mut entries = self.0.entries.borrow_mut();
            if entries.len() <= 1 {
                return;
            }
            let Some(pos) = entries.iter().position(|e| e.id == id) else {
                return;
            };
            let removed = entries.remove(pos);
            let fallback = entries[pos.min(entries.len() - 1)].clone();
            (removed, fallback)
        };

        removed.tiler.close_all_panes();
        self.0.stack.remove(&removed.tiler);
        self.0.sidebar_list.remove(&removed.row);
        // The closed group might have been the only one still asking for
        // attention, and it can't answer for itself now that it's gone -
        // leaving the hamburger lit for a project that no longer exists.
        self.refresh_hamburger();

        if removed_was_active {
            self.0.stack.set_visible_child_name(&fallback.id);
            self.0.sidebar_list.select_row(Some(&fallback.row));
        }
    }

    /// Switches to the next (`delta = 1`) or previous (`delta = -1`) group,
    /// wrapping around. A no-op with a single group.
    pub fn cycle_group(&self, delta: i32) {
        let entries = self.0.entries.borrow();
        let len = entries.len();
        if len < 2 {
            return;
        }
        let current = self.0.stack.visible_child_name();
        let idx = entries
            .iter()
            .position(|e| Some(e.id.as_str()) == current.as_deref())
            .unwrap_or(0);
        let next = (idx as i32 + delta).rem_euclid(len as i32) as usize;
        let target = entries[next].clone();
        drop(entries);

        self.0.stack.set_visible_child_name(&target.id);
        self.0.sidebar_list.select_row(Some(&target.row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::gtk_test;

    /// The order of the groups, as `entries` has it - which is what decides
    /// what `cycle_group` calls "next".
    fn order(groups: &Groups) -> Vec<String> {
        groups
            .0
            .entries
            .borrow()
            .iter()
            .map(|e| e.id.clone())
            .collect()
    }

    /// The order of the groups as the *sidebar* shows them, read back off the
    /// ListBox - which is the thing the user actually reorders, and which only
    /// matches `order` above if the sort func in `new` is doing its job.
    fn sidebar_order(groups: &Groups) -> Vec<String> {
        let mut ids = Vec::new();
        let mut i = 0;
        while let Some(row) = groups.0.sidebar_list.row_at_index(i) {
            ids.push(row.widget_name().to_string());
            i += 1;
        }
        ids
    }

    /// A row is two drop targets, not one - its top half and its bottom half -
    /// and that's what gives a list of n rows the n+1 places a project can go.
    #[test]
    fn a_row_is_split_into_an_above_half_and_a_below_half() {
        assert!(!drops_below(4.0, 30.0), "the top of a row inserts above it");
        assert!(drops_below(26.0, 30.0), "the bottom of a row inserts below");
    }

    /// The off-by-one that makes a *downward* drag land where it was dropped
    /// rather than one row short of it: the dragged project is lifted out of
    /// the list before it's put back, so every slot below the hole it left has
    /// already shifted up by one.
    #[test]
    fn a_downward_drop_accounts_for_the_hole_the_drag_left_behind() {
        // The first project, dropped below the third: it lands third (index 2),
        // not fourth - the two it passed have each moved up one.
        assert_eq!(drop_index(0, 2, true), 2);
        assert_eq!(drop_index(0, 2, false), 1);

        // Dragging *up*, nothing below the hole has moved, so the line drawn is
        // exactly the index it lands on.
        assert_eq!(drop_index(3, 1, false), 1);
        assert_eq!(drop_index(3, 1, true), 2);

        // Dropped back onto itself, either half: it stays put.
        assert_eq!(drop_index(2, 2, false), 2);
        assert_eq!(drop_index(2, 2, true), 2);
    }

    /// A reorder has to move the project in *both* orders at once - the sidebar
    /// the user is looking at, and the `entries` order that decides which group
    /// `[`/`]` go to next. A drag that only repainted the list would leave the
    /// keyboard cycling projects in an order nothing on screen still shows.
    #[test]
    fn reordering_moves_a_group_for_the_sidebar_and_the_keyboard_alike() {
        gtk_test(|| {
            let groups = Groups::new("/tmp");
            groups.add_group("/usr");
            groups.add_group("/etc");
            assert_eq!(order(&groups), ["0", "1", "2"]);
            assert_eq!(sidebar_order(&groups), ["0", "1", "2"]);

            // Drag the last project onto the top half of the first: to the top.
            assert!(groups.reorder_onto("2", "0", false));
            assert_eq!(order(&groups), ["2", "0", "1"]);
            assert_eq!(
                sidebar_order(&groups),
                ["2", "0", "1"],
                "the sidebar must show the order the drag produced",
            );

            // And the keyboard now cycles in that order: after "2" comes "0".
            groups.0.stack.set_visible_child_name("2");
            groups.cycle_group(1);
            assert_eq!(groups.0.stack.visible_child_name().as_deref(), Some("0"));

            // Super+Alt+Shift+[ on the visible group ("0", now second) lifts it back
            // above "2"...
            groups.move_active_group(-1);
            assert_eq!(order(&groups), ["0", "2", "1"]);
            assert_eq!(sidebar_order(&groups), ["0", "2", "1"]);

            // ...and pressing it again at the top does nothing, rather than wrapping
            // the project round to the bottom of the list behind the user's back.
            groups.move_active_group(-1);
            assert_eq!(order(&groups), ["0", "2", "1"]);

            // A drag *into* the sidebar from outside the app (a text selection from
            // a terminal pane, say) arrives as a string that names no group. It must
            // be declined, not acted on.
            assert!(!groups.reorder_onto("/some/dragged/text", "0", true));
            assert_eq!(order(&groups), ["0", "2", "1"]);
        });
    }

    /// A moved project keeps its panes and its identity - the reorder shuffles
    /// rows, not the groups behind them. Getting this wrong (rebuilding rows in
    /// place of moving them) would silently hang up the agents in the group the
    /// user dragged.
    #[test]
    fn a_reordered_group_keeps_its_tiler() {
        gtk_test(|| {
            let groups = Groups::new("/tmp");
            groups.add_group("/usr");
            let moved = groups.0.entries.borrow()[1].tiler.clone();

            assert!(groups.reorder_onto("1", "0", false));
            assert_eq!(order(&groups), ["1", "0"]);
            assert_eq!(
                groups.0.entries.borrow()[0].tiler,
                moved,
                "the dragged group must arrive with the same Tiler it left with",
            );
        });
    }

    /// Exercises the group state machine directly (add/switch/cycle/remove),
    /// bypassing `new_group`'s `FileDialog` - GTK's own file chooser needs a
    /// desktop portal or a fully-fledged window manager to actually render,
    /// neither of which a test run can assume, and none of that machinery
    /// is what this test is meant to cover anyway.
    #[test]
    fn add_switch_remove_and_cycle_groups() {
        gtk_test(|| {
            let groups = Groups::new("/tmp");
            assert_eq!(groups.0.entries.borrow().len(), 1);
            assert_eq!(groups.0.stack.visible_child_name().as_deref(), Some("0"));
            // The initial group must not get an agent pane spawned into it
            // unasked - see `add_group`'s doc comment.
            assert!(groups.active_tiler().is_some());

            groups.add_group("/usr");
            groups.add_group("/etc");
            assert_eq!(groups.0.entries.borrow().len(), 3);
            // add_group switches to the group it just created.
            assert_eq!(groups.0.stack.visible_child_name().as_deref(), Some("2"));

            groups.cycle_group(1);
            assert_eq!(groups.0.stack.visible_child_name().as_deref(), Some("0"));
            groups.cycle_group(-1);
            assert_eq!(groups.0.stack.visible_child_name().as_deref(), Some("2"));

            // Removing the active group falls back to a neighbor.
            groups.remove_group("2");
            assert_eq!(groups.0.entries.borrow().len(), 2);
            assert_ne!(groups.0.stack.visible_child_name().as_deref(), Some("2"));

            // Never removes the last remaining group.
            groups.remove_group("1");
            assert_eq!(groups.0.entries.borrow().len(), 1);
            groups.remove_group("0");
            assert_eq!(groups.0.entries.borrow().len(), 1);
        });
    }

    /// Clicking the update button must hand the (network-bound) check off to
    /// another thread and lock out a second one - not run it on the main loop.
    ///
    /// The check itself is deliberately never allowed to *finish* here: this
    /// test doesn't iterate the main loop, so the future stays parked at its
    /// first await and no dialog is ever shown. What's under test is the
    /// wiring up to that point; `update`'s own tests cover what the check
    /// then decides.
    #[test]
    fn the_update_button_locks_out_a_second_check_while_one_is_running() {
        gtk_test(|| {
            let groups = Groups::new("/tmp");
            assert!(groups.0.update_button.is_sensitive());
            assert!(!groups.0.update_in_flight.get());

            groups.0.update_button.emit_clicked();
            assert!(
                groups.0.update_in_flight.get(),
                "check runs off the main loop"
            );
            assert!(!groups.0.update_button.is_sensitive());

            // The keybinding path bypasses the button, so it's the flag - not the
            // button's sensitivity - that has to hold the line here.
            groups.check_for_updates();
            assert!(groups.0.update_in_flight.get());
        });
    }

    /// A check that couldn't be *made* must not be read as "no update": it
    /// says nothing either way, and clearing the button on it would throw
    /// away a true answer the last check had already found.
    #[test]
    fn a_failed_check_says_nothing_about_whether_an_update_exists() {
        use update::{Status, Update};

        let available = || {
            Status::Available(Update {
                commits: 1,
                subjects: vec!["a shiny new feature".to_string()],
                blocked: None,
            })
        };

        assert_eq!(update_available(&available()), Some(true));
        assert_eq!(update_available(&Status::UpToDate), Some(false));
        assert_eq!(
            update_available(&Status::Failed("no network".to_string())),
            None,
            "a failed check leaves the last answer standing",
        );
    }

    /// The button's caption and its green highlight are painted from one fact,
    /// so they can't drift apart - a green button captioned "Check for
    /// updates" (or a plain one captioned "Update available") tells the user
    /// nothing.
    #[test]
    fn the_update_buttons_caption_and_highlight_always_agree() {
        gtk_test(|| {
            let groups = Groups::new("/tmp");
            let green = || groups.0.update_button.has_css_class("update-available");
            let caption = || groups.0.update_label.label().to_string();

            groups.refresh_update_button();
            assert!(!green());
            assert_eq!(caption(), UPDATE_LABEL);

            groups.0.update_available.set(true);
            groups.refresh_update_button();
            assert!(green());
            assert_eq!(caption(), "Update available");

            // The state a *failed* check leaves behind: the flag is untouched, so
            // repainting must restore the found update rather than clear it.
            groups.refresh_update_button();
            assert!(green());
            assert_eq!(caption(), "Update available");

            groups.0.update_available.set(false);
            groups.refresh_update_button();
            assert!(!green());
            assert_eq!(caption(), UPDATE_LABEL);
        });
    }
}
