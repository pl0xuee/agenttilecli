use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::gio;
use gtk4::prelude::*;

use crate::pane::folder_name;
use crate::tiler::Tiler;

/// How much each font-size keybinding press changes the UI's text scale - a
/// multiplier applied both to every pane's VTE `font-scale` and, via a
/// dynamic `window { font-size: {scale}em; }` CSS rule, to every chrome
/// element sized in `em` (sidebar, floating buttons, pane borders/labels) -
/// so the whole program's text and the app's own controls grow together
/// instead of only the terminal contents.
const FONT_SCALE_STEP: f64 = 0.1;
const FONT_SCALE_MIN: f64 = 0.5;
const FONT_SCALE_MAX: f64 = 3.0;

#[derive(Clone)]
struct GroupEntry {
    /// The `Stack` page name for this group's `Tiler`, and the
    /// `widget-name` of its sidebar row - the thread linking the two so a
    /// row click or a stack switch can look up the other side.
    id: String,
    tiler: Tiler,
    row: gtk4::ListBoxRow,
}

struct GroupsInner {
    root: gtk4::Box,
    stack: gtk4::Stack,
    sidebar_list: gtk4::ListBox,
    revealer: gtk4::Revealer,
    entries: RefCell<Vec<GroupEntry>>,
    next_id: Cell<u32>,
    /// The last folder a "new group" pick landed on, so the picker reopens
    /// pre-filled with it - same courtesy `Tiler::spawn_pane` used to offer
    /// for panes before groups existed.
    last_dir: RefCell<String>,
    /// Shared with every group's own `Tiler::set_title_callback` closure
    /// (each of which only forwards through it while its group is the
    /// visible one) - set once via `Groups::set_title_callback`.
    window_title_cb: Rc<RefCell<Option<Box<dyn Fn(&str)>>>>,
    /// The global text-size multiplier - shared across every group so
    /// switching groups never shows a different zoom level, and applied to
    /// the chrome via `css_provider` alongside every pane's VTE terminal.
    font_scale: Cell<f64>,
    /// Holds just the one dynamic `window { font-size: ... }` rule that
    /// drives chrome scaling (see `FONT_SCALE_STEP`'s doc comment) -
    /// reloaded in place on every scale change rather than recreated, so it
    /// keeps sitting at the priority it was added to the display with.
    css_provider: gtk4::CssProvider,
}

/// A hamburger-toggled sidebar of project groups, each holding its own
/// `Tiler` (and therefore its own independent set of agent panes, layout
/// mode, and focus). Exactly one group's `Tiler` is visible at a time
/// (backed by a `Stack`); the others keep running in the background -
/// closing/hiding a group's widget doesn't touch its panes' processes.
#[derive(Clone)]
pub struct Groups(Rc<GroupsInner>);

impl Groups {
    /// Builds the sidebar/stack scaffold and creates the first group from
    /// `initial_cwd` (the app's own launch directory). Does *not* toggle the
    /// help pane on it - callers that want that (as `main.rs` does, for
    /// parity with the pre-groups startup sequence) call
    /// `active_tiler().unwrap().toggle_help()` themselves, after wiring up
    /// `set_title_callback` so the resulting title-change actually lands
    /// somewhere.
    pub fn new(initial_cwd: &str) -> Self {
        let stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::None)
            .hexpand(true)
            .vexpand(true)
            .build();

