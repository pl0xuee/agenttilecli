//! The appearance settings, with the window changing as you move them.
//!
//! Everything here was reachable already by editing `config.toml` and
//! restarting. That is a reasonable way to set a shell command and a poor way to
//! choose an opacity, because opacity is a thing you judge by looking at it: the
//! right value depends on the wallpaper behind the window, which is not a fact
//! anyone can hold in their head while typing a number into a file.
//!
//! So these apply live, on every tick of the control. Nothing is confirmed and
//! there is no OK button - the window in front of you is the preview, and the
//! dialog is small enough to see past.
//!
//! **What it writes, and what it doesn't.** These land in the session, never in
//! `config.toml`. That file exists to be opened and commented, and serialising a
//! struct back over it would delete every comment in it. The file states what
//! the app opens as; the session remembers what you last dragged it to. Only
//! the fields that actually differ from the file are written, so a slider you
//! never touched can't freeze its current value into the session and start
//! quietly overriding later edits to the file.
//!
//! Deliberately not everything the config file holds. The command a pane runs,
//! how many agents a project starts with and the scrollback depth are all things
//! you set once with a reason, and a dialog that mixes them in with the look
//! turns "make this a bit less transparent" into a settings window to be read.

use adw::prelude::*;

use crate::app::App;
use crate::appearance::{self, Appearance};

/// Opens the preferences dialog over `app`'s window.
pub fn present(app: &App) {
    // `.atc-dialog` is what scopes this app's own rules onto libadwaita's
    // preference widgetry - see the block of that name in `style.css`. Without
    // it the rows arrive on `card_bg_color`, which this app aliases to @chip:
    // the rung meant for a small control sitting on a pane, and sixteen points
    // lighter than the dialog it would be sitting in.
    let page = adw::PreferencesPage::builder()
        .title("Preferences")
        .icon_name("preferences-system-symbolic")
        .css_classes(["atc-dialog"])
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("Appearance")
        .description(
            "The window's chrome is translucent and its panes are solid, until you \
             say otherwise. Raise either opacity if a bright wallpaper is showing \
             through more than you want - and expect to want the panes higher than \
             the chrome, since that is the surface with text on it.",
        )
        .build();

    group.add(&opacity_row(
        app,
        "Window opacity",
        "The gutters between tiles, the strip behind the header bar, and the rack",
        |appearance| appearance.window_opacity,
        |appearance, value| appearance.window_opacity = value,
    ));

    group.add(&opacity_row(
        app,
        "Pane opacity",
        "The terminal surfaces themselves. Lowering this puts your wallpaper behind \
         the text an agent is writing",
        |appearance| appearance.pane_opacity,
        |appearance, value| appearance.pane_opacity = value,
    ));

    // Half the gutter, and the row says so rather than making anyone deduce it
    // from the fact that two tiles share a seam - which is what `layout`'s
    // header had to explain at length, and how this ended up twice the size it
    // wanted to be once already.
    let gap = adw::SpinRow::builder()
        .title("Space around tiles")
        .subtitle("In pixels. Two tiles that share a seam end up twice this far apart")
        .adjustment(&gtk4::Adjustment::new(
            f64::from(appearance::get().gap),
            0.0,
            40.0,
            1.0,
            4.0,
            0.0,
        ))
        .build();
    {
        let app = app.clone();
        gap.connect_value_notify(move |row| {
            let mut next = appearance::get();
            next.gap = row.value() as i32;
            app.set_appearance(next);
        });
    }
    group.add(&gap);

    page.add(&group);

    // A height as well as a width, because without one the dialog opened at
    // roughly the height of two of its three rows and cut the third off at the
    // frame - "Space around tiles" arrived permanently half-drawn. An
    // `AdwPreferencesDialog` sizes itself to its content only up to the height
    // it is given, and the default is not enough for a group carrying a
    // four-line description above three subtitled rows.
    let dialog = adw::PreferencesDialog::builder()
        .title("Preferences")
        .content_width(560)
        .content_height(520)
        .build();
    dialog.add(&page);
    dialog.present(Some(app.window()));
}

/// One opacity control, as a spin row over the 0.5..=1.0 the appearance clamps
/// to.
///
/// A spin row rather than a slider, and the reason is the range: a slider across
/// half a unit gives every pixel of travel about a percent of opacity, which is
/// impossible to land on a round number with and impossible to read back off
/// afterwards. Two decimal places in a box says exactly where you are.
fn opacity_row(
    app: &App,
    title: &str,
    subtitle: &str,
    read: fn(&Appearance) -> f64,
    write: fn(&mut Appearance, f64),
) -> adw::SpinRow {
    let row = adw::SpinRow::builder()
        .title(title)
        .subtitle(subtitle)
        .digits(2)
        .adjustment(&gtk4::Adjustment::new(
            read(&appearance::get()),
            0.5,
            1.0,
            0.01,
            0.05,
            0.0,
        ))
        .build();

    let app = app.clone();
    row.connect_value_notify(move |row| {
        let mut next = appearance::get();
        write(&mut next, row.value());
        app.set_appearance(next);
    });
    row
}
