mod app;
mod clipboard;
mod keybindings;
mod layout;
mod model;
mod palette;
mod pane;
mod shortcuts;
#[cfg(test)]
mod testing;
mod tiler;
mod update;
mod updates;

use adw::prelude::*;
use gtk4::{gdk, glib, CssProvider};

use app::App;

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
    // Before anything else, and in particular before an update can overwrite the
    // file we're running from - which is what makes its path unreadable. See
    // `update::remember_exe`.
    update::remember_exe();

    let application = adw::Application::builder()
        .application_id(app_id())
        .build();
    application.connect_startup(|_| {
        load_css();
        // This app has exactly one palette, and it is a dark one - the graphite
        // ramp, the warm focus lamp and the ANSI colours inside every pane are
        // all built against each other and against a dark surface. Letting
        // libadwaita follow the desktop's light/dark preference would repaint
        // its own widgets light while every terminal stayed dark, which is not
        // a light theme - it's a broken dark one. A real light variant means a
        // second ramp, and that is its own piece of work.
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    });
    application.connect_activate(build_window);
    application.run()
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

fn build_window(application: &adw::Application) {
    gtk4::Window::set_default_icon_name("agenttilecli");

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/".to_string());

    let app = App::new(application, &cwd, &base_title());
    keybindings::install(app.window(), &app);
    app.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every label in this app that ellipsizes, by CSS class. Kept here rather
    /// than derived, because the property that matters is one a reader of
    /// `style.css` cannot see: whether the widget wearing that class was built
    /// with `EllipsizeMode` set on it.
    const ELLIPSIZING_LABELS: &[&str] = &[".pane-dir", ".sidebar-row-label", ".sidebar-version"];

    /// An ellipsizing label must not be given `letter-spacing`.
    ///
    /// GTK leaves letter-spacing out of a label's size request, so the label is
    /// allocated the width of the *un-spaced* string and then ellipsizes the
    /// wider text it actually draws down to fit inside it. The result is a name
    /// cut short in a row with obvious room to spare - "Getting Start…" in a
    /// sidebar half empty - and nothing about it points at the spacing.
    ///
    /// This has now been introduced three separate times, twice after being
    /// fixed, which is what this test is for. A Pango attribute does not dodge
    /// it either; the spacing simply cannot be had on a label that ellipsizes,
    /// and dropping the ellipsize instead means one long project name setting
    /// the minimum width of the whole window.
    #[test]
    fn an_ellipsizing_label_is_never_letter_spaced() {
        let css = include_str!("style.css");
        for class in ELLIPSIZING_LABELS {
            let start = css
                .find(&format!("\n{class} {{"))
                .unwrap_or_else(|| panic!("{class} has no rule in style.css - has it been renamed?"));
            let body = &css[start..];
            let body = &body[..body.find('}').expect("an unterminated rule")];
            assert!(
                !body.contains("letter-spacing"),
                "{class} both ellipsizes and is letter-spaced, so it will \
                 ellipsize text that would otherwise have fitted",
            );
        }
    }
    use crate::testing::gtk_test;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// GTK doesn't reject a stylesheet it can't understand - it drops the
    /// offending declaration, prints a warning to a terminal a GUI app doesn't
    /// have, and carries on. So a mistyped property doesn't fail the build, or
    /// the app, or the eye: it just quietly stops styling something, which is
    /// indistinguishable from the rule having worked and looked like that.
    ///
    /// GTK's CSS is also only a *subset* of the web's, and the gap is where this
    /// bites: `animation-name: none` to stop the update button's pulse under the
    /// pointer is valid CSS that GTK may or may not take. This is what says
    /// which - at `cargo test`, rather than by squinting at a button.
    #[test]
    fn the_stylesheet_parses_without_errors() {
        gtk_test(|| {
            let errors = Rc::new(RefCell::new(Vec::new()));
            let provider = CssProvider::new();
            let sink = errors.clone();
            provider.connect_parsing_error(move |_, section, error| {
                sink.borrow_mut()
                    .push(format!("{}: {error}", section.to_str()));
            });
            provider.load_from_string(include_str!("style.css"));

            let errors = errors.borrow();
            assert!(
                errors.is_empty(),
                "style.css has {} parse error(s) GTK would have silently ignored:\n{}",
                errors.len(),
                errors.join("\n"),
            );
        });
    }
}