        let sidebar_list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::Single)
            .css_classes(["sidebar-list"])
            .build();

        let header_label = gtk4::Label::builder()
            .label("Projects")
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .css_classes(["sidebar-header-label"])
            .build();
        let new_group_button = gtk4::Button::builder()
            .icon_name("list-add-symbolic")
            .css_classes(["flat", "circular"])
            .can_focus(false)
            .tooltip_text("Open a new project as a new group")
            .build();
        let header = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .css_classes(["sidebar-header"])
            .build();
        header.append(&header_label);
        header.append(&new_group_button);

        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&sidebar_list)
            .build();

        let sidebar_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .css_classes(["sidebar"])
            .build();
        sidebar_box.append(&header);
        sidebar_box.append(&scrolled);

        // `hexpand` explicitly pinned `false`: without it, the row label's
        // own `hexpand(true)` below (needed to push each row's close button
        // to the right edge) propagates all the way up through the
        // otherwise-hexpand-silent ancestors between it and here, making
        // the Revealer itself computed-hexpand and so claim equal leftover
        // width against the Stack - even while collapsed.
        let revealer = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideRight)
            .transition_duration(180)
            .reveal_child(false)
            .hexpand(false)
            .child(&sidebar_box)
            .build();

        // The sidebar-toggle and new-agent buttons float over the *content*
        // overlay (just the stack), not the whole root - overlaying them on
        // the root instead would pin them to the window's own top-left/
        // bottom-right corners, so opening the sidebar would slide it out
        // right underneath the hamburger button and cover its own header.
        // Anchoring to the stack's overlay instead means they shift right
        // along with the stack as the revealer claims space.
        let hamburger_button = gtk4::Button::builder()
            .icon_name("open-menu-symbolic")
            .css_classes(["circular", "add-pane", "floating-top-left"])
            .can_focus(false)
            .halign(gtk4::Align::Start)
            .valign(gtk4::Align::Start)
            .tooltip_text("Toggle the project sidebar (Super+Alt+g)")
            .build();
        let new_agent_button = gtk4::Button::builder()
            .icon_name("tab-new-symbolic")
            .css_classes(["circular", "add-pane", "floating-bottom-right"])
            .can_focus(false)
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::End)
            .tooltip_text("Spawn a new agent in the current project")
            .build();

        let content = gtk4::Overlay::new();
        content.set_child(Some(&stack));
        content.add_overlay(&new_agent_button);
        content.add_overlay(&hamburger_button);

        let root = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .build();
        root.append(&revealer);
        root.append(&content);

        let css_provider = gtk4::CssProvider::new();
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &css_provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }

        let this = Groups(Rc::new(GroupsInner {
            root,
            stack,
            sidebar_list,
            revealer,
            entries: RefCell::new(Vec::new()),
            next_id: Cell::new(0),
            last_dir: RefCell::new(initial_cwd.to_string()),
            window_title_cb: Rc::new(RefCell::new(None)),
            font_scale: Cell::new(1.0),
            css_provider,
        }));

        // A row click (or arrow-key navigation within the sidebar) switches
        // the stack; a stack switch (from here, from `cycle_group`, or from
        // the initial `add_group`) re-focuses and re-titles the newly
        // visible group.
        let this_weak = Rc::downgrade(&this.0);
        this.0.sidebar_list.connect_row_selected(move |_, row| {
            let (Some(this), Some(row)) = (this_weak.upgrade(), row) else {
                return;
            };
            let name = row.widget_name();
            if !name.is_empty() {
                this.stack.set_visible_child_name(&name);
            }
        });

        this.0.stack.connect_visible_child_notify(|stack| {
            if let Some(tiler) = stack.visible_child().and_then(|w| w.downcast::<Tiler>().ok()) {
                tiler.on_shown();
            }
        });

        let this_clone = this.clone();
        new_group_button.connect_clicked(move |_| this_clone.new_group());

        let this_clone = this.clone();
        hamburger_button.connect_clicked(move |_| this_clone.toggle_sidebar());

        let this_clone = this.clone();
        new_agent_button.connect_clicked(move |_| {
            if let Some(tiler) = this_clone.active_tiler() {
                tiler.spawn_pane_here();
            }
        });

        // The initial group is the one that opens straight to the help pane
        // (see `add_group_named`'s doc comment) - labeled and iconed to
        // match, rather than after whatever folder the app happened to
        // launch from.
        this.add_group_named(
            initial_cwd,
            "Getting Started".to_string(),
            "dialog-information-symbolic",
        );
        this
    }

    /// The widget to embed in the rest of the UI.
    pub fn widget(&self) -> &gtk4::Box {
        &self.0.root
    }

    pub fn toggle_sidebar(&self) {
        let revealed = self.0.revealer.reveals_child();
        self.0.revealer.set_reveal_child(!revealed);
    }

    /// The `Tiler` for whichever group is currently visible - `None` only
    /// transiently, if ever, since a group always exists once `new` returns.
    pub fn active_tiler(&self) -> Option<Tiler> {
        self.0
            .stack
            .visible_child()
            .and_then(|w| w.downcast::<Tiler>().ok())
    }

    /// Registers the callback invoked with the combined "group name [·
    /// pane-title]" string whenever the *visible* group's title-worthy state
    /// changes (focus move, foreground process title change, or a group
    /// switch) - mirrors `Tiler::set_title_callback`, just scoped across
    /// groups instead of panes.
    pub fn set_title_callback(&self, f: impl Fn(&str) + 'static) {
        *self.0.window_title_cb.borrow_mut() = Some(Box::new(f));
    }

    /// Applies `scale` to every group's panes and to the chrome (sidebar,
    /// floating buttons, pane borders/labels) via the dynamic CSS provider.
    fn set_font_scale(&self, scale: f64) {
        self.0.font_scale.set(scale);
        for entry in self.0.entries.borrow().iter() {
            entry.tiler.set_font_scale(scale);
        }
        self.0
            .css_provider
            .load_from_string(&format!("window {{ font-size: {scale}em; }}"));
    }

    pub fn inc_font_scale(&self) {
        let scale = (self.0.font_scale.get() + FONT_SCALE_STEP).min(FONT_SCALE_MAX);
        self.set_font_scale(scale);
    }

    pub fn dec_font_scale(&self) {
        let scale = (self.0.font_scale.get() - FONT_SCALE_STEP).max(FONT_SCALE_MIN);
        self.set_font_scale(scale);
    }

    pub fn reset_font_scale(&self) {
        self.set_font_scale(1.0);
    }

    /// Asks (via a folder picker) which project to open, then how many
    /// agents to start it with, then creates a new group for it and
    /// switches to it. The folder picker opens pre-filled with the last
    /// directory used (or the app's own launch directory, the very first
    /// time). Cancelling either dialog creates nothing.
    pub fn new_group(&self) {
        let last_dir = self.0.last_dir.borrow().clone();

        let dialog = gtk4::FileDialog::builder()
            .title("Open project as a new group")
            .accept_label("Open")
            .modal(true)
            .initial_folder(&gio::File::for_path(&last_dir))
            .build();

        let this = self.clone();
        let parent = self
            .0
            .root
            .root()
            .and_then(|r| r.downcast::<gtk4::Window>().ok());
        let parent_for_count = parent.clone();
        dialog.select_folder(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
            let Some(dir) = result.ok().and_then(|file| file.path()) else {
                return;
            };
            let dir = dir.to_string_lossy().into_owned();
            this.0.last_dir.replace(dir.clone());

            // Buttons rather than a spinner/entry - every other dialog in
            // this app (including the folder picker just above) is a click
            // or two, never typed input, and 1-4 covers every layout mode
            // (grid, master-stack, monocle) without the picker itself
            // needing scroll or validation. More agents can always be added
            // afterward with `new-agent`.
            //
            // Index 0 is Cancel (and is registered as `cancel_button`, so
            // Escape / closing the dialog reports it too) - so button index
            // *is* the agent count for every non-cancel choice.
            let count_dialog = gtk4::AlertDialog::builder()
                .message("How many agents?")
                .detail(folder_name(&dir))
                .buttons(["Cancel", "1", "2", "3", "4"])
                .cancel_button(0)
                .default_button(1)
                .build();

            let this = this.clone();
            let parent_for_count = parent_for_count.clone();
            count_dialog.choose(
                parent_for_count.as_ref(),
                None::<&gio::Cancellable>,
                move |result| {
                    // Cancelling (button 0, Escape, or the dialog being
                    // dismissed - which reports `Err`) creates nothing at
                    // all, rather than falling back to a group nobody asked
                    // for: backing out here should leave the app exactly as
                    // it was before the folder picker opened.
                    let Ok(count @ 1..=4) = result else {
                        return;
                    };
                    let tiler = this.add_group(&dir);
                    for _ in 0..count {
                        tiler.spawn_pane_here();
                    }
                },
            );
        });
    }

    /// Registers a new group for `cwd`, named after its folder (a fresh
    /// `Tiler`, a stack page, and a sidebar row) and switches to it - see
    /// `add_group_named` for the details `new_group` and `Groups::new` both
    /// build on top of.
    fn add_group(&self, cwd: &str) -> Tiler {
        self.add_group_named(cwd, folder_name(cwd), "folder-symbolic")
    }

    /// Registers a new group for `cwd` under an explicit sidebar label and
    /// icon (a fresh `Tiler`, a stack page, and a sidebar row) and switches
    /// to it, returning the new `Tiler` so the caller can decide whether to
    /// spawn a pane into it - `new_group` always does, but `Groups::new`'s
    /// initial group deliberately doesn't, so startup still shows only the
    /// help pane rather than surprising the user with an agent already
    /// running in whatever directory the app happened to launch from.
    fn add_group_named(&self, cwd: &str, name: String, icon: &str) -> Tiler {
        let id = self.0.next_id.get().to_string();
        self.0.next_id.set(self.0.next_id.get() + 1);

        let tiler = Tiler::new(cwd.to_string());
        tiler.set_font_scale(self.0.font_scale.get());
        self.0.stack.add_named(&tiler, Some(&id));

        let stack_weak = self.0.stack.downgrade();
        let title_cb = self.0.window_title_cb.clone();
        let id_for_cb = id.clone();
        let name_for_cb = name.clone();
        tiler.set_title_callback(move |pane_title| {
            let Some(stack) = stack_weak.upgrade() else {
                return;
            };
            if stack.visible_child_name().as_deref() != Some(id_for_cb.as_str()) {
                return;
            }
            let combined = if pane_title.is_empty() {
                name_for_cb.clone()
            } else {
                format!("{name_for_cb} \u{b7} {pane_title}")
            };
            if let Some(cb) = title_cb.borrow().as_ref() {
                cb(&combined);
            }
        });

        let row_icon = gtk4::Image::builder()
            .icon_name(icon)
            .css_classes(["sidebar-row-icon"])
            .build();
        let row_label = gtk4::Label::builder()
            .label(&name)
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["sidebar-row-label"])
            .build();
        let close_button = gtk4::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "circular", "sidebar-row-close"])
            .can_focus(false)
            .tooltip_text("Close this project group")
            .build();
        let row_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .build();
        row_box.append(&row_icon);
        row_box.append(&row_label);
        row_box.append(&close_button);

        let row = gtk4::ListBoxRow::builder().child(&row_box).build();
        row.set_widget_name(&id);
        row.add_css_class("sidebar-row");
        self.0.sidebar_list.append(&row);

        let this = self.clone();
        let id_for_close = id.clone();
        close_button.connect_clicked(move |_| this.remove_group(&id_for_close));

        self.0.entries.borrow_mut().push(GroupEntry {
            id: id.clone(),
            tiler: tiler.clone(),
            row: row.clone(),
        });

        self.0.stack.set_visible_child_name(&id);
        self.0.sidebar_list.select_row(Some(&row));

        tiler
    }

    /// Closes every pane in the group `id` and removes it from both the
    /// stack and the sidebar. Refuses to remove the last remaining group -
    /// there's always at least one project open. If the removed group was
    /// the visible one, falls back to a neighboring group.
    fn remove_group(&self, id: &str) {
        let removed_was_active = self.0.stack.visible_child_name().as_deref() == Some(id);
        let (removed, fallback) = {
            let mut entries = self.0.entries.borrow_mut();
            if entries.len() <= 1 {
                return;
            }
            let Some(pos) = entries.iter().position(|e| e.id == id) else {
                return;
            };
            let removed = entries.remove(pos);
            let fallback = entries[pos.min(entries.len() - 1)].clone();
            (removed, fallback)
        };

        removed.tiler.close_all_panes();
        self.0.stack.remove(&removed.tiler);
        self.0.sidebar_list.remove(&removed.row);

        if removed_was_active {
            self.0.stack.set_visible_child_name(&fallback.id);
            self.0.sidebar_list.select_row(Some(&fallback.row));
        }
    }

    /// Switches to the next (`delta = 1`) or previous (`delta = -1`) group,
    /// wrapping around. A no-op with a single group.
    pub fn cycle_group(&self, delta: i32) {
        let entries = self.0.entries.borrow();
        let len = entries.len();
        if len < 2 {
            return;
        }
        let current = self.0.stack.visible_child_name();
        let idx = entries
            .iter()
            .position(|e| Some(e.id.as_str()) == current.as_deref())
            .unwrap_or(0);
        let next = (idx as i32 + delta).rem_euclid(len as i32) as usize;
        let target = entries[next].clone();
        drop(entries);

        self.0.stack.set_visible_child_name(&target.id);
        self.0.sidebar_list.select_row(Some(&target.row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the group state machine directly (add/switch/cycle/remove),
    /// bypassing `new_group`'s `FileDialog` - GTK's own file chooser needs a
    /// desktop portal or a fully-fledged window manager to actually render,
    /// neither of which a test run can assume, and none of that machinery
    /// is what this test is meant to cover anyway.
    #[test]
    fn add_switch_remove_and_cycle_groups() {
        if gtk4::init().is_err() {
            eprintln!("skipping: no display available for gtk4::init()");
            return;
        }

        let groups = Groups::new("/tmp");
        assert_eq!(groups.0.entries.borrow().len(), 1);
        assert_eq!(groups.0.stack.visible_child_name().as_deref(), Some("0"));
        // The initial group must not get an agent pane spawned into it
        // unasked - see `add_group`'s doc comment.
        assert!(groups.active_tiler().is_some());

        groups.add_group("/usr");
        groups.add_group("/etc");
        assert_eq!(groups.0.entries.borrow().len(), 3);
        // add_group switches to the group it just created.
        assert_eq!(groups.0.stack.visible_child_name().as_deref(), Some("2"));

        groups.cycle_group(1);
        assert_eq!(groups.0.stack.visible_child_name().as_deref(), Some("0"));
        groups.cycle_group(-1);
        assert_eq!(groups.0.stack.visible_child_name().as_deref(), Some("2"));

        // Removing the active group falls back to a neighbor.
        groups.remove_group("2");
        assert_eq!(groups.0.entries.borrow().len(), 2);
        assert_ne!(groups.0.stack.visible_child_name().as_deref(), Some("2"));

        // Never removes the last remaining group.
        groups.remove_group("1");
        assert_eq!(groups.0.entries.borrow().len(), 1);
        groups.remove_group("0");
        assert_eq!(groups.0.entries.borrow().len(), 1);
    }
}
