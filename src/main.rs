mod keybindings;
mod layout;
mod pane;
mod tiler;

use gtk4::prelude::*;
use gtk4::{gdk, glib, Application, ApplicationWindow, CssProvider};

use tiler::Tiler;

const APP_ID: &str = "dev.agenttilecli.AgentTileCli";

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
    gtk4::Window::set_default_icon_name("agenttilecli");

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/".to_string());

    let tiler = Tiler::new(cwd);

    let new_agent_button = gtk4::Button::builder()
        .icon_name("tab-new-symbolic")
        .css_classes(["circular", "add-pane"])
        .can_focus(false)
        .tooltip_text("Spawn a new agent in the current project")
        .build();
    new_agent_button.connect_clicked(glib::clone!(
        #[strong]
        tiler,
        move |_| tiler.spawn_pane_here()
    ));

    let add_button = gtk4::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["circular", "add-pane"])
        .can_focus(false)
        .tooltip_text("Open a new project in a new pane (Super+Alt+Return)")
        .build();
    add_button.connect_clicked(glib::clone!(
        #[strong]
        tiler,
        move |_| tiler.spawn_pane()
    ));

    let corner_buttons = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(10)
        .halign(gtk4::Align::End)
        .valign(gtk4::Align::End)
        .margin_end(20)
        .margin_bottom(20)
        .build();
    corner_buttons.append(&new_agent_button);
    corner_buttons.append(&add_button);

    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&tiler));
    overlay.add_overlay(&corner_buttons);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("AgentTileCLI")
        .default_width(1280)
        .default_height(800)
        .child(&overlay)
        .build();

    let window_weak = window.downgrade();
    tiler.set_title_callback(move |title| {
        if let Some(window) = window_weak.upgrade() {
            let title = if title.is_empty() {
                "AgentTileCLI".to_string()
            } else {
                format!("AgentTileCLI — {title}")
            };
            window.set_title(Some(&title));
        }
    });

    keybindings::install(&window, &tiler);

    tiler.toggle_help();
    window.present();
}
