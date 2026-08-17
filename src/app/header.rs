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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4::gio;

use super::{App, Mode};
use crate::agent::Kind;
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

/// What the header bar says about where you are, and what is happening there.
///
/// This replaces an `AdwWindowTitle` centred in the bar, and the two changes
/// are separate arguments.
///
/// It is *left-aligned* because it is a location, not a caption. A centred
/// title is what a document window has - the name of the thing you are looking
/// at, floating over the middle of it - whereas this names which of seven
/// projects the workspace below is currently showing. Everything else that
/// answers that question in this app is down the left edge: the rail's lit
/// chip, the drawer's lit strip. The title being 700px away from all of them,
/// in the middle of the bar, was the header not participating in the sentence
/// the rest of the window was saying.
///
/// And it carries a *state dot*, which is the part that makes the bar do some
/// work rather than label things. The bar's own subtitle already knew how many
/// agents were running; what it could not say at a glance was whether any of
/// them had stopped and was waiting for you - the one fact in this application
/// worth interrupting someone for. The dot is the same one the pane heads and
/// the drawer rows wear, in the same three colours, so it needs no legend.
#[derive(Clone)]
pub(super) struct HeaderTitle {
    /// The whole block, for packing into the bar.
    widget: gtk4::Box,
    dot: gtk4::Box,
    name: gtk4::Label,
    subtitle: gtk4::Label,
    /// The subtitle as last set, kept because whether it is *shown* depends on
    /// something else that changes independently - see `set_compact`. Without
    /// this the two writers would have to be ordered, and the loser would blank
    /// a subtitle the winner had just set.
    text: Rc<RefCell<String>>,
    /// True on a window too narrow to carry both halves of the block.
    compact: Rc<Cell<bool>>,
}

impl HeaderTitle {
    pub(super) fn new(app_name: &str) -> Self {
        // Hidden until there is a state to report - a project with no agents
        // running has nothing to say here, and a permanently grey dot beside
        // every title is a light that means "the bulb works".
        let dot = gtk4::Box::builder()
            .css_classes(["pane-status"])
            .valign(gtk4::Align::Center)
            .visible(false)
            .build();

        let name = gtk4::Label::builder()
            .label(app_name)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["header-title-name"])
            .build();

