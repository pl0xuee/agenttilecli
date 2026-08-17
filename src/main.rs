mod agent;
mod app;
mod appearance;
mod clipboard;
mod commands;
mod config;
mod editor;
mod hooks;
mod ipc;
mod keybindings;
mod layout;
mod links;
mod model;
mod palette;
mod pane;
mod preferences;
mod search;
mod session;
mod shortcuts;
#[cfg(test)]
mod testing;
mod tiler;
mod update;
mod updates;

use adw::prelude::*;
use gtk4::{CssProvider, gdk, glib};

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
    // Before anything else at all: this process may not be a window. claude runs
    // `agenttilecli --hook <event>` from inside a pane, and that invocation has
    // to do its one small job and get out of the way - no GTK, no application
    // id, no single-instance handshake that would hand the work to the running
    // window and wait for it.
    if let Some(event) = hook_event() {
        report_hook(event);
        return glib::ExitCode::SUCCESS;
    }

    // Before anything else, and in particular before an update can overwrite the
    // file we're running from - which is what makes its path unreadable. See
    // `update::remember_exe`.
    update::remember_exe();

    // Read before the window is built: the config sets the window's own opening
    // size indirectly (through the session), what its panes run, and how much
    // air there is between them.
    config::install(config::Config::load());
    // After the config and before the window: the appearance takes its defaults
    // from the config file, and the window is painted from the appearance.
    appearance::install();

    let builder = adw::Application::builder().application_id(app_id());
    // A capture run must never be handed off to a window that is already open.
    // GApplication is single-instance per id, so `ATC_SHOT=... cargo run` beside
    // a running build of the same branch would send an activate to *that*
    // process - which has no ATC_SHOT in its environment - and exit having
    // photographed nothing, silently.
    //
    // Shadowed rather than mutated, because the whole block vanishes in a
    // release build and a `mut` binding nothing reassigns is a warning there.
    #[cfg(debug_assertions)]
    let builder = match std::env::var_os("ATC_SHOT") {
        Some(_) => builder.flags(gtk4::gio::ApplicationFlags::NON_UNIQUE),
        None => builder,
    };
    let application = builder.build();
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
    let display = gdk::Display::default().expect("no default display");

    let provider = CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // A second provider, for the handful of declarations `style.css` is not
    // allowed to hold - see `standalone_colour_css`. Empty on an older GTK, and
    // an empty provider costs nothing, so this is unconditional.
    let standalone = CssProvider::new();
    standalone.load_from_string(&standalone_colour_css());
    gtk4::style_context_add_provider_for_display(
        &display,
        &standalone,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // And the palette aliases for libadwaita's named colours, one notch above
    // the user's own gtk.css - the only provider of this app's that is. A
    // theming tool that generates `~/.config/gtk-4.0/gtk.css` redefines these
    // exact names at priority 800, and a `@define-color` fight is won purely on
    // priority; the file's header tells the whole story. Definitions only, so
    // nothing in it can shadow a rule from the providers below.
    let adwaita = CssProvider::new();
    adwaita.load_from_string(include_str!("adwaita-colors.css"));
    gtk4::style_context_add_provider_for_display(
        &display,
        &adwaita,
        gtk4::STYLE_PROVIDER_PRIORITY_USER + 1,
    );
}

/// The five standalone signal colours, for the libadwaita that stopped reading
/// them as `@define-color` names.
///
/// `style.css` aliases the ramp onto Adwaita's colour names, and for surfaces
/// that still works. For the five *standalone* colours - `accent_color`,
/// `destructive_color`, `success_color`, `warning_color`, `error_color`, which
/// libadwaita uses for text, icons, check marks and links rather than for fills -
/// it stopped working at 1.6: the compatibility shim maps the surface names onto
/// CSS variables but *derives* these, as an oklab lightening of the matching
/// `*_bg_color`. Nothing reads `@accent_color` on a modern install, so those five
/// lines in the stylesheet are inert there - and the colour libadwaita picks
/// instead is a lightened @filament rather than @filament, which is close enough
/// to have gone unnoticed and is still not what the palette says.
///
/// Generated rather than written down: it is built from `palette`, which reads
/// the ramp out of the stylesheet, so there is no second copy of these values
/// to drift. (It used to be runtime-gated as well, when the GTK floor was 4.12
/// and a custom property in a stylesheet was a parse error there; the floor is
/// 4.16 now - see Cargo.toml - and the gate went with it.)
///
/// The five `@define-color *_color` aliases stay in `adwaita-colors.css`
/// regardless. They are what a libadwaita older than 1.6 reads, they cost
/// nothing on a newer one, and they keep that file the complete statement of
/// what Adwaita is told.
fn standalone_colour_css() -> String {
    // Property name, and the ramp rung it is meant to be.
    const STANDALONE: [(&str, &str); 5] = [
        ("--accent-color", "filament"),
        ("--destructive-color", "hangup"),
        ("--success-color", "fresh"),
        ("--warning-color", "tally"),
        ("--error-color", "hangup"),
    ];

    let mut css = String::from(":root {\n");
    for (property, name) in STANDALONE {
        let c = palette::color(name);
        css.push_str(&format!(
            "  {property}: #{:02x}{:02x}{:02x};\n",
            c.r, c.g, c.b
        ));
    }
    css.push_str("}\n");
    css
}

