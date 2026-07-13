mod groups;
mod keybindings;
mod layout;
mod pane;
mod tiler;

use gtk4::prelude::*;
use gtk4::{gdk, glib, Application, ApplicationWindow, CssProvider};

use groups::Groups;

const APP_ID: &str = "dev.agenttilecli.AgentTileCli";

/// The GTK application id - APP_ID for builds off `master`, with a
/// branch-specific suffix otherwise. GApplication is single-instance per id
/// (activating a second launch just wakes the first), so without this a dev
/// build launched alongside an already-running master build wouldn't open
/// its own window - it'd just poke the master instance over D-Bus.
fn app_id() -> String {
    const BRANCH: &str = env!("AGENTTILECLI_GIT_BRANCH");
    if BRANCH.is_empty() || BRANCH == "master" {
        APP_ID.to_string()
    } else {
        let suffix: String = BRANCH
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        format!("{APP_ID}.{suffix}")
    }
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(app_id()).build();
    app.connect_startup(|_| load_css());
    app.connect_activate(build_window);
    app.run()
}

/// The window title's base text - "AgentTileCLI", with a "[branch]"
/// suffix when built from anything other than `master` so dev builds
/// are easy to tell apart from release ones at a glance.
fn base_title() -> String {
    const BRANCH: &str = env!("AGENTTILECLI_GIT_BRANCH");
    if BRANCH.is_empty() || BRANCH == "master" {
        "AgentTileCLI".to_string()
    } else {
        format!("AgentTileCLI [{BRANCH}]")
    }
}

fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("no default display"),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn build_window(app: &Application) {
    gtk4::Window::set_default_icon_name("agenttilecli");

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/".to_string());

    let groups = Groups::new(&cwd);

    // A clean 16:10 aspect ratio (1488 / 930 = 1.6 exactly). At this size the
    // default monospace font gives the help pane roughly 183x47 cells to
    // work with; its help_text() is laid out three-wide to actually use that
    // space (widest line 157 columns, across 41 lines) rather than leaving
    // most of the window blank. This is just the starting size - the app
    // never resizes itself afterward (adding panes tiles them smaller
    // within whatever size the window already is instead), so the user's
    // own resize is the last word on how big it gets.
    let window = ApplicationWindow::builder()
        .application(app)
        .title(base_title())
        .default_width(1488)
        .default_height(930)
        .child(groups.widget())
        .build();

    let window_weak = window.downgrade();
    groups.set_title_callback(move |title| {
        if let Some(window) = window_weak.upgrade() {
            let title = if title.is_empty() {
                base_title()
            } else {
                format!("{} — {title}", base_title())
            };
            window.set_title(Some(&title));
        }
    });

    keybindings::install(&window, &groups);

    if let Some(tiler) = groups.active_tiler() {
        tiler.toggle_help();
    }
    window.present();
}
