//! The rack: the sidebar of projects, and the grip that resizes it.
//!
//! Split out of `app` because it is the half of the window that answers "which
//! project", while the rest answers "what is it doing" - and because between
//! them they had grown past the point where either could be read on its own.
//!
//! A strip is built the way a pane is: a surface a rung above the thing it sits
//! on, inside a hairline, with the project's own hue as its inner left edge. The
//! rack is meant to read as a legend for the workspace rather than as a list
//! beside it, and the construction is where that comes from - see the rules in
//! `style.css`, which carry the rest of the argument.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{gdk, glib};

use super::{set_class, App, ProjectId, ATTENTION_CLASS, DROP_ABOVE_CLASS, DROP_BELOW_CLASS};
use crate::model;
use crate::update;

/// The rack's resize grip: how wide the hit area is, and how far the drag is
/// allowed to take it. The grip is deliberately narrow - it is a seam, not a
/// control, and the pointer changing shape over it is what advertises it - but
/// not so narrow that it takes aim to catch.
///
/// The bounds are pixels because that is what the user is dragging, and they
/// replace the fixed 220-320 the split view was pinned to: project names are the
/// one thing here whose length isn't the app's to choose.
pub(super) const SIDEBAR_GRIP_PX: i32 = 5;
pub(super) const SIDEBAR_MIN_PX: f64 = 170.0;
pub(super) const SIDEBAR_MAX_PX: f64 = 560.0;


/// What share of a `total`-wide split view the rack should take, given the width
/// it had when the drag started and how far the pointer has moved since.
///
/// `None` when there is no width to divide - a split view that hasn't been
/// allocated yet reports zero, and dividing by it would hand the property a NaN
/// that GTK carries silently into a rack of no width at all.
///
/// Two clamps, and both are load-bearing. The pixel one is the range a person
/// dragging expects to be held to. The fraction one is what stops that range
/// being nonsense on a window narrower than the range itself - 560px of rack in
/// a 700px window is not a sidebar, it is the whole application - and the panes
/// are what this is supposed to be sharing with.
/// Where the pointer is horizontally, in the window surface's own coordinates.
///
/// Surface coordinates rather than widget ones because the widget being dragged
/// is the edge being moved - see `build_sidebar_grip`. The surface stays put.
fn pointer_x(gesture: &gtk4::GestureDrag) -> Option<f64> {
    gesture.current_event()?.position().map(|(x, _)| x)
}

fn sidebar_fraction(start_width: f64, offset_x: f64, total: f64) -> Option<f64> {
    if total <= 0.0 {
        return None;
    }
    let wanted = (start_width + offset_x).clamp(SIDEBAR_MIN_PX, SIDEBAR_MAX_PX);
    Some((wanted / total).clamp(0.1, 0.5))
}

/// Takes the insertion line off a row - on leaving it, and on dropping onto it.
/// Both matter: a class left behind here is a line drawn under a drag that ended
/// somewhere else entirely.
fn clear_drop_classes(row: &gtk4::ListBoxRow) {
    row.remove_css_class(DROP_ABOVE_CLASS);
    row.remove_css_class(DROP_BELOW_CLASS);
}

impl App {
    /// The rack: a header, the project rows, and the version beneath them.
    pub(super) fn build_sidebar(&self) -> gtk4::Box {
        let header_label = gtk4::Label::builder()
            .label("Projects")
            .css_classes(["sidebar-header-label"])
            .build();
        let new_project = gtk4::Button::builder()
            .icon_name("list-add-symbolic")
            .css_classes(["flat", "circular"])
            .can_focus(false)
            .tooltip_text("Open a new project as a new group (Super+Alt+Return)")
            .build();
        let this = self.clone();
        new_project.connect_clicked(move |_| this.new_project());

        let header = adw::HeaderBar::builder()
            // An empty title, with the real one packed at the start below: an
            // AdwHeaderBar centres whatever it's given as a title, and a heading
            // centred over a left-aligned column doesn't head it.
            .title_widget(&gtk4::Box::new(gtk4::Orientation::Horizontal, 0))
            // The content side carries the window controls; two sets of them,
            // one either side of the split, reads as two windows. Both ends have
            // to be turned off to mean that: the start side is where the window
            // menu / app icon lands, and left on it put a dim copy of the app's
            // own icon in the sidebar's top-left corner - close enough to a
            // disabled button to look like one, and attached to nothing.
            .show_end_title_buttons(false)
            .show_start_title_buttons(false)
            .build();
        header.pack_start(&header_label);
        header.pack_end(&new_project);

        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&self.0.list)
            .build();