        // Ellipsized, and it is the half that gives way: on a narrow window the
        // project's name is what you cannot afford to lose, and "3 agents · 1
        // waiting for you" is a sentence the dot beside it already summarises.
        let subtitle = gtk4::Label::builder()
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["header-title-sub"])
            .build();

        let widget = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(0)
            .valign(gtk4::Align::Center)
            .css_classes(["header-title"])
            .build();
        widget.append(&dot);
        widget.append(&name);
        widget.append(&subtitle);

        HeaderTitle {
            widget,
            dot,
            name,
            subtitle,
            text: Rc::new(RefCell::new(String::new())),
            compact: Rc::new(Cell::new(false)),
        }
    }

    pub(super) fn widget(&self) -> &gtk4::Box {
        &self.widget
    }

    pub(super) fn set_title(&self, title: &str) {
        self.name.set_label(title);
    }

    /// The line after the name. Prefixed with a separator here rather than by
    /// each caller, so the two labels can be packed flush and the gap belongs
    /// to whichever of them is actually present.
    pub(super) fn set_subtitle(&self, subtitle: &str) {
        *self.text.borrow_mut() = subtitle.to_string();
        self.render_subtitle();
    }

    /// Drops the subtitle on a window too narrow for both halves.
    ///
    /// Two ellipsized labels sharing a strip do not degrade gracefully - they
    /// degrade *equally*, so at 380px the bar read "a… · …": the project's name
    /// cut to its first letter so that a tally already summarised by the dot
    /// beside it could keep three dots of its own. One of the two has to yield
    /// outright, and it is not the name.
    ///
    /// Driven from the same breakpoint that sheds the mode switcher (see
    /// `install_breakpoint`), through its `apply`/`unapply` signals rather than
    /// a property setter, because what has to change here is a *decision* the
    /// widget re-makes whenever its text changes, not a property that can be
    /// set once and left.
    pub(super) fn set_compact(&self, compact: bool) {
        self.compact.set(compact);
        self.render_subtitle();
    }

    fn render_subtitle(&self) {
        let text = self.text.borrow();
        if self.compact.get() || text.is_empty() {
            self.subtitle.set_visible(false);
            return;
        }
        self.subtitle
            .set_label(&format!("\u{2002}\u{b7}\u{2002}{text}"));
        self.subtitle.set_visible(true);
    }

    /// Points the dot at the most urgent thing any agent in this project is
    /// doing, or hides it when nothing is running.
    ///
    /// Most urgent rather than most common, and the order is the app's existing
    /// one: an agent waiting on you outranks an agent working, which outranks
    /// an agent sitting idle. A summary that averaged would report "idle" for a
    /// project with three idle agents and one that has been waiting ten minutes
    /// for an answer, which is the single case this dot exists for.
    pub(super) fn set_tally(&self, tally: &crate::tiler::Tally) {
        for class in ["waiting", "working", "idle"] {
            self.dot.remove_css_class(class);
        }
        let class = if tally.waiting > 0 {
            "waiting"
        } else if tally.working > 0 {
            "working"
        } else if tally.total() > 0 {
            "idle"
        } else {
            self.dot.set_visible(false);
            return;
        };
        self.dot.add_css_class(class);
        self.dot.set_visible(true);
    }
}

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
        title: &HeaderTitle,
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

        // An empty centre, with the real title packed at the start below. An
        // `AdwHeaderBar` centres whatever it is handed as a title widget, and
        // this one is a location rather than a caption - the same argument the
        // drawer's own header settled (see `build_sidebar`).
        let header = adw::HeaderBar::builder()
            .title_widget(&gtk4::Box::new(gtk4::Orientation::Horizontal, 0))
            .show_start_title_buttons(false)
            .build();
        header.pack_start(sidebar_toggle);
        header.pack_start(title.widget());

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

        // The strip's one primary. Every control in this bar was drawn
        // identically, which left the action the window exists for - start
        // another agent - indistinguishable from "check for updates". It takes
        // a filament outline rather than a fill; see `.header-primary`.
        let new_agent = gtk4::Button::builder()
            .icon_name("tab-new-symbolic")
            .can_focus(false)
            .valign(gtk4::Align::Center)
            .css_classes(["header-action", "header-primary"])
            .tooltip_text("Spawn a new agent in this project")
            .build();
        let this = self.clone();
        new_agent.connect_clicked(move |_| {
            if let Some(tiler) = this.active_tiler() {
                tiler.spawn_pane_here();
            }
        });

        // A split rather than a menu on the `+` itself. Spawning an agent is
        // the action this window exists for and it keeps its single click; the
        // arrow is for the other answer, which is rarer by construction, since
        // a project tends to be a project you run one kind of agent in. The
        // group remembers what the arrow was last used for, so it is rarer
        // still after the first time.
        let agent_menu = gio::Menu::new();
        for kind in Kind::ALL {
            agent_menu.append(
                Some(&format!("Spawn {}", kind.label())),
                Some(&format!("win.spawn-{}", kind.label())),
            );
        }
        let choose_agent = gtk4::MenuButton::builder()
            .can_focus(false)
            .valign(gtk4::Align::Center)
            .css_classes(["header-action", "header-split-arrow"])
            .menu_model(&agent_menu)
            .tooltip_text("Choose which agent to spawn")
            .build();

        let spawn_split = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .css_classes(["header-split"])
            .build();
        spawn_split.append(&new_agent);
        spawn_split.append(&choose_agent);

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
        header.pack_end(&spawn_split);
        header.pack_end(&broadcast);
        // The layout switcher moves to this end, away from the title.
        //
        // It sat at the start, between the drawer toggle and the (then centred)
        // title, which put a three-position control in the one part of the bar
        // that answers "where am I". Everything at this end acts on the
        // workspace - spawn an agent, broadcast to all of them, arrange them -
        // and everything at the other end says where you are. One question per
        // side of the bar is most of what stops a toolbar reading as a drawer
        // of loose parts.
        header.pack_end(&self.build_mode_switcher());

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
    fn build_mode_switcher(&self) -> adw::ToggleGroup {
        // AdwToggleGroup rather than a linked row of ToggleButtons - this is
        // the widget the libadwaita 1.7 floor was raised for. The old row
        // hand-rolled what the group means natively: exactly one active, the
        // active one refusing to un-toggle, and one keyboard stop instead of
        // three. Centred rather than filling the bar's height, for the same
        // reason the round buttons beside it are.
        let group = adw::ToggleGroup::builder()
            .valign(gtk4::Align::Center)
            .css_classes(["mode-switcher"])
            .can_focus(false)
            .build();
        for (_, icon, tooltip) in MODE_BUTTONS {
            let toggle = adw::Toggle::builder().icon_name(icon).build();
            toggle.set_tooltip(tooltip);
            group.add(toggle);
        }
        let this = self.clone();
        group.connect_active_notify(move |group| {
            if this.0.syncing_mode.get() {
                return;
            }
            let Some((mode, _, _)) = MODE_BUTTONS.get(group.active() as usize) else {
                return;
            };
            if let Some(tiler) = this.active_tiler() {
                tiler.set_mode(*mode);
            }
        });
        *self.0.mode_switcher.borrow_mut() = Some(group.clone());
        group
    }

    /// Points the mode toggles at `mode` without letting their `active` signal
    /// write it straight back - see `Inner::syncing_mode`.
    pub(super) fn sync_mode_buttons(&self, mode: Mode) {
        let Some(index) = MODE_BUTTONS.iter().position(|(m, _, _)| *m == mode) else {
            return;
        };
        let Some(group) = self.0.mode_switcher.borrow().clone() else {
            return;
        };
        self.0.syncing_mode.set(true);
        group.set_active(index as u32);
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
        if let Some(group) = self.0.mode_switcher.borrow().clone() {
            group.set_sensitive(pane_count > 0);
        }
    }

    /// A picture of the thing this window does, drawn out of the window's own
    /// parts: four tiles in a grid with one of them lit.
    ///
    /// This replaces a stock `tab-new-symbolic` at status-page size - a sheet
    /// of paper with a plus on it, which is the icon every application in the
    /// world shows on an empty screen and says nothing about what *this* one
    /// would do with the space. The first screen of a tiling window manager can
    /// afford to show the tiling.
    ///
    /// It is built from boxes wearing the panes' own rules rather than drawn as
    /// an asset, which is what keeps it honest: `.empty-tile` takes its fill,
    /// its radius and its lit treatment from the same palette names `.pane`
    /// does, so a change to the ramp or to @filament moves the diagram with the
    /// real thing instead of leaving a picture of the app as it used to look.
    ///
    /// Four rather than three, and lit top-left rather than centre: this is the
    /// grid the app opens in, and the lamp is the one fact about it worth
    /// teaching before the first agent starts.
    fn build_empty_diagram(&self) -> gtk4::Box {
        let tile = |lit: bool| {
            let classes: &[&str] = if lit {
                &["empty-tile", "lit"]
            } else {
                &["empty-tile"]
            };
            gtk4::Box::builder().css_classes(classes).build()
        };

        let row = |a: gtk4::Box, b: gtk4::Box| {
            let row = gtk4::Box::builder()
                .orientation(gtk4::Orientation::Horizontal)
                .spacing(5)
                .build();
            row.append(&a);
            row.append(&b);
            row
        };

        let grid = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(5)
            .halign(gtk4::Align::Center)
            .css_classes(["empty-diagram"])
            .build();
        grid.append(&row(tile(true), tile(false)));
        grid.append(&row(tile(false), tile(false)));
        grid
    }

    /// What a project with nothing running shows.
    ///
    /// This is the first screen of the app, so it has exactly one job: say what
    /// to press. The previous answer was a help pane listing all twenty-one
    /// bindings at once, which is a reference card handed to someone who has not
    /// yet done the one thing that makes any of them matter. The full list is
    /// still a keystroke away, in the menu and on `Super+Alt+/`.
    ///
    /// The page builds its own heading and description rather than taking the
    /// `StatusPage`'s. Both were already half ours - the description had to be,
    /// to wrap at the window's width rather than the clamp's - and the heading
    /// arrives from libadwaita at a size chosen for a full-window error page,
    /// which is louder than a workspace waiting for its first agent needs to be.
    /// What the widget is still worth having for is the part that has nothing to
    /// do with looks: it centres a clamp in whatever space it is given, at every
    /// window size, which is the one piece of geometry here worth not writing.
    pub(super) fn build_empty_state(&self) -> adw::StatusPage {
        // The action that answers the sentence above it. This page exists to
        // report "no agents running", and starting one is what stops that being
        // true - so it leads, and opening another project follows. That is the
        // other way round from how this read for its first year, when the
        // primary was "Open a project..." on a page that was already standing
        // inside a project.
        //
        // Not `suggested-action`. That class paints from `accent_bg_color`,
        // which this app aliases to @filament - so the stock treatment made
        // this button a solid warm pill and the brightest thing in the window,
        // in the one colour the stylesheet reserves for "the keyboard is here".
        // A primary action on an empty screen has no competition and needs
        // none: it is one of two things on an otherwise blank page.
        let agent = gtk4::Button::builder()
            .label("Start an agent here")
            .halign(gtk4::Align::Center)
            .css_classes(["pill", "empty-primary"])
            .tooltip_text("Run claude in this project's own folder")
            .build();
        let this = self.clone();
        agent.connect_clicked(move |_| {
            if let Some(tiler) = this.active_tiler() {
                tiler.spawn_pane_here();
            }
        });

        let start = gtk4::Button::builder()
            .label("Open another project\u{2026}")
            .halign(gtk4::Align::Center)
            .css_classes(["pill", "empty-secondary"])
            .build();
        let this = self.clone();
        start.connect_clicked(move |_| this.new_project());

        // The one line worth adding to an empty screen: where everything else
        // is. This is the only page in the app with nothing on it to read, so
        // it is the only place a pointer to the command palette costs nothing
        // and is certain to be seen.
        let hint = gtk4::Label::builder()
            .label("Super+Alt+P for everything else")
            .halign(gtk4::Align::Center)
            .css_classes(["empty-hint"])
            .build();

        let heading = gtk4::Label::builder()
            .label("No agents running")
            .halign(gtk4::Align::Center)
            .css_classes(["empty-heading"])
            .build();

        // Our own label rather than the StatusPage's `description`: the stock
        // one wraps at its clamp's width, not the window's, so a window
        // narrower than the clamp (a quarter-snap on a tiled desktop) clipped
        // the sentence mid-word at the frame. A label told to wrap with a
        // bounded natural width shrinks instead.
        //
        // It describes the diagram above it rather than repeating the buttons
        // below it. The old copy spent its first clause explaining the folder
        // picker, which is what the button says; what nothing on the page said
        // was what happens *after* - that the panes arrange themselves, and
        // that one of them is always the lit one.
        let description = gtk4::Label::builder()
            .label(
                "Start one and it takes the whole workspace. Start another and they \
                 tile themselves \u{2014} every agent keeps an equal share, and the one \
                 you're typing in stays lit.",
            )
            .wrap(true)
            .justify(gtk4::Justification::Center)
            .max_width_chars(52)
            .halign(gtk4::Align::Center)
            .css_classes(["empty-description"])
            .build();

        // The description is clamped rather than merely told a width, and this
        // is a bug being fixed rather than a preference. `max-width-chars` sets
        // a label's *natural* width, which GTK is free to exceed when the
        // parent has room - and a StatusPage centred in a 1500px workspace has
        // room, so the sentence came out as one 130-character line spanning the
        // whole window. A clamp is a hard ceiling: the label wraps at 34em and
        // shrinks below it, which is what the `max_width_chars` on it was
        // always meant to be saying and never was.
        let clamped = adw::Clamp::builder()
            .maximum_size(430)
            .child(&description)
            .build();

        let buttons = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(8)
            .halign(gtk4::Align::Center)
            .build();
        buttons.append(&self.build_empty_diagram());
        buttons.append(&heading);
        buttons.append(&clamped);
        buttons.append(&agent);
        buttons.append(&start);
        buttons.append(&hint);

        // The floor, which this page has to paint for itself: it is the tiler's
        // sibling in the project's stack rather than its child, and the floor is
        // now painted per-region rather than by one fill behind everything (see
        // `appearance::content_css`). Without this the empty state is a window
        // with a desktop showing through it.
        adw::StatusPage::builder()
            .css_classes(["workspace-floor", "empty-state"])
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

        // One per agent, named after it: the split button's menu, the command
        // palette and any future keybinding all want the same verb, and an
        // action is the one place GTK lets three callers share one.
        for kind in Kind::ALL {
            let this = self.clone();
            let spawn = gio::SimpleAction::new(&format!("spawn-{}", kind.label()), None);
            spawn.connect_activate(move |_, _| {
                if let Some(tiler) = this.active_tiler() {
                    tiler.spawn_pane_of(kind);
                }
            });
            self.0.window.add_action(&spawn);
        }
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
