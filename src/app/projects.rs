//! Opening, closing, switching and ordering projects.
//!
//! A project is one folder, one `Tiler` of agent panes, one sidebar strip and
//! one stack page, and the whole reason `model` exists is that keeping those
//! four in step by hand is where the old `groups.rs` spent its bugs. Order and
//! which-one-is-active live in `ProjectStore`; everything here reads from it
//! rather than from the widgets, so there is only ever one answer to "what comes
//! after this one".
//!
//! The widgets are still a parallel `Vec<ProjectView>`, looked up by id. That is
//! deliberate: they are GTK objects and the store is deliberately GTK-free, which
//! is what lets the ordering rules be tested without a display.

use std::rc::Rc;

use adw::prelude::*;
use gtk4::gio;

use super::{ATTENTION_CLASS, App, ProjectView};
use crate::agent::Kind;
use crate::model::{ProjectId, Removal};
use crate::pane::folder_name;
use crate::tiler::Tiler;

impl App {
    /// Registers a project: a `Tiler`, a stack page and a sidebar row, switched
    /// to immediately.
    pub(super) fn add_project(&self, path: &str, name: String, icon: &str) -> Tiler {
        let id = self.0.store.borrow_mut().add(path, name.clone(), icon);
        let page = id.as_name();

        let tiler = Tiler::new(path.to_string());
        tiler.set_font_scale(self.0.font_scale.get());

        let project_view = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .transition_duration(120)
            .build();
        project_view.add_named(&self.build_empty_state(), Some("empty"));
        project_view.add_named(&tiler, Some("panes"));
        self.0.stack.add_named(&project_view, Some(&page));

        let view_weak = project_view.downgrade();
        let weak = Rc::downgrade(&self.0);
        tiler.set_pane_count_callback(move |count| {
            if let Some(view) = view_weak.upgrade() {
                view.set_visible_child_name(if count == 0 { "empty" } else { "panes" });
            }
            // Only the visible project's count can speak for the header bar; a
            // background group closing its last pane must not dim the toggles
            // over the group you are actually looking at.
            if let Some(inner) = weak.upgrade() {
                let app = App(inner);
                app.refresh_row_tally(id);
                if app.0.store.borrow().active() == Some(id) {
                    app.sync_mode_sensitivity(count);
                    // What the next project gets opened with - see
                    // `Inner::last_agent_count`. The *agent* tally rather than
                    // the pane count it used to be: "I work with two agents
                    // and had a file open" is a preference for two agents, not
                    // three.
                    let agents = app.tiler_for(id).map_or(0, |t| t.agent_tally().total());
                    if agents > 0 {
                        app.0.last_agent_count.set(agents);
                    }
                }
            }
        });

        // Weak, all three: the `Tiler` these are hung on is itself owned (via
        // `tilers`) by the `Inner` they would otherwise hold a strong reference
        // back to.
        let weak = Rc::downgrade(&self.0);
        let name_for_title = name.clone();
        tiler.set_title_callback(move |pane_title| {
            let Some(inner) = weak.upgrade() else { return };
            if inner.store.borrow().active() != Some(id) {
                return;
            }
            inner.title.set_title(&name_for_title);
            *inner.pane_title.borrow_mut() = pane_title.to_string();
            App(inner.clone()).refresh_subtitle();
            // The header bar shows the project, because that's what you need
            // while working. The *window* title still leads with the app - it's
            // what the taskbar and the alt-tab switcher show, where "Getting
            // Started" on its own names nothing recognisable. It also carries
            // the branch marker for dev builds, which used to live in the WM
            // titlebar that client-side decorations have now replaced.
            let base = &inner.base_title;
            inner.window.set_title(Some(&if pane_title.is_empty() {
                format!("{base} \u{2014} {name_for_title}")
            } else {
                format!("{base} \u{2014} {name_for_title} \u{b7} {pane_title}")
            }));
        });

        let weak = Rc::downgrade(&self.0);
        tiler.set_attention_callback(move || {
            if let Some(inner) = weak.upgrade() {
                App(inner).flash_row(id);
            }
        });

        let weak = Rc::downgrade(&self.0);
        tiler.set_mode_callback(move |mode| {
            let Some(inner) = weak.upgrade() else { return };
            let app = App(inner);
            // Mirrored into the store so it survives a project switch, and
            // pushed at the header bar so the toggles show where the keyboard
            // just put us.
            if let Some(project) = app.0.store.borrow_mut().get_mut(id) {
                project.mode = mode;
            }
            if app.0.store.borrow().active() == Some(id) {
                app.sync_mode_buttons(mode);
            }
            app.schedule_save();
        });

        let weak = Rc::downgrade(&self.0);
        tiler.set_layout_callback(move |state| {
            let Some(inner) = weak.upgrade() else { return };
            // Nothing on screen reads these back yet - they're mirrored so the
            // model is a complete account of how a group is arranged rather
            // than a partial one, which is what phase 3 serialises. Mirroring
            // as it changes, rather than gathering it at save time, is what
            // keeps that true for a group that isn't the visible one.
            if let Some(project) = inner.store.borrow_mut().get_mut(id) {
                project.master_ratio = state.master_ratio;
                project.master_count = state.master_count;
                project.focus = state.focus;
            }
            App(inner).schedule_save();
        });

        let (row, agents) = self.build_row(id);
        self.0.views.borrow_mut().push(ProjectView {
            id,
            tiler: tiler.clone(),
            row: row.clone(),
            agents,
            view: project_view,
        });
        self.0.list.append(&row);
        self.0.list.select_row(Some(&row));
        self.show_project(id);
        self.schedule_save();
        tiler
    }

