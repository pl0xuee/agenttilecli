//! The rail: every project, one glyph, for when the drawer is shut.
//!
//! The rack used to be the only place a project existed visually, and the rack
//! is summonable - which meant a narrow window, or a closed sidebar, was a
//! window where your other projects didn't exist at all. The rail answers that
//! and only that: with the drawer shut it is the index, one initial per project
//! plus whether that project wants you.
//!
//! With the drawer *open* it is not on screen at all, because there it had
//! nothing left to say - the drawer lists the same projects, in the same order,
//! with their names spelled out. See `App::new`, which binds the two together.
//!
//! No colour. A glyph carries an initial, a lit ring if it is the project on
//! screen, and amber if an agent in it is waiting - and that is the whole
//! vocabulary. Identity hues were tried here and made a column of seven
//! projects read as a paint chart; colour in this window belongs to state.
//!
//! Rebuilt whole from the store on every change rather than kept in step
//! incrementally - the same trade `tree::rebuild` makes, for the same reason:
//! a dozen small widgets are cheap to remake and impossible to desynchronise.
//! Every mutation of the project list already funnels through a handful of
//! `App` methods, and each of those ends with one `refresh_rail` call.

use adw::prelude::*;

use super::{ATTENTION_CLASS, App};

/// The class the active project's glyph wears. `lit`, like the pane the
/// keyboard is in and the tile rung named for it: one word for "this is where
/// you are" everywhere the app says it.
const LIT_CLASS: &str = "lit";

impl App {
    /// The rail column: glyphs, the add button, and the version dot.
    ///
    /// Wrapped in a `WindowHandle` because the rail is the one strip of chrome
    /// with almost nothing clickable on it, which is exactly what a window
    /// with client-side decorations wants to be dragged by.
    pub(super) fn build_rail(&self) -> gtk4::WindowHandle {
        // No visible scrollbar: a bar inside a 56px strip eats a third of it.
        // The wheel still scrolls if someone opens more projects than the
        // window is tall, and the drawer lists every project regardless.
        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::External)
            .child(&self.0.rail_glyphs)
            .vexpand(true)
            .build();

        let add = gtk4::Button::builder()
            .icon_name("list-add-symbolic")
            .css_classes(["rail-add"])
            .can_focus(false)
            .tooltip_text("Open a new project as a new group (Super+Alt+Return)")
            .build();
        let this = self.clone();
        add.connect_clicked(move |_| this.new_project());

        let version = gtk4::Label::builder()
            .label("\u{25cf}")
            .css_classes(["rail-version-dot"])
            .tooltip_text(format!("AgentTileCLI {}", crate::update::version()))
            .build();

        // The two controls that are not projects, fenced off from the column
        // that is. In a strip whose entire vocabulary is "one chip per project",
        // an add button and a version dot sitting flush under the last chip
        // read as two more projects - one of them blank and one of them a full
        // stop. The rule across the top of this box is what says they are the
        // rail's own furniture; see `.rail-foot` in style.css.
        let foot = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(2)
            .css_classes(["rail-foot"])
            .build();
        foot.append(&add);
        foot.append(&version);

        let column = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(6)
            .css_classes(["rail"])
            .build();
        column.append(&scroll);
        column.append(&foot);

        gtk4::WindowHandle::builder().child(&column).build()
    }

    /// Redraws the rail from the store: order, who is lit, and who wants you.
    ///
    /// Attention is read back off the drawer rows rather than tracked twice -
    /// `flash_row` owns that state, the row wears it, and the rail mirrors the
    /// row the same way the sidebar toggle does.
    pub(super) fn refresh_rail(&self) {
        let glyphs = &self.0.rail_glyphs;
        while let Some(child) = glyphs.first_child() {
            glyphs.remove(&child);
        }

        let store = self.0.store.borrow();
        let active = store.active();
        // The drawer's heading count, written here because this is the one call
        // every change to the project list already ends with - see
        // `Inner::sidebar_count`.
        self.0.sidebar_count.set_label(&store.iter().count().to_string());
        for project in store.iter() {
            let id = project.id;

            // The welcome entry keeps its info glyph - it is a home screen
            // occupying a slot, not a project with an initial worth learning.
            let face: gtk4::Widget = if project.icon == crate::model::WELCOME_ICON {
                gtk4::Image::builder()
                    .icon_name(&*project.icon)
                    .css_classes(["rail-glyph-face"])
                    .build()
                    .upcast()
            } else {
                let initial = project
                    .name
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "?".to_string());
                gtk4::Label::builder()
                    .label(&initial)
                    .css_classes(["rail-glyph-face"])
                    .build()
                    .upcast()
            };

            let agents = self.tiler_for(id).map_or(0, |t| t.agent_tally().total());
            let tooltip = match agents {
                0 => project.name.clone(),
                1 => format!("{} \u{2014} 1 agent", project.name),
                n => format!("{} \u{2014} {n} agents", project.name),
            };

            let button = gtk4::Button::builder()
                .child(&face)
                .css_classes(["rail-glyph"])
                .can_focus(false)
                .tooltip_text(&tooltip)
                .build();
            if active == Some(id) {
                button.add_css_class(LIT_CLASS);
            }
            if self
                .row_for(id)
                .is_some_and(|row| row.has_css_class(ATTENTION_CLASS))
            {
                button.add_css_class(ATTENTION_CLASS);
            }

            // The active glyph is where you already are, so its click means
            // the other thing you'd want from the rail: the drawer.
            let this = self.clone();
            button.connect_clicked(move |_| {
                if this.0.store.borrow().active() == Some(id) {
                    let shown = this.0.split.shows_sidebar();
                    this.0.split.set_show_sidebar(!shown);
                } else {
                    this.select(id);
                }
            });

            glyphs.append(&button);
        }
    }
}