        // What the update button is talking about, kept where it can be read
        // without pressing anything: the answer to "which build am I actually
        // running?", a question only asked right before or right after clicking
        // the button above it.
        let version = gtk4::Label::builder()
            .label(format!("AgentTileCLI {}", update::version()))
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .selectable(true)
            .css_classes(["sidebar-version"])
            .tooltip_text("The version and commit this build was made from")
            .build();

        // Just the version now. The update button that used to sit above it
        // moved into the app menu, where it is one item rather than a second
        // copy of one - and where it is reachable without opening the sidebar,
        // which was always the odd part of keeping it here.
        let footer = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .css_classes(["sidebar-footer"])
            .build();
        footer.append(&version);

        let view = adw::ToolbarView::builder()
            .css_classes(["sidebar"])
            .content(&scrolled)
            .hexpand(true)
            .build();
        view.add_top_bar(&header);
        view.add_bottom_bar(&footer);

        // The rack and the grip that widens it, side by side, so the split view
        // gets one widget and the grip lands on the rack's trailing edge.
        let rack = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .build();
        rack.append(&view);
        rack.append(&self.build_sidebar_grip(&view));
        rack
    }

    /// The strip you drag to make the rack wider or narrower.
    ///
    /// `AdwOverlaySplitView` sizes its sidebar from a fraction of its own width
    /// and offers no handle for changing it, so this is a handle: a few pixels
    /// of hit area on the rack's trailing edge that writes that fraction as it
    /// moves. The fraction, not a width, because the fraction is what the split
    /// view actually honours - and because a rack set as a share of the window
    /// keeps its proportions when the window is resized rather than eating an
    /// ever-larger part of a shrinking one.
    ///
    /// Project names are the one thing in this app whose length isn't the app's
    /// to choose, which is what makes a fixed rack width the wrong call however
    /// carefully it's picked.
    fn build_sidebar_grip(&self, rack: &adw::ToolbarView) -> gtk4::Box {
        let grip = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .width_request(SIDEBAR_GRIP_PX)
            .css_classes(["sidebar-grip"])
            .tooltip_text("Drag to resize the sidebar")
            .build();
        // The pointer has to say this is draggable before it's dragged - a grip
        // this thin is invisible until the cursor changes over it.
        grip.set_cursor_from_name(Some("col-resize"));

        // The rack's width and the pointer's position when the drag began.
        //
        // Both are needed, and the pointer one is why this doesn't use the
        // offset `GestureDrag` hands to `drag_update`. That offset is measured
        // in *widget* coordinates, and this widget is attached to the very edge
        // being dragged - so every frame moves the grip out from under the
        // pointer, which changes the reported offset even when the hand holding
        // the mouse is perfectly still. The next frame reads that as further
        // movement and widens again. What you see is the rack juddering between
        // two widths, several times per second, ghosting as it goes.
        //
        // Measuring against the window surface instead fixes it at the source:
        // the surface doesn't move when the sidebar resizes, so a still hand
        // produces a still number.
        // `None` until a drag begins, and again if the press arrived without a
        // position we can read. A plain 0.0 default would be indistinguishable
        // from "the pointer is at the left edge of the window", so the first
        // motion event would compute a travel of the whole pointer x and slam
        // the rack to its maximum width.
        let start_width = Rc::new(Cell::new(0.0));
        let start_pointer: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));

        let drag = gtk4::GestureDrag::new();
        let rack_at_begin = rack.clone();
        let width_at_begin = start_width.clone();
        let pointer_at_begin = start_pointer.clone();
        drag.connect_drag_begin(move |gesture, _, _| {
            width_at_begin.set(f64::from(rack_at_begin.width()));
            pointer_at_begin.set(pointer_x(gesture));
        });

        let split = self.0.split.clone();
        let width_at_begin = start_width.clone();
        let pointer_at_begin = start_pointer.clone();
        drag.connect_drag_update(move |gesture, _, _| {
            let (Some(now), Some(began)) = (pointer_x(gesture), pointer_at_begin.get()) else {
                return;
            };
            let travelled = now - began;
            let total = f64::from(split.width());
            if let Some(fraction) = sidebar_fraction(width_at_begin.get(), travelled, total) {
                // Only when it actually moves. The property notifies and
                // relayouts on every set, and a drag delivers motion events far
                // faster than the rack can change by a whole pixel.
                if (fraction - split.sidebar_width_fraction()).abs() > f64::EPSILON {
                    split.set_sidebar_width_fraction(fraction);
                }
            }
        });
        grip.add_controller(drag);
        grip
    }

    /// Builds the sidebar row for a project, reading what it says off the model
    /// rather than off its caller.
    ///
    /// The arguments this used to take were the same strings that had just been
    /// handed to `ProjectStore::add`, which is one value with two owners and the
    /// shape of every drift bug this module was split up to prevent - a rename
    /// would have had to remember to touch both. The store is the only place a
    /// project's name, hue and icon are written, so it is the only place they
    /// are read.
    pub(super) fn build_row(&self, id: ProjectId) -> (gtk4::ListBoxRow, gtk4::Label) {
        let store = self.0.store.borrow();
        let Some(project) = store.get(id) else {
            // Unreachable: the caller adds the project before building its row.
            // A blank row beats a panic in a UI callback either way.
            return (gtk4::ListBoxRow::new(), gtk4::Label::new(None));
        };
        let name = project.name.clone();
        let hue = project.hue.clone();
        let row_icon = gtk4::Image::builder()
            .icon_name(&project.icon)
            .css_classes(["sidebar-row-icon"])
            .build();
        drop(store);
        let label = gtk4::Label::builder()
            .label(&name)
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["sidebar-row-label"])
            .build();
        // How many agents this project has running. The rack's whole premise is
        // that a project you aren't looking at keeps working, and until now a
        // row said nothing about how much was going on behind it - the one
        // question you'd actually ask of a project you can't see. Blank at zero
        // rather than "0": a project with nothing running is what the empty
        // state already says at length the moment you open it, and a column of
        // zeroes is noise on every other row to say it again.
        let count = gtk4::Label::builder()
            .halign(gtk4::Align::End)
            .css_classes(["sidebar-row-count"])
            .build();

        let close = gtk4::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "circular", "sidebar-row-close"])
            .can_focus(false)
            .tooltip_text("Close this project group")
            .build();
        let this = self.clone();
        close.connect_clicked(move |_| this.remove_project(id));

        let content = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .build();
        content.append(&row_icon);
        content.append(&label);
        content.append(&count);
        content.append(&close);

        let row = gtk4::ListBoxRow::builder().child(&content).build();
        row.set_widget_name(&id.as_name());
        row.add_css_class("sidebar-row");
        row.add_css_class(&hue);
        row.set_tooltip_text(Some(&format!(
            "{name}\nDrag to reorder (or Super+Alt+Shift+[ / ])"
        )));
        self.install_reorder(&row, id);
        (row, count)
    }

    /// Makes `row` draggable onto its neighbours, so the sidebar's project order
    /// is the user's rather than the order they happened to open things in.
    ///
    /// The drag carries the project's id as a plain string, which means the row
    /// will also *accept* any old text dragged in from outside the app - a
    /// selection from a terminal pane, say. `ProjectId::from_widget_name`
    /// returning `None` is what declines those, rather than the payload being
    /// trusted because it arrived on the right widget.
    fn install_reorder(&self, row: &gtk4::ListBoxRow, id: ProjectId) {
        let drag = gtk4::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .build();
        let dragged = id.as_name();
        drag.connect_prepare(move |_, _, _| {
            Some(gdk::ContentProvider::for_value(&dragged.to_value()))
        });
        // Without an explicit icon the drag has no visible payload at all: the
        // row stays put and nothing follows the pointer, so the drag reads as
        // the app having ignored the gesture. A picture of the row itself is
        // both the obvious icon and a free one.
        let row_weak = row.downgrade();
        drag.connect_drag_begin(move |source, _| {
            if let Some(row) = row_weak.upgrade() {
                source.set_icon(Some(&gtk4::WidgetPaintable::new(Some(&row))), 0, 0);
            }
        });
        row.add_controller(drag);

        let drop = gtk4::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);

        // The insertion line, redrawn as the pointer crosses the row's midpoint:
        // a drop with no preview is a guess, and the guess is wrong half the
        // time by construction (each row is two targets, not one).
        let row_weak = row.downgrade();
        drop.connect_motion(move |_, _, y| {
            if let Some(row) = row_weak.upgrade() {
                let below = model::drops_below(y, f64::from(row.height()));
                set_class(&row, DROP_ABOVE_CLASS, !below);
                set_class(&row, DROP_BELOW_CLASS, below);
            }
            gdk::DragAction::MOVE
        });

        let row_weak = row.downgrade();
        drop.connect_leave(move |_| {
            if let Some(row) = row_weak.upgrade() {
                clear_drop_classes(&row);
            }
        });

        let this = self.clone();
        let row_weak = row.downgrade();
        drop.connect_drop(move |_, value, _, y| {
            let Some(row) = row_weak.upgrade() else {
                return false;
            };
            clear_drop_classes(&row);
            let Some(source) = value
                .get::<String>()
                .ok()
                .and_then(|s| ProjectId::from_widget_name(&s))
            else {
                return false;
            };
            let below = model::drops_below(y, f64::from(row.height()));
            let moved = this.0.store.borrow_mut().reorder_onto(source, id, below);
            if moved {
                // Outside the borrow above: sorting calls back into the store,
                // and a sort kicked off while it was still mutably borrowed
                // would panic.
                this.0.list.invalidate_sort();
            }
            moved
        });
        row.add_controller(drop);
    }

    /// Flags a project as wanting the user: its sidebar row pulses a few times
    /// and then stays quietly tinted until the project is shown.
    ///
    /// A project the user is already looking at gets nothing. The agent that
    /// rang is on screen in front of them; a sidebar row lighting up to report
    /// what they can already see is just noise, and noise is what makes people
    /// stop reading notifications.
    pub(super) fn flash_row(&self, id: ProjectId) {
        if self.0.store.borrow().active() == Some(id) {
            return;
        }
        let Some(row) = self.row_for(id) else { return };
        let toggle = self.0.sidebar_toggle.clone();

        // A CSS animation restarts only when the class is *newly* added, so
        // re-adding one the widget already carries would pulse nothing - which
        // is exactly the case that matters, a second agent finishing while the
        // first is still waiting. Dropping the class and restoring it once GTK
        // has had a frame to notice it gone replays the pulses from the top.
        row.remove_css_class(ATTENTION_CLASS);
        toggle.remove_css_class(ATTENTION_CLASS);
        glib::idle_add_local_once(move || {
            row.add_css_class(ATTENTION_CLASS);
            toggle.add_css_class(ATTENTION_CLASS);
        });
    }

    /// The sidebar toggle speaks for every project at once, so it goes quiet
    /// only once the *last* one still asking has been seen - or closed.
    pub(super) fn refresh_attention(&self) {
        let still_waiting = self
            .0
            .views
            .borrow()
            .iter()
            .any(|v| v.row.has_css_class(ATTENTION_CLASS));
        if !still_waiting {
            self.0.sidebar_toggle.remove_css_class(ATTENTION_CLASS);
        }
    }

    /// Writes `count` onto a project's sidebar row.
    ///
    /// Blank at zero rather than "0" - see `build_row`. The row is looked up by
    /// id rather than captured, because the pane-count callback is registered
    /// before the row it writes to exists; the very first call finds nothing and
    /// does nothing, which is correct, since a project with no panes yet has
    /// nothing to report.
    pub(super) fn sync_row_count(&self, id: ProjectId, count: usize) {
        let views = self.0.views.borrow();
        let Some(view) = views.iter().find(|v| v.id == id) else {
            return;
        };
        view.count.set_label(&if count == 0 {
            String::new()
        } else {
            count.to_string()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rack follows the pointer between its bounds, and stops at them.
    #[test]
    fn dragging_the_rack_edge_stays_inside_its_bounds() {
        let total = 1488.0;
        let frac = |offset| sidebar_fraction(300.0, offset, total).unwrap() * total;

        // Straight tracking in the middle of the range.
        assert!((frac(80.0) - 380.0).abs() < 0.5);
        assert!((frac(-80.0) - 220.0).abs() < 0.5);

        // And held at the ends, however far the pointer keeps going.
        assert!((frac(-9000.0) - SIDEBAR_MIN_PX).abs() < 0.5);
        assert!((frac(9000.0) - SIDEBAR_MAX_PX).abs() < 0.5);
    }

    /// On a window narrow enough that the pixel bounds are absurd, the fraction
    /// clamp is what keeps the panes a window of their own.
    #[test]
    fn a_narrow_window_never_gives_the_rack_more_than_half() {
        let total = 700.0;
        let fraction = sidebar_fraction(300.0, 9000.0, total).unwrap();
        assert!(fraction <= 0.5, "rack took {fraction} of a narrow window");
        assert!(fraction * total < SIDEBAR_MAX_PX);
    }

    /// An unallocated split view reports zero width, and a fraction derived from
    /// it would be a NaN the property accepts without complaint.
    #[test]
    fn an_unallocated_split_view_is_left_alone() {
        assert_eq!(sidebar_fraction(300.0, 40.0, 0.0), None);
    }
}