    /// Asks (via a folder picker, then an agent picker) what to open, then
    /// creates it, switches to it, and starts however many agents you last
    /// worked with.
    ///
    /// Cancelling either dialog creates nothing at all, rather than falling
    /// back to a project nobody asked for.
    pub fn new_project(&self) {
        let dialog = gtk4::FileDialog::builder()
            .title("Open project as a new group")
            .accept_label("Open")
            .modal(true)
            .initial_folder(&gio::File::for_path(&*self.0.last_dir.borrow()))
            .build();

        let this = self.clone();
        let window = self.0.window.clone();
        dialog.select_folder(Some(&window), None::<&gio::Cancellable>, move |result| {
            let Some(dir) = result.ok().and_then(|file| file.path()) else {
                return;
            };
            let dir = dir.to_string_lossy().into_owned();
            this.0.last_dir.replace(dir.clone());
            this.ask_which_agent(dir);
        });
    }

    /// The second half of opening a project: which agent it is for.
    ///
    /// A dialog, which this app spent a release getting rid of - the old "how
    /// many agents?" modal was asked every time and answered the same way
    /// almost every time, and that is a dialog costing a click forever in
    /// exchange for earning its place once. What makes this one different is
    /// that its answer genuinely varies: a project is usually a project you
    /// work on with one agent or the other, and which one is not something the
    /// app can infer from a folder.
    ///
    /// It is still built to cost as little as possible. The default response is
    /// whichever agent you chose last, so Enter takes the common path, Escape
    /// cancels the whole thing, and anyone who always answers the same way is
    /// paying one keystroke rather than one decision.
    ///
    /// Built from `Kind::ALL` rather than from two hardcoded buttons, so an
    /// agent added to `agent` turns up here rather than being quietly
    /// unreachable from the one screen that opens projects.
    fn ask_which_agent(&self, dir: String) {
        let ask = adw::AlertDialog::new(
            Some("Which agent?"),
            Some(&format!(
                "{} will open with it.",
                folder_name(&dir)
            )),
        );

        let responses: Vec<(String, String)> = Kind::ALL
            .iter()
            .map(|kind| (kind.label().to_string(), kind.label().to_string()))
            .collect();
        for (id, label) in &responses {
            ask.add_response(id, label);
        }
        ask.add_response("cancel", "Cancel");
        ask.set_close_response("cancel");

        let remembered = self.0.last_agent_kind.get();
        ask.set_response_appearance(remembered.label(), adw::ResponseAppearance::Suggested);
        ask.set_default_response(Some(remembered.label()));

        let this = self.clone();
        ask.connect_response(None, move |_, response| {
            // Cancel, Escape, and anything unrecognised all mean the same
            // thing: no project. Falling back to a default here would create a
            // group out of a dialog somebody dismissed.
            let Some(kind) = Kind::parse(response) else {
                return;
            };
            this.0.last_agent_kind.set(kind);
            this.open_project(dir.clone(), kind);
        });
        ask.present(Some(&self.0.window));
    }

