//! The keyboard cheatsheet.
//!
//! This replaces a help *pane* - a VTE terminal with no process behind it, fed
//! ~160 lines of hand-laid-out ANSI: three columns padded to a common height,
//! a `visible_len` that counted characters while skipping escape sequences, and
//! a `side_by_side` that padded each column to its own widest line. All of that
//! existed to reimplement, in a terminal, what a list widget does for free.
//!
//! It cost more than the code. The cheatsheet took a tile in the grid, so
//! reading the keys meant giving up a pane, and it was a *pane* - so the way to
//! dismiss it was to close it, and the way to learn that was to read the
//! cheatsheet. Worse, it was a second copy of the bindings: nothing connected
//! the text to `keybindings`, so a binding could change and the help would go on
//! confidently describing the old one.
//!
//! Now it's a dialog rendered from `keybindings::SECTIONS` - the same table the
//! matcher is written beside - and the keys are drawn by `GtkShortcutLabel`
//! from real accelerator strings rather than spelled out by hand.
//!
//! Built from `AdwPreferencesDialog` rather than `GtkShortcutsWindow`, which is
//! deprecated as of GTK 4.18, or `AdwShortcutsDialog`, which would raise this
//! app's libadwaita floor to 1.8 for no gain over what 1.5 already draws well.

use adw::prelude::*;

use crate::keybindings::SECTIONS;

/// Opens the shortcuts dialog over `parent`.
pub fn present(parent: &impl IsA<gtk4::Widget>) {
    let page = adw::PreferencesPage::builder()
        .title("Keyboard Shortcuts")
        .icon_name("preferences-desktop-keyboard-symbolic")
        .build();

    for section in SECTIONS {
        let group = adw::PreferencesGroup::builder()
            .title(section.title)
            .description(section.note.unwrap_or_default())
            .build();

        for binding in section.bindings {
            let keys = gtk4::ShortcutLabel::builder()
                .accelerator(binding.accelerator)
                .valign(gtk4::Align::Center)
                .build();
            let row = adw::ActionRow::builder()
                .title(binding.description)
                .activatable(false)
                .build();
            row.add_suffix(&keys);
            group.add(&row);
        }
        page.add(&group);
    }

    let dialog = adw::PreferencesDialog::builder()
        .title("Keyboard Shortcuts")
        .content_width(560)
        .content_height(720)
        .build();
    dialog.add(&page);
    dialog.present(Some(parent));
}
