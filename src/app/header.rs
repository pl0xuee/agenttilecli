//! The working half: the header bar that reports where you are, the layout
//! switch, the app menu, and what a project with nothing running shows.
//!
//! Split out of `app` alongside `sidebar`. This is the side that answers "what
//! is this project doing" - which mode its panes are in, which pane has the
//! keyboard, and what to press when there are no panes at all - while the rack
//! answers "which project".
//!
//! The header bar exists to say things the app previously knew and never told
//! anyone. Cycling layout modes changed the tiling and left the only evidence in
//! the shape of the panes, which is readable with four of them open and a guess
//! with one.

use adw::prelude::*;
use gtk4::gio;

use super::{App, Mode};
use crate::update;

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

/// Set on the app-menu button once a check has found a newer version, so the
/// news survives dismissing the dialog even though the menu itself is shut.
const UPDATE_CLASS: &str = "update-available";

/// The app menu's resting caption for the update item.
const UPDATE_LABEL: &str = "Check for Updates";

/// The action the update item is attached to, which is also how it is found.
const UPDATE_ACTION: &str = "win.updates";

/// Where the update item currently sits in the menu.
///
/// Found rather than written down. `gio::Menu` items are immutable, so
/// relabelling one means removing and reinserting it *by position* - and this
/// was a constant, with a comment warning that a wrong index silently rewrites
/// the wrong row. That is not a warning that survives contact with someone
/// adding a menu item above it, which is exactly what happened the first time
/// anyone did: the index went stale and "Check for Updates" started relabelling
/// the shortcuts row instead.
///
/// Asking the menu where the item is cannot go stale.
fn update_item_index(menu: &gio::Menu) -> Option<i32> {
    (0..menu.n_items()).find(|index| {
        menu.item_attribute_value(*index, "action", None)
            .and_then(|value| value.get::<String>())
            .is_some_and(|action| action == UPDATE_ACTION)
    })
}

impl App {
    /// The working half: a header bar that reports where you are and how the
    /// panes are arranged, above the stack of projects.
    pub(super) fn build_content(
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
        menu.append(Some("Commands\u{2026}"), Some("win.commands"));
        menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
        menu.append(Some("Preferences"), Some("win.preferences"));
        menu.append(Some(UPDATE_LABEL), Some(UPDATE_ACTION));
        menu.append(Some("About AgentTileCLI"), Some("win.about"));
        let menu_button = gtk4::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            // `flat` because this is the one control in the strip that is a
            // `MenuButton`, which renders as `menubutton > button` - so
            // libadwaita styles a node inside it that the app's own rules have
            // to reach explicitly (see `.header-action` in style.css). Flat
            // turns off the theme's raised treatment of that inner button, and
            // the stylesheet then paints it like every other header control.
            .css_classes(["app-menu", "header-action", "flat"])
            .valign(gtk4::Align::Center)
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
            // is to replace it in place - at wherever it currently is.
            if let Some(index) = update_item_index(&menu_for_state) {
                menu_for_state.remove(index);
                menu_for_state.insert(index, Some(label), Some(UPDATE_ACTION));
            }

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
            .valign(gtk4::Align::Center)
            .css_classes(["header-action"])
            .tooltip_text("Spawn a new agent in this project")
            .build();
        let this = self.clone();
        new_agent.connect_clicked(move |_| {
            if let Some(tiler) = this.active_tiler() {
                tiler.spawn_pane_here();
            }
        });

        // Broadcast is a `ToggleButton` rather than a plain one because it is a
        // *mode*, not an action, and a mode has to show whether it is on. Typing
        // one line into four agents is what it is for and also what makes it
        // dangerous: the armed state (see `.broadcast-active` in style.css) is
        // loud on purpose, so a broadcast left on is not something you discover
        // by watching the same keystroke land in four panes.
        let broadcast = gtk4::ToggleButton::builder()
            .icon_name("send-to-symbolic")
            .can_focus(false)
            .valign(gtk4::Align::Center)
            .css_classes(["broadcast-toggle", "header-action"])
            .tooltip_text("Broadcast typing to every agent in this project")
            .build();
        let this = self.clone();
        broadcast.connect_toggled(move |button| {
            if this.0.syncing_broadcast.get() {
                return;
            }
            if let Some(tiler) = this.active_tiler() {
                tiler.set_broadcast(button.is_active());
            }
            this.set_broadcast_armed(button.is_active());
        });
        *self.0.broadcast_button.borrow_mut() = Some(broadcast.clone());

        header.pack_end(&menu_button);
        header.pack_end(&new_agent);
        header.pack_end(&broadcast);

        // The scaled-content class is what the text-size keybinding targets: the
        // dynamic font-size rule lands here rather than on `window`, so scaling
        // grows the header and panes but leaves the sidebar at its native size
        // (the sidebar is the split view's other, separate subtree).
        //
        // It also carries the workspace floor - see `appearance::content_css`.
        // The floor is painted here rather than on the window because the window
        // spans the sidebar too, and a floor under the rack is a floor the
        // rack's own glass composites against.
        let view = adw::ToolbarView::builder()
            .content(&self.0.stack)
            .css_classes(["scaled-content"])
            // Flat, so the header bar stops being a slab with the workspace
            // below it and becomes part of the workspace. The default `Raised`
            // style gives the bar its own fill and a rule under it, which is a
            // second horizontal line immediately above a grid of tiles that are
            // already drawing their own - and with the floor now translucent, an
            // opaque bar across the top of it is the one place the glass would
            // visibly stop.
            .top_bar_style(adw::ToolbarStyle::Flat)
            .build();
        view.add_top_bar(&header);
        view
    }

