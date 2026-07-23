//! Finding something in a pane's scrollback.
//!
//! An agent's turn can run to thousands of lines, and the interesting one is
//! usually somewhere in the middle: the error before the retry, the path it
//! printed twenty tool calls ago. Scrolling back to look for it by eye is the
//! kind of work a terminal should not be asking for.
//!
//! Searches the *focused* pane, not all of them. A window-wide search would
//! have to say which pane each hit was in and move focus to it, and the
//! question people actually have is "where in this one" - they already know
//! which agent they were reading.
//!
//! The bar is a `GtkSearchBar`, which brings the behaviour people expect from
//! every other one: Escape closes it, and closing it hands the keyboard back.

use gtk4::prelude::*;
use gtk4::{gdk, glib};
use vte4::{prelude::*, Regex, Terminal};

use crate::app::App;

/// PCRE2's `MULTILINE` and `CASELESS`. Case-insensitive because a terminal
/// search is a search for something you half-remember seeing.
const SEARCH_FLAGS: u32 = 0x0000_0400 | 0x0000_0008;

/// The search bar, and the wiring that points it at whichever pane has focus.
#[derive(Clone)]
pub struct Search {
    bar: gtk4::SearchBar,
    entry: gtk4::SearchEntry,
}

impl Search {
    pub fn new(app: &App) -> Self {
        let entry = gtk4::SearchEntry::builder()
            .placeholder_text("Find in this pane")
            .hexpand(true)
            .build();

        let previous = gtk4::Button::builder()
            .icon_name("go-up-symbolic")
            .can_focus(false)
            .tooltip_text("Previous match (Shift+Enter)")
            .css_classes(["flat"])
            .build();
        let next = gtk4::Button::builder()
            .icon_name("go-down-symbolic")
            .can_focus(false)
            .tooltip_text("Next match (Enter)")
            .css_classes(["flat"])
            .build();

        let row = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["search-row"])
            .build();
        row.append(&entry);
        row.append(&previous);
        row.append(&next);

        let bar = gtk4::SearchBar::builder()
            .key_capture_widget(app.window())
            .child(&row)
            .build();
        bar.connect_entry(&entry);

        let this = Search { bar, entry };

        // Retyping the pattern on every keystroke is what makes the search feel
        // live. VTE holds the compiled regex, so this is the only place it is
        // set - and clearing it when the box is empty stops the last search
        // leaving highlights behind.
        let search = this.clone();
        let app_for_change = app.clone();
        this.entry.connect_search_changed(move |entry| {
            search.point_at(&app_for_change, &entry.text());
        });

        let app_for_next = app.clone();
        this.entry.connect_activate(move |_| {
            if let Some(terminal) = focused_terminal(&app_for_next) {
                terminal.search_find_next();
            }
        });

        let app_for_button = app.clone();
        next.connect_clicked(move |_| {
            if let Some(terminal) = focused_terminal(&app_for_button) {
                terminal.search_find_next();
            }
        });
        let app_for_button = app.clone();
        previous.connect_clicked(move |_| {
            if let Some(terminal) = focused_terminal(&app_for_button) {
                terminal.search_find_previous();
            }
        });

        // Shift+Enter for the other direction, which is what every other search
        // box in the desktop does.
        let app_for_keys = app.clone();
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, state| {
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            if shift && matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) {
                if let Some(terminal) = focused_terminal(&app_for_keys) {
                    terminal.search_find_previous();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        this.entry.add_controller(keys);

        this
    }

    pub fn widget(&self) -> &gtk4::SearchBar {
        &self.bar
    }

    /// Opens the bar and puts the cursor in it, or closes it if it was already
    /// open - so the same key does both, as it does everywhere else.
    pub fn toggle(&self, app: &App) {
        let opening = !self.bar.is_search_mode();
        self.bar.set_search_mode(opening);
        if opening {
            self.entry.grab_focus();
        } else {
            // Leaving a compiled regex behind would leave its highlights behind
            // with it.
            self.point_at(app, "");
            if let Some(terminal) = focused_terminal(app) {
                terminal.grab_focus();
            }
        }
    }

    /// Points the focused pane's terminal at `pattern`, or clears it.
    ///
    /// The pattern is escaped rather than passed through: people type paths and
    /// error messages into a search box, both of which are full of characters
    /// PCRE2 has opinions about, and "no such file (os error 2)" should find
    /// that line rather than fail to compile.
    fn point_at(&self, app: &App, pattern: &str) {
        let Some(terminal) = focused_terminal(app) else {
            return;
        };
        if pattern.is_empty() {
            terminal.search_set_regex(None, 0);
            return;
        }
        match Regex::for_search(&glib::Regex::escape_string(pattern), SEARCH_FLAGS) {
            Ok(regex) => {
                terminal.search_set_regex(Some(&regex), 0);
                // A search that stops at the top of the scrollback is one you
                // have to think about the direction of.
                terminal.search_set_wrap_around(true);
                terminal.search_find_previous();
            }
            Err(_) => terminal.search_set_regex(None, 0),
        }
    }
}

/// The terminal the keyboard is in, if any.
fn focused_terminal(app: &App) -> Option<Terminal> {
    app.active_tiler()?.focused_terminal()
}
