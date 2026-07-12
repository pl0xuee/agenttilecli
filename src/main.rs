mod keybindings;
mod layout;
mod pane;
mod tiler;

use gtk4::prelude::*;
use gtk4::{gdk, glib, Application, ApplicationWindow, CssProvider};

use tiler::Tiler;

const APP_ID: &str = "dev.aitile.Aitile";

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| load_css());
    app.connect_activate(build_window);
    app.run()
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
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/".to_string());

    let tiler = Tiler::new(cwd);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("aitile")
        .default_width(1280)
        .default_height(800)
        .child(&tiler)
        .build();

    let window_weak = window.downgrade();
    tiler.set_title_callback(move |title| {
        if let Some(window) = window_weak.upgrade() {
            let title = if title.is_empty() {
                "aitile".to_string()
            } else {
                format!("aitile — {title}")
            };
            window.set_title(Some(&title));
        }
    });

    keybindings::install(&window, &tiler);

    tiler.spawn_pane();
    tiler.toggle_help();
    window.present();
}