    /// Opens `dir` as a new group running `kind`, and starts it with as many
    /// agents as the last project you worked in ended up running.
    ///
    /// *How many* is remembered rather than asked, and that half is unchanged:
    /// this replaced a modal offering buttons for 1-4, asked every single time
    /// a project was opened and answered the same way almost every time. The
    /// count you use is a habit, so it is learned, and a project that wants a
    /// different number is one spawn away (the + button, or `Super+Alt+Return`'s
    /// sibling `spawn_pane_here`).
    ///
    /// *Which agent* is asked, because that one is not a habit - see
    /// `ask_which_agent`. Setting it as the group's default before spawning is
    /// what makes the answer stick: every agent this opens with is that kind,
    /// and so is every later one the group's own + starts.
    pub(super) fn open_project(&self, dir: String, kind: Kind) {
        let count = self.0.last_agent_count.get().max(1);
        let tiler = self.add_project(&dir, folder_name(&dir), "folder-symbolic");
        tiler.set_default_kind(kind);
        for _ in 0..count {
            tiler.spawn_pane_here();
        }
    }

    /// Closes every pane in a project and removes it from the stack and the
    /// sidebar. Refuses to remove the last one.
    pub(super) fn remove_project(&self, id: ProjectId) {
        let outcome = self.0.store.borrow_mut().remove(id);
        let Removal::Removed { fallback } = outcome else {
            return;
        };

        let removed = {
            let mut views = self.0.views.borrow_mut();
            let Some(pos) = views.iter().position(|v| v.id == id) else {
                return;
            };
            views.remove(pos)
        };
        removed.tiler.close_all_panes();
        self.0.stack.remove(&removed.view);
        self.0.list.remove(&removed.row);
        // The closed project might have been the only one still asking for
        // attention, and it can't answer for itself now that it's gone - which
        // would leave the toggle lit for a project that no longer exists.
        self.refresh_attention();

        if let Some(fallback) = fallback {
            self.select(fallback);
        }
        self.schedule_save();
    }

    /// Opens `file` in an editor pane of the project whose tree it was clicked
    /// in - tiled in with the agents, because that is what this window is, and
    /// a dialog floating over the workspace covered the very agents whose work
    /// you opened the file to check on. Switching to the project is part of
    /// the same promise: the pane appears in that project's grid, and a click
    /// whose result lands behind the sidebar is a click that did nothing.
    ///
    /// The editor's refusals (not text, too big, unreadable) arrive as the
    /// `Err` sentence and land as a toast.
    pub(super) fn edit_file(&self, id: ProjectId, file: &std::path::Path) {
        let Some(tiler) = self.tiler_for(id) else {
            return;
        };
        self.select(id);
        if let Err(why) = tiler.open_editor_pane(file) {
            self.toast(&why);
        }
    }

    /// Hands an agent's report to whichever project owns the pane that sent it.
    ///
    /// Asked of every group rather than routed, because a pane id is unique
    /// across the window and a group is the only thing that knows which panes it
    /// holds. A message for a pane that has since been closed is claimed by
    /// nobody, which is the correct outcome and needs no special case.
    pub(super) fn on_agent_event(&self, message: &crate::ipc::Message) {
        let hit = {
            let views = self.0.views.borrow();
            views
                .iter()
                .find(|v| v.tiler.apply_agent_event(message))
                .map(|v| v.id)
        };
        if let Some(id) = hit {
            self.refresh_row_tally(id);
        }
    }

