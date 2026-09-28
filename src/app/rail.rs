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
//! Rebuilt from the store when the projects change, and updated in place when
//! only their state does - see `refresh_rail` for why the difference turned out
//! to be visible. Every mutation of the project list already funnels through a
//! handful of `App` methods, and each of those ends with one `refresh_rail`
//! call.

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
    ///
    /// In place when it can be. This runs on every agent state change - twice
    /// per tool call, per agent - and it used to tear down and rebuild every
    /// glyph each time. That was more than waste: a rebuilt glyph is a new
    /// widget, and a new widget wearing `.needs-attention` starts its pulse
    /// from the top. So a project flagged while its agents kept working never
    /// finished pulsing - it strobed, for as long as they worked. Now the
    /// buttons are rebuilt only when the projects themselves change, and a
    /// class the glyph already wears is left exactly as it is.
    pub(super) fn refresh_rail(&self) {
        let store = self.0.store.borrow();
        let active = store.active();
        // The drawer's heading count, written here because this is the one call
        // every change to the project list already ends with - see
        // `Inner::sidebar_count`.
        let count = store.iter().count().to_string();
        if self.0.sidebar_count.label() != count {
            self.0.sidebar_count.set_label(&count);
        }

        let wanted: Vec<(crate::model::ProjectId, String, String)> = store
            .iter()
            .map(|project| {
                // The welcome entry keeps its info glyph - it is a home screen
                // occupying a slot, not a project with an initial worth learning.
                let face = if project.icon == crate::model::WELCOME_ICON {
                    project.icon.clone()
                } else {
                    project
                        .name
                        .chars()
                        .next()
                        .map(|c| c.to_uppercase().to_string())
                        .unwrap_or_else(|| "?".to_string())
                };
                (project.id, face, project.name.clone())
            })
            .collect();
        drop(store);

        let same_glyphs = {
            let built = self.0.rail_buttons.borrow();
            built.len() == wanted.len()
                && built
                    .iter()
                    .zip(&wanted)
                    .all(|(glyph, (want_id, want_face, _))| glyph.id == *want_id && glyph.face == *want_face)
        };
        if !same_glyphs {
            self.rebuild_rail(&wanted);
        }

        for glyph in self.0.rail_buttons.borrow().iter() {
            let id = glyph.id;
            let name = wanted
                .iter()
                .find(|(want, _, _)| *want == id)
                .map(|(_, _, name)| name.as_str())
                .unwrap_or_default();
            let tally = self.tiler_for(id).map(|t| t.agent_tally()).unwrap_or_default();
            let tooltip = if tally.total() == 0 {
                name.to_string()
            } else {
                format!("{name} \u{2014} {}", self.agent_words(&tally))
            };
            let button = &glyph.button;
            if button.tooltip_text().as_deref() != Some(tooltip.as_str()) {
                button.set_tooltip_text(Some(&tooltip));
            }
            super::set_class(button, LIT_CLASS, active == Some(id));
            let wants = self
                .row_for(id)
                .is_some_and(|row| row.has_css_class(ATTENTION_CLASS));
            super::set_class(button, ATTENTION_CLASS, wants);

            // The badges: how many agents in it are asking - the number worth
            // reading from across the window - and whether any is working.
            glyph.asking.set_visible(tally.waiting > 0);
            let asking = tally.waiting.to_string();
            if glyph.asking.label() != asking {
                glyph.asking.set_label(&asking);
            }
            glyph.working.set_visible(tally.working > 0 && tally.waiting == 0);
        }
    }

    /// Makes one glyph button per project, in order - for when the projects
    /// themselves have changed.
    fn rebuild_rail(&self, wanted: &[(crate::model::ProjectId, String, String)]) {
        let glyphs = &self.0.rail_glyphs;
        while let Some(child) = glyphs.first_child() {
            glyphs.remove(&child);
        }
        let mut built = Vec::with_capacity(wanted.len());
        for (id, face_key, _) in wanted {
            let id = *id;
            let face: gtk4::Widget = if face_key == crate::model::WELCOME_ICON {
                gtk4::Image::builder()
                    .icon_name(face_key)
                    .css_classes(["rail-glyph-face"])
                    .build()
                    .upcast()
            } else {
                gtk4::Label::builder()
                    .label(face_key)
                    .css_classes(["rail-glyph-face"])
                    .build()
                    .upcast()
            };
            // The face, with two badges pinned to its corners: a count of the
            // agents asking, top right, and a pip while any is working, bottom
            // right. An overlay rather than a row, because a glyph's size is
            // the rail's width and nothing may widen it.
            let asking = gtk4::Label::builder()
                .css_classes(["rail-badge"])
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::Start)
                .visible(false)
                .can_target(false)
                .build();
            let working = gtk4::Box::builder()
                .css_classes(["rail-pip"])
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::End)
                .visible(false)
                .can_target(false)
                .build();
            let overlay = gtk4::Overlay::builder().child(&face).build();
            overlay.add_overlay(&asking);
            overlay.add_overlay(&working);
            let button = gtk4::Button::builder()
                .child(&overlay)
                .css_classes(["rail-glyph"])
                .can_focus(false)
                .build();

            // The active glyph is where you already are, so its click means
            // the other thing you'd want from the rail: the drawer.
            let weak = std::rc::Rc::downgrade(&self.0);
            button.connect_clicked(move |_| {
                let Some(inner) = weak.upgrade() else { return };
                let this = App(inner);
                if this.0.store.borrow().active() == Some(id) {
                    let shown = this.0.split.shows_sidebar();
                    this.0.split.set_show_sidebar(!shown);
                } else {
                    this.select(id);
                }
            });

            glyphs.append(&button);
            built.push(RailGlyph {
                id,
                face: face_key.clone(),
                button,
                asking,
                working,
            });
        }
        *self.0.rail_buttons.borrow_mut() = built;
    }
}

/// One project's glyph on the rail, as built - kept so a refresh can change
/// what it says without rebuilding it (see `refresh_rail`).
pub(super) struct RailGlyph {
    id: crate::model::ProjectId,
    /// The initial or icon it was built showing.
    face: String,
    button: gtk4::Button,
    /// How many agents are asking, pinned top right.
    asking: gtk4::Label,
    /// Lit while an agent is working, pinned bottom right.
    working: gtk4::Box,
}