/// The event named by `--hook <event>`, if this process was launched as one.
fn hook_event() -> Option<hooks::Event> {
    let mut args = std::env::args().skip(1);
    if args.next()? != "--hook" {
        return None;
    }
    hooks::Event::parse(&args.next()?)
}

/// Tells the window what just happened in this pane, and returns.
///
/// Every path here is infallible by construction, because the caller is claude
/// and the cost of failing is claude's. A window that has closed, a socket that
/// was never created, a hook environment that isn't there: all of them mean the
/// same thing - nobody is listening - and the answer to that is to exit
/// quietly. The bell hook on `Stop` and `Notification` is what still gets
/// through when this doesn't (see `hooks::settings_json`).
fn report_hook(event: hooks::Event) {
    let (Ok(pane), Ok(socket)) = (std::env::var(ipc::ENV_PANE), std::env::var(ipc::ENV_SOCKET))
    else {
        return;
    };

    // claude hands the hook a JSON object on stdin. The only field this app has
    // a use for is which tool is about to run - and reading it is best-effort
    // for the same reason as everything else here: an event with no tool name
    // is still worth reporting.
    let tool = std::io::read_to_string(std::io::stdin())
        .ok()
        .and_then(|input| serde_json::from_str::<serde_json::Value>(&input).ok())
        .and_then(|v| v["tool_name"].as_str().map(str::to_string));

    let _ = ipc::send(&socket, &ipc::Message { pane, event, tool });
}

fn build_window(application: &adw::Application) {
    // `activate` fires again every time another launch forwards to this
    // process - GApplication is single-instance per id, and a second
    // `agenttilecli` from a terminal is the common way. That launch means
    // "show me the window", not "build me another": a second `App::new` would
    // add one more dynamic CSS provider to the shared display (they are never
    // removed), and re-run `appearance::restore` from disk over live settings
    // the 1500ms debounce hadn't saved yet.
    if let Some(window) = application.active_window() {
        window.present();
        return;
    }

    gtk4::Window::set_default_icon_name("agenttilecli");

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/".to_string());

    let app = App::new(application, &cwd, &base_title());
    keybindings::install(app.window(), &app);
    app.present();

    // After presenting, so the dialog has a window to sit on. Someone typed
    // that file; a mistake in it gets said out loud rather than silently
    // replaced with defaults.
    if let Some(problem) = config::problem() {
        app.report_config_problem(problem);
    }

    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("ATC_SHOT") {
        capture_and_quit(application, &app, path.into());
    }
}