    /// Makes a project the visible one, and answers its call for attention - the
    /// user has now seen whatever the agent rang about. This is the single choke
    /// point for that: every way of switching projects arrives here.
    pub(super) fn show_project(&self, id: ProjectId) {
        self.0.store.borrow_mut().set_active(id);
        self.0.stack.set_visible_child_name(&id.as_name());

        if let Some(tiler) = self.tiler_for(id) {
            // Neither of these happens on its own while a `Tiler` sits hidden in
            // a background project.
            tiler.on_shown();
            self.sync_mode_buttons(tiler.mode());
            self.sync_mode_sensitivity(tiler.pane_count());
            self.sync_broadcast_button(tiler.broadcast());
        }
        if let Some(row) = self.row_for(id) {
            row.remove_css_class(ATTENTION_CLASS);
        }
        self.refresh_attention();

        // The title belonged to the project we just left. Clearing it before
        // refreshing means the header falls back to the new project's tally
        // rather than showing what the *previous* project's focused agent was
        // doing until that project happens to retitle itself.
        self.0.pane_title.borrow_mut().clear();
        self.refresh_subtitle();

        // On a narrow window the sidebar is covering the panes, so having picked
        // a project, get out of the way of it.
        if self.0.split.is_collapsed() {
            self.0.split.set_show_sidebar(false);
        }
        self.schedule_save();
    }

    /// Selects a row, which switches the stack through `connect_row_selected`.
    pub(super) fn select(&self, id: ProjectId) {
        if let Some(row) = self.row_for(id) {
            self.0.list.select_row(Some(&row));
        }
    }

    pub(super) fn tiler_for(&self, id: ProjectId) -> Option<Tiler> {
        self.0
            .views
            .borrow()
            .iter()
            .find(|v| v.id == id)
            .map(|v| v.tiler.clone())
    }

    pub(super) fn row_for(&self, id: ProjectId) -> Option<gtk4::ListBoxRow> {
        self.0
            .views
            .borrow()
            .iter()
            .find(|v| v.id == id)
            .map(|v| v.row.clone())
    }

    /// The `Tiler` for whichever project is currently visible.
    /// Every open project as (id, name, is-the-open-one), in sidebar order.
    ///
    /// A snapshot rather than a borrow, because the caller is the command
    /// palette: it holds what it reads for as long as the dialog is open, and
    /// activating a row calls straight back into `select`, which takes the same
    /// `RefCell` mutably.
    pub fn project_list(&self) -> Vec<(ProjectId, String, bool)> {
        let store = self.0.store.borrow();
        let active = store.active();
        store
            .iter()
            .map(|project| (project.id, project.name.clone(), Some(project.id) == active))
            .collect()
    }

    /// Switches to `id`.
    ///
    /// Goes through the sidebar's selection rather than straight to
    /// `show_project`, because the row being selected is what everything else
    /// hangs off - the stack page, the header title, the mode buttons and the
    /// attention flag all follow from it. Public because the palette switches
    /// projects too, and it must switch them the same way a click does.
    pub fn switch_to_project(&self, id: ProjectId) {
        self.select(id);
    }

    pub fn active_tiler(&self) -> Option<Tiler> {
        let id = self.0.store.borrow().active()?;
        self.tiler_for(id)
    }

    /// Switches to the next (`1`) or previous (`-1`) project, wrapping around.
    pub fn cycle_project(&self, delta: i32) {
        let next = self.0.store.borrow_mut().cycle(delta);
        if let Some(id) = next {
            self.select(id);
        }
    }

    /// Moves the visible project one place up (`-1`) or down (`1`) - the
    /// keyboard's way in to what a drag does with the mouse.
    pub fn move_active_project(&self, delta: i32) {
        self.0.store.borrow_mut().move_active(delta);
        self.0.list.invalidate_sort();
        self.refresh_rail();
    }
}