    /// The three layout modes as one segmented control.
    ///
    /// This is the header bar earning its place. The mode was previously
    /// invisible: `Super+Alt+Tab` cycled grid to master-stack to monocle and the
    /// only evidence of which one you'd landed in was the shape of the panes -
    /// readable with four panes open, and a guess with one.
    fn build_mode_switcher(&self) -> gtk4::Box {
        // Centred rather than filling the bar's height, for the same reason the
        // round buttons beside it are: a header bar stretches what it is given,
        // and a control with a fixed shape has to say it doesn't want that.
        let row = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .valign(gtk4::Align::Center)
            .css_classes(["mode-switcher"])
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

    /// Points the mode toggles at `mode` without letting their `toggled` signal
    /// write it straight back - see `Inner::syncing_mode`.
    pub(super) fn sync_mode_buttons(&self, mode: Mode) {
        let Some(index) = MODE_BUTTONS.iter().position(|(m, _, _)| *m == mode) else {
            return;
        };
        self.0.syncing_mode.set(true);
        if let Some(button) = self.0.mode_buttons.borrow().get(index) {
            button.set_active(true);
        }
        self.0.syncing_mode.set(false);
    }

    /// Points the broadcast toggle at a group's state without letting it write
    /// that state back - the same dance `sync_mode_buttons` does.
    pub(super) fn sync_broadcast_button(&self, on: bool) {
        let Some(button) = self.0.broadcast_button.borrow().clone() else {
            return;
        };
        self.0.syncing_broadcast.set(true);
        button.set_active(on);
        self.0.syncing_broadcast.set(false);
        self.set_broadcast_armed(on);
    }

    /// Dims the mode toggles for a project with no panes.
    ///
    /// A layout mode is an answer to "how should these be arranged", and with
    /// nothing to arrange there is no answer for the control to give - it sits
    /// over an empty state reading "No agents running" and offers three ways to
    /// tile them anyway. The keybindings are left alone deliberately: they route
    /// through `Tiler::set_mode`, which is a no-op on an empty group, and taking
    /// a key away is a bigger claim than greying a button out.
    pub(super) fn sync_mode_sensitivity(&self, pane_count: usize) {
        let usable = pane_count > 0;
        for button in self.0.mode_buttons.borrow().iter() {
            button.set_sensitive(usable);
        }
    }

    /// What a project with nothing running shows.
    ///
    /// This is the first screen of the app, so it has exactly one job: say what
    /// to press. The previous answer was a help pane listing all twenty-one
    /// bindings at once, which is a reference card handed to someone who has not
    /// yet done the one thing that makes any of them matter. The full list is
    /// still a keystroke away, in the menu and on `Super+Alt+/`.
    pub(super) fn build_empty_state(&self) -> adw::StatusPage {
        // Not `suggested-action`. That class paints from `accent_bg_color`,
        // which this app aliases to @filament - so the stock treatment made
        // this button a solid warm pill and the brightest thing in the window,
        // in the one colour the stylesheet reserves for "the keyboard is here".
        // A primary action on an empty screen has no competition and needs
        // none: it is one of two things on an otherwise blank page.
        let start = gtk4::Button::builder()
            .label("Open a project\u{2026}")
            .halign(gtk4::Align::Center)
            .css_classes(["pill", "empty-primary"])
            .build();
        let this = self.clone();
        start.connect_clicked(move |_| this.new_project());

        let agent = gtk4::Button::builder()
            .label("Start an agent here")
            .halign(gtk4::Align::Center)
            .css_classes(["pill", "empty-secondary"])
            .tooltip_text("Run claude in this project's own folder")
            .build();
        let this = self.clone();
        agent.connect_clicked(move |_| {
            if let Some(tiler) = this.active_tiler() {
                tiler.spawn_pane_here();
            }
        });

        // The one line worth adding to an empty screen: where everything else
        // is. This is the only page in the app with nothing on it to read, so
        // it is the only place a pointer to the command palette costs nothing
        // and is certain to be seen.
        let hint = gtk4::Label::builder()
            .label("Press Super+Alt+P for everything else")
            .halign(gtk4::Align::Center)
            .css_classes(["empty-hint"])
            .build();

        let buttons = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(10)
            .halign(gtk4::Align::Center)
            .build();
        buttons.append(&start);
        buttons.append(&agent);
        buttons.append(&hint);

        // The floor, which this page has to paint for itself: it is the tiler's
        // sibling in the project's stack rather than its child, and the floor is
        // now painted per-region rather than by one fill behind everything (see
        // `appearance::content_css`). Without this the empty state is a window
        // with a desktop showing through it.
        adw::StatusPage::builder()
            .css_classes(["workspace-floor"])
            .icon_name("tab-new-symbolic")
            .title("No agents running")
            .description(
                "Open a project folder and choose how many agents to start it with. \
                 They tile themselves \u{2014} spawn, close or promote a pane and the rest re-arrange.",
            )
            .child(&buttons)
            .build()
    }

    pub(super) fn install_window_actions(&self) {
        let this = self.clone();
        let commands = gio::SimpleAction::new("commands", None);
        commands.connect_activate(move |_, _| this.show_command_palette());
        self.0.window.add_action(&commands);

        let this = self.clone();
        let preferences = gio::SimpleAction::new("preferences", None);
        preferences.connect_activate(move |_, _| this.show_preferences());
        self.0.window.add_action(&preferences);

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

    /// Says that the config file could not be used, and what was wrong with it.
    ///
    /// A dialog rather than a toast: it carries a parser's line-and-column
    /// complaint, which is several lines and worth reading twice, and it ends in
    /// something only the user can go and fix.
    pub fn report_config_problem(&self, problem: &str) {
        self.0
            .updates
            .alert("Your config file wasn't used", problem);
    }

    pub fn show_shortcuts(&self) {
        crate::shortcuts::present(&self.0.window);
    }

    pub fn show_about(&self) {
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
}