/// Renders the window to a PNG and quits - `ATC_SHOT=/path/to.png`.
///
/// The app draws itself rather than being photographed by a screenshot tool,
/// and that is the point. Every desktop grabber worth using offers "the active
/// window", which is a description of whatever happens to have focus at the
/// instant the shutter fires - and when the answer is "not this app" the file
/// you get is a picture of someone's other window. Asking GTK to render its own
/// widget tree cannot photograph anything that isn't this process.
///
/// It also keeps the alpha. A grabber composites the window against the desktop
/// and hands back an opaque image; this writes the translucency out as real
/// transparency, which is the only way to see what the glass is actually doing
/// rather than what it happens to look like over today's wallpaper.
///
/// `ATC_SHOT_WITH=palette|shortcuts` opens that dialog first. libadwaita's
/// dialogs are a layer inside the window rather than windows of their own, so
/// they land in the same render - but only once something has opened them.
///
/// Debug builds only, alongside the screenshot staging it is usually paired
/// with (`ATC_SCREENSHOT=1 ATC_SHOT=shot.png`).
#[cfg(debug_assertions)]
fn capture_and_quit(application: &adw::Application, app: &App, path: std::path::PathBuf) {
    use gtk4::prelude::*;

    let application = application.clone();
    let app = app.clone();
    // A beat after presenting, so the window has been mapped, laid out and had
    // its first frame drawn. Rendering before that yields a texture of the
    // window's idea of itself before the tiler has allocated anything.
    glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
        // `ATC_SHOT_SIDEBAR=1` re-opens the rack for the shot. At overlay
        // widths startup's own project pick has always closed it by now, so
        // without this the collapsed rack cannot be photographed at all.
        if std::env::var_os("ATC_SHOT_SIDEBAR").is_some() {
            app.show_sidebar_for_screenshot();
        }

        match std::env::var("ATC_SHOT_WITH").as_deref() {
            Ok("palette") => app.show_command_palette(),
            Ok("shortcuts") => app.show_shortcuts(),
            Ok("preferences") => app.show_preferences(),
            Ok(other) => eprintln!("ATC_SHOT_WITH: no dialog called {other:?}"),
            Err(_) => {}
        }

        // Then try to render, and keep trying. A widget tree that has been asked
        // to lay out a dialog does not necessarily have a frame to give on the
        // next tick - `to_node` returns nothing at all when the snapshot came
        // back empty - and how many ticks it takes depends on what else the
        // machine is doing. A fixed delay long enough to always work is a delay
        // that is always wasted; retrying is neither.
        let mut attempts = 0;
        glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            attempts += 1;
            if let Some(texture) = render(app.window()) {
                let bytes = texture.save_to_png_bytes();
                match std::fs::write(&path, &bytes) {
                    Ok(()) => eprintln!("ATC_SHOT: wrote {}", path.display()),
                    Err(e) => eprintln!("ATC_SHOT: could not write {}: {e}", path.display()),
                }
                application.quit();
                return glib::ControlFlow::Break;
            }
            if attempts >= 20 {
                eprintln!("ATC_SHOT: the window never produced a frame");
                application.quit();
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    });
}

/// The window as a texture, or `None` if it has no frame to give yet.
#[cfg(debug_assertions)]
fn render(window: &adw::ApplicationWindow) -> Option<gtk4::gdk::Texture> {
    use gtk4::prelude::*;

    let renderer = window.native()?.renderer()?;
    let paintable = gtk4::WidgetPaintable::new(Some(window));
    let snapshot = gtk4::Snapshot::new();
    paintable.snapshot(
        &snapshot,
        f64::from(window.width()),
        f64::from(window.height()),
    );
    Some(renderer.render_texture(&snapshot.to_node()?, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every label in this app that ellipsizes, by CSS class. Kept here rather
    /// than derived, because the property that matters is one a reader of
    /// `style.css` cannot see: whether the widget wearing that class was built
    /// with `EllipsizeMode` set on it.
    const ELLIPSIZING_LABELS: &[&str] =
        &[".pane-head-label", ".sidebar-row-label", ".sidebar-version"];

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
            let start = css.find(&format!("\n{class} {{")).unwrap_or_else(|| {
                panic!("{class} has no rule in style.css - has it been renamed?")
            });
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
    /// The standalone colours have to be the ramp's, and all five have to be
    /// there.
    ///
    /// These are the one set of colours stated outside `style.css` (see
    /// `standalone_colour_css` for why they have to be), so they are also the one
    /// set that could quietly stop agreeing with it. Generated from `palette`
    /// rather than written down, which makes drift impossible - and this is what
    /// says the generation still happens at all, since a `String::new()` returned
    /// by mistake would leave libadwaita deriving its own accent again and nothing
    /// would look obviously wrong.
    ///
    #[test]
    fn the_standalone_colours_are_the_ramp() {
        let css = standalone_colour_css();
        for (property, name) in [
            ("--accent-color", "filament"),
            ("--destructive-color", "hangup"),
            ("--success-color", "fresh"),
            ("--warning-color", "tally"),
            ("--error-color", "hangup"),
        ] {
            let c = palette::color(name);
            let expected = format!("{property}: #{:02x}{:02x}{:02x};", c.r, c.g, c.b);
            assert!(
                css.contains(&expected),
                "{property} should be @{name} ({expected}), but the generated \
                 block is:\n{css}",
            );
        }
    }

    /// The same check for the half of the stylesheet this file cannot hold.
    ///
    /// `appearance::content_css` builds its rules at runtime, so the test below
    /// never sees them: it reads `style.css`, and the settings-dependent fills are
    /// by definition not in there. That leaves the app's most fragile CSS as its
    /// only unchecked CSS - a stray character in a `format!` reaches a provider
    /// that drops the declaration and carries on, and the visible result is a
    /// surface that silently stops being translucent.
    ///
    /// Run at a non-default opacity so every rule is exercised with an `alpha()`
    /// wrapped around it rather than at the value where one might be skipped.
    #[test]
    fn the_dynamic_rules_parse_without_errors() {
        gtk_test(|| {
            appearance::set(appearance::Appearance {
                window_opacity: 0.85,
                pane_opacity: 0.6,
                gap: 6,
                font: String::new(),
            });

            let errors = Rc::new(RefCell::new(Vec::new()));
            let provider = CssProvider::new();
            let sink = errors.clone();
            provider.connect_parsing_error(move |_, section, error| {
                sink.borrow_mut()
                    .push(format!("{}: {error}", section.to_str()));
            });
            let css = appearance::content_css(1.25);
            provider.load_from_string(&css);

            let errors = errors.borrow();
            assert!(
                errors.is_empty(),
                "the dynamic rules have {} parse error(s) GTK would have silently \
                 ignored:\n{}\nthe rules were:\n{css}",
                errors.len(),
                errors.join("\n"),
            );
        });
    }

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

    /// And the same again for the palette aliases, which are the one stylesheet
    /// loaded above the user's own CSS - a parse error here reverts a name to
    /// whatever a theming tool last wrote, which is how the rack went solid.
    #[test]
    fn the_adwaita_colours_parse_without_errors() {
        gtk_test(|| {
            let errors = Rc::new(RefCell::new(Vec::new()));
            let provider = CssProvider::new();
            let sink = errors.clone();
            provider.connect_parsing_error(move |_, section, error| {
                sink.borrow_mut()
                    .push(format!("{}: {error}", section.to_str()));
            });
            provider.load_from_string(include_str!("adwaita-colors.css"));

            let errors = errors.borrow();
            assert!(
                errors.is_empty(),
                "adwaita-colors.css has {} parse error(s) GTK would have silently \
                 ignored:\n{}",
                errors.len(),
                errors.join("\n"),
            );
        });
    }

    /// The names a theming tool's generated gtk.css redefines - the ones that
    /// put an opaque slab behind the rack in 2026-08 - must be (re)defined in
    /// `adwaita-colors.css`, the file that loads above user CSS, and must not
    /// drift back into `style.css`, which loads below it and would lose the
    /// fight silently. String checks, so they hold on a headless CI too.
    #[test]
    fn the_riceable_names_are_defined_above_user_css() {
        let armoured = include_str!("adwaita-colors.css");
        let below = include_str!("style.css");
        for name in [
            "window_bg_color",
            "headerbar_bg_color",
            "headerbar_backdrop_color",
            "sidebar_bg_color",
            "sidebar_backdrop_color",
            "secondary_sidebar_bg_color",
            "dialog_bg_color",
            "popover_bg_color",
            "shade_color",
            "sidebar_shade_color",
        ] {
            let define = format!("@define-color {name} ");
            assert!(
                armoured.contains(&define),
                "{name} is not defined in adwaita-colors.css, so a user gtk.css \
                 defines it instead and paints whatever it likes with it",
            );
            assert!(
                !below.contains(&define),
                "{name} is defined in style.css, which loads below user CSS - \
                 the definition must live in adwaita-colors.css to win",
            );
        }
        for name in ["sidebar_bg_color", "sidebar_backdrop_color"] {
            assert!(
                armoured.contains(&format!("@define-color {name} transparent;")),
                "{name} must be transparent - any fill here sits behind the \
                 rack's glass and composites it toward solid",
            );
        }
        assert!(
            below.contains(".sidebar-pane"),
            "style.css has lost the rule silencing the split view's own region \
             fill - the second lock on the door adwaita-colors.css guards",
        );
    }

    /// The static `.sidebar` fallback is what renders whenever the dynamic
    /// provider has nothing to give - the frame before it loads, or every
    /// frame after a parse error made GTK drop the glass rule without a word.
    /// It must fail *to glass*, because an opaque rack is indistinguishable
    /// from the palette working, and that disguise once kept a real bug
    /// invisible for weeks.
    #[test]
    fn the_static_rack_fallback_is_glass() {
        let css = include_str!("style.css");
        assert!(
            css.contains(".sidebar {\n  background-color: alpha(@rack,"),
            "the .sidebar fallback in style.css is not glass - a dropped \
             dynamic rule now degrades to a solid rack instead of a default one",
        );
    }
}
