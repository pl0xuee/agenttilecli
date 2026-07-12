use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::gio::prelude::*;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{
    gdk, glib, graphene, gsk, EventControllerMotion, EventSequenceState, GestureClick,
    GestureDrag, LayoutManager, PropagationPhase, Widget,
};
use vte4::prelude::*;

/// Pixel tolerance (in either direction) for grabbing a divider with the mouse.
const RESIZE_HANDLE_PX: f64 = 10.0;
/// Never let a mouse-drag squeeze a pane below this many pixels.
const MIN_PANE_PX: f64 = 40.0;

/// How much each font-size keybinding press changes VTE's `font-scale`
/// (a multiplier on the terminal's base font size, independent of layout).
const FONT_SCALE_STEP: f64 = 0.1;
const FONT_SCALE_MIN: f64 = 0.5;
const FONT_SCALE_MAX: f64 = 3.0;

/// Which draggable seam the pointer is over.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Handle {
    /// The single master/stack divider (MasterStack mode).
    Master,
    /// The seam between grid row `i` and row `i + 1` (spans full width).
    GridRow(usize),
    /// The seam between column `j` and `j + 1` within grid row `row`.
    GridCol(usize, usize),
}

/// A snapshot of the two ratios/pixel-widths either side of a grid seam,
/// taken once at drag-begin so drag-update can apply the cumulative offset
/// GestureDrag reports (relative to the start point) without drifting.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GridDragState {
    handle: Handle,
    ratio_a: f64,
    ratio_b: f64,
    px_a: f64,
    px_b: f64,
}

use crate::layout::{self, Mode};
use crate::pane::Pane;

// ---------------------------------------------------------------------
// TilerLayout: a GtkLayoutManager that arranges its widget's children in
// a dwm-style master/stack column. Pure geometry (layout::compute) drives
// allocation; this is just the GTK glue.
// ---------------------------------------------------------------------

mod layout_imp {
    use super::*;

    pub struct TilerLayout {
        pub mode: Cell<Mode>,
        pub master_count: Cell<usize>,
        pub master_ratio: Cell<f64>,
        pub focus: Cell<usize>,
        /// Adjustable Grid-mode weights: one per row, and one per column
        /// within each row (rows can have different item counts because a
        /// partial last row exists). Regenerated to all-equal whenever the
        /// pane count changes underneath them, since the whole grid shape
        /// (rows/cols) is recomputed from `n` at that point anyway.
        pub row_ratios: RefCell<Vec<f64>>,
        pub col_ratios: RefCell<Vec<Vec<f64>>>,
        pub grid_shape_n: Cell<usize>,
    }

    impl Default for TilerLayout {
        fn default() -> Self {
            TilerLayout {
                mode: Cell::new(Mode::default()),
                master_count: Cell::new(1),
                master_ratio: Cell::new(0.55),
                focus: Cell::new(0),
                row_ratios: RefCell::new(Vec::new()),
                col_ratios: RefCell::new(Vec::new()),
                grid_shape_n: Cell::new(usize::MAX),
            }
        }
    }

    impl TilerLayout {
        /// Regenerate all-equal Grid ratios if `n` doesn't match what the
        /// current ratios were built for.
        fn ensure_grid_ratios(&self, n: usize) {
            if self.grid_shape_n.get() == n {
                return;
            }
            let (cols, rows) = layout::grid_shape(n);
            let counts = layout::row_item_counts(n, cols, rows);
            *self.row_ratios.borrow_mut() = vec![1.0; rows];
            *self.col_ratios.borrow_mut() = counts.iter().map(|&c| vec![1.0; c]).collect();
            self.grid_shape_n.set(n);
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TilerLayout {
        const NAME: &'static str = "AgentTileCliTilerLayout";
        type Type = super::TilerLayout;
        type ParentType = LayoutManager;
    }

    impl ObjectImpl for TilerLayout {}

    impl LayoutManagerImpl for TilerLayout {
        fn allocate(&self, widget: &Widget, width: i32, height: i32, _baseline: i32) {
            let mut children = Vec::new();
            let mut next = widget.first_child();
            while let Some(child) = next {
                next = child.next_sibling();
                children.push(child);
            }
            let n = children.len();
            if n == 0 {
                return;
            }

            let rects = if self.mode.get() == Mode::Grid {
                self.ensure_grid_ratios(n);
                layout::grid_weighted(
                    n,
                    width,
                    height,
                    &self.row_ratios.borrow(),
                    &self.col_ratios.borrow(),
                )
            } else {
                layout::compute(
                    n,
                    self.focus.get(),
                    self.mode.get(),
                    self.master_count.get(),
                    self.master_ratio.get(),
                    width,
                    height,
                )
            };

            for (child, rect) in children.iter().zip(rects.iter()) {
                let visible = rect.width > 0 && rect.height > 0;
                child.set_child_visible(visible);
                if visible {
                    let transform = gsk::Transform::new()
                        .translate(&graphene::Point::new(rect.x as f32, rect.y as f32));
                    child.allocate(rect.width, rect.height, -1, Some(transform));
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct TilerLayout(ObjectSubclass<layout_imp::TilerLayout>) @extends LayoutManager;
}

impl TilerLayout {
    fn new() -> Self {
        glib::Object::new()
    }
}

// ---------------------------------------------------------------------
// Tiler: the container widget. Owns pane order/focus; the LayoutManager
// above only needs mode/ratio/master_count/focus to compute geometry from
// the widget tree's actual child order, which Tiler keeps in sync with
// its own `panes` Vec.
// ---------------------------------------------------------------------

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Tiler {
        pub panes: RefCell<Vec<Rc<Pane>>>,
        pub focus: Cell<usize>,
        pub cwd: RefCell<String>,
        pub title_cb: RefCell<Option<Box<dyn Fn(&str)>>>,
        pub resizing: Cell<bool>,
        pub drag_start_ratio: Cell<f64>,
        pub drag_start_width: Cell<i32>,
        pub(crate) grid_drag: RefCell<Option<GridDragState>>,
        /// VTE `font-scale` applied to every pane, including ones spawned
        /// after a resize. Set to 1.0 in `Tiler::new`, since `Cell<f64>`'s
        /// `Default` is 0.0 (invisible text), not the unscaled size.
        pub font_scale: Cell<f64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Tiler {
        const NAME: &'static str = "AgentTileCliTiler";
        type Type = super::Tiler;
        type ParentType = Widget;
    }

    impl ObjectImpl for Tiler {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_layout_manager(Some(super::TilerLayout::new()));
        }

        fn dispose(&self) {
            for pane in self.panes.borrow().iter() {
                pane.frame.unparent();
            }
        }
    }

    impl WidgetImpl for Tiler {}
}

glib::wrapper! {
    pub struct Tiler(ObjectSubclass<imp::Tiler>)
        @extends Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl Tiler {
    pub fn new(cwd: String) -> Self {
        let this: Self = glib::Object::new();
        *this.imp().cwd.borrow_mut() = cwd;
        this.imp().font_scale.set(1.0);
        this.setup_resize();
        this
    }

    fn layout_mgr(&self) -> TilerLayout {
        self.layout_manager()
            .expect("Tiler always has a layout manager")
            .downcast::<TilerLayout>()
            .expect("Tiler's layout manager is always a TilerLayout")
    }

    /// The top-level window, used only to parent the folder-picker dialog
    /// (so it's placed/modal relative to the app instead of floating free).
    fn parent_window(&self) -> Option<gtk4::Window> {
        self.root().and_then(|r| r.downcast::<gtk4::Window>().ok())
    }

    /// The x-coordinate (in this widget's own space) of the master/stack
    /// divider, or `None` when there's no such divider to drag (not in
    /// MasterStack mode, or every pane is already in the master column).
    fn master_boundary_x(&self) -> Option<i32> {
        let lm = self.layout_mgr();
        if lm.imp().mode.get() != Mode::MasterStack {
            return None;
        }
        let n = self.imp().panes.borrow().len();
        if n == 0 {
            return None;
        }
        let master_count = lm.imp().master_count.get().clamp(1, n);
        if master_count >= n {
            return None;
        }
        Some((self.width() as f64 * lm.imp().master_ratio.get()) as i32)
    }

    /// Which draggable seam (if any) is under `(x, y)`, in this widget's own
    /// coordinate space.
    fn handle_at(&self, x: f64, y: f64) -> Option<Handle> {
        let lm = self.layout_mgr();
        match lm.imp().mode.get() {
            Mode::MasterStack => self
                .master_boundary_x()
                .filter(|&bx| (x - bx as f64).abs() <= RESIZE_HANDLE_PX)
                .map(|_| Handle::Master),
            Mode::Grid => self.grid_handle_at(x, y),
            Mode::Monocle => None,
        }
    }

    fn grid_handle_at(&self, x: f64, y: f64) -> Option<Handle> {
        let lm = self.layout_mgr();
        let row_ratios = lm.imp().row_ratios.borrow();
        if row_ratios.is_empty() {
            return None;
        }
        let row_spans = layout::weighted_spans(self.height(), &row_ratios);

        // Column seams take priority: only reachable within their own row's
        // vertical extent, whereas row seams span the full width.
        for (row_i, &(ry, rh)) in row_spans.iter().enumerate() {
            if y < ry as f64 - RESIZE_HANDLE_PX || y > (ry + rh) as f64 + RESIZE_HANDLE_PX {
                continue;
            }
            let col_ratios = lm.imp().col_ratios.borrow();
            let Some(ratios) = col_ratios.get(row_i) else {
                continue;
            };
            let col_spans = layout::weighted_spans(self.width(), ratios);
            for j in 0..col_spans.len().saturating_sub(1) {
                let boundary = col_spans[j + 1].0;
                if (x - boundary as f64).abs() <= RESIZE_HANDLE_PX {
                    return Some(Handle::GridCol(row_i, j));
                }
            }
        }

        for i in 0..row_spans.len().saturating_sub(1) {
            let boundary = row_spans[i + 1].0;
            if (y - boundary as f64).abs() <= RESIZE_HANDLE_PX {
                return Some(Handle::GridRow(i));
            }
        }

        None
    }

    /// Wires up mouse drag-to-resize for whichever seam is under the
    /// pointer (the master/stack divider, or a grid row/column seam) plus a
    /// resize cursor on hover, so dragging is discoverable without reading
    /// the help pane.
    fn setup_resize(&self) {
        let drag = GestureDrag::new();
        drag.set_propagation_phase(PropagationPhase::Capture);
        drag.set_button(gdk::BUTTON_PRIMARY);

        let this = self.clone();
        drag.connect_drag_begin(move |gesture, x, y| {
            this.imp().resizing.set(false);
            *this.imp().grid_drag.borrow_mut() = None;

            match this.handle_at(x, y) {
                Some(Handle::Master) => {
                    this.imp().resizing.set(true);
                    this.imp()
                        .drag_start_ratio
                        .set(this.layout_mgr().imp().master_ratio.get());
                    this.imp().drag_start_width.set(this.width());
                    gesture.set_state(EventSequenceState::Claimed);
                }
                Some(Handle::GridRow(i)) => {
                    let lm = this.layout_mgr();
                    let ratios = lm.imp().row_ratios.borrow();
                    let spans = layout::weighted_spans(this.height(), &ratios);
                    let state = GridDragState {
                        handle: Handle::GridRow(i),
                        ratio_a: ratios[i],
                        ratio_b: ratios[i + 1],
                        px_a: spans[i].1 as f64,
                        px_b: spans[i + 1].1 as f64,
                    };
                    drop(ratios);
                    *this.imp().grid_drag.borrow_mut() = Some(state);
                    gesture.set_state(EventSequenceState::Claimed);
                }
                Some(Handle::GridCol(row_i, j)) => {
                    let lm = this.layout_mgr();
                    let col_ratios = lm.imp().col_ratios.borrow();
                    let ratios = &col_ratios[row_i];
                    let spans = layout::weighted_spans(this.width(), ratios);
                    let state = GridDragState {
                        handle: Handle::GridCol(row_i, j),
                        ratio_a: ratios[j],
                        ratio_b: ratios[j + 1],
                        px_a: spans[j].1 as f64,
                        px_b: spans[j + 1].1 as f64,
                    };
                    drop(col_ratios);
                    *this.imp().grid_drag.borrow_mut() = Some(state);
                    gesture.set_state(EventSequenceState::Claimed);
                }
                None => {}
            }
        });

        let this = self.clone();
        drag.connect_drag_update(move |_, offset_x, offset_y| {
            if this.imp().resizing.get() {
                let width = this.imp().drag_start_width.get();
                if width <= 0 {
                    return;
                }
                let ratio = this.imp().drag_start_ratio.get() + offset_x / width as f64;
                this.layout_mgr()
                    .imp()
                    .master_ratio
                    .set(ratio.clamp(0.1, 0.9));
                this.queue_allocate();
                return;
            }

            let Some(state) = *this.imp().grid_drag.borrow() else {
                return;
            };
            let delta = match state.handle {
                Handle::GridRow(_) => offset_y,
                Handle::GridCol(_, _) => offset_x,
                Handle::Master => return,
            };
            let combined = state.px_a + state.px_b;
            let new_a = (state.px_a + delta).clamp(MIN_PANE_PX, combined - MIN_PANE_PX);
            let new_b = combined - new_a;
            let sum = state.ratio_a + state.ratio_b;
            let new_ratio_a = sum * (new_a / combined);
            let new_ratio_b = sum * (new_b / combined);

            let lm = this.layout_mgr();
            match state.handle {
                Handle::GridRow(i) => {
                    let mut ratios = lm.imp().row_ratios.borrow_mut();
                    ratios[i] = new_ratio_a;
                    ratios[i + 1] = new_ratio_b;
                }
                Handle::GridCol(row_i, j) => {
                    let mut col_ratios = lm.imp().col_ratios.borrow_mut();
                    let ratios = &mut col_ratios[row_i];
                    ratios[j] = new_ratio_a;
                    ratios[j + 1] = new_ratio_b;
                }
                Handle::Master => {}
            }
            this.queue_allocate();
        });

        let this = self.clone();
        drag.connect_drag_end(move |_, _, _| {
            this.imp().resizing.set(false);
            *this.imp().grid_drag.borrow_mut() = None;
        });

        self.add_controller(drag);

        let motion = EventControllerMotion::new();
        let this = self.clone();
        motion.connect_motion(move |_, x, y| {
            let cursor = match this.handle_at(x, y) {
                Some(Handle::Master) | Some(Handle::GridCol(_, _)) => Some("col-resize"),
                Some(Handle::GridRow(_)) => Some("row-resize"),
                None => None,
            };
            this.set_cursor_from_name(cursor);
        });
        let this = self.clone();
        motion.connect_leave(move |_| {
            this.set_cursor_from_name(None);
        });
        self.add_controller(motion);
    }

    /// Change focus, refresh the focus-border CSS, re-tile (needed in
    /// Monocle mode, harmless elsewhere), and grab keyboard focus onto the
    /// newly-focused pane's terminal.
    fn set_focus(&self, idx: usize) {
        self.imp().focus.set(idx);
        self.update_focus_style();
        self.relayout();
        self.grab_focus_on_current();
    }

    /// Push this widget's focus index into the layout manager and request a
    /// re-tile. Geometry only actually depends on focus in Monocle mode, but
    /// this is cheap enough to call unconditionally after any pane op.
    fn relayout(&self) {
        let focus = self.imp().focus.get();
        self.layout_mgr().imp().focus.set(focus);
        self.queue_allocate();
    }

    fn update_focus_style(&self) {
        let focus = self.imp().focus.get();
        for (i, pane) in self.imp().panes.borrow().iter().enumerate() {
            if i == focus {
                pane.frame.add_css_class("focused");
            } else {
                pane.frame.remove_css_class("focused");
            }
        }
    }

    /// Reparent every pane in current Vec order so the widget child order
    /// (which the layout manager reads directly) always matches it. Cheap
    /// for the small pane counts this app deals with.
    fn reflow_children(&self) {
        let panes = self.imp().panes.borrow();
        for pane in panes.iter() {
            pane.frame.unparent();
        }
        for pane in panes.iter() {
            pane.frame.set_parent(self);
        }
    }

    /// Asks (via a folder picker) which project to open, then spawns a pane
    /// there. The dialog opens pre-filled with the last directory used (or
    /// the app's own launch directory, the very first time). Cancelling it
    /// spawns nothing - `spawn_pane_here` is the dedicated way to add a pane
    /// without picking a folder.
    pub fn spawn_pane(&self) {
        let last_dir = self.imp().cwd.borrow().clone();

        let dialog = gtk4::FileDialog::builder()
            .title("Open project in a new pane")
            .accept_label("Open")
            .modal(true)
            .initial_folder(&gtk4::gio::File::for_path(&last_dir))
            .build();

        let this = self.clone();
        let parent = self.parent_window();
        dialog.select_folder(
            parent.as_ref(),
            None::<&gtk4::gio::Cancellable>,
            move |result| {
                let Some(dir) = result.ok().and_then(|file| file.path()) else {
                    return;
                };
                let dir = dir.to_string_lossy().into_owned();
                this.imp().cwd.replace(dir.clone());
                this.spawn_pane_in(&dir);
            },
        );
    }

    /// Spawns a pane in the current project (whatever directory the last
    /// `spawn_pane` folder pick landed on, or the app's launch directory if
    /// none has happened yet) - no dialog, unlike `spawn_pane`.
    pub fn spawn_pane_here(&self) {
        let cwd = self.imp().cwd.borrow().clone();
        self.spawn_pane_in(&cwd);
    }

    fn spawn_pane_in(&self, cwd: &str) {
        let pane = Rc::new(Pane::new(cwd));

        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        pane.terminal.connect_child_exited(move |_, _status| {
            if let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) {
                this.remove_pane(&pane);
            }
        });

        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        pane.terminal.connect_window_title_notify(move |_| {
            if let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) {
                let focus = this.imp().focus.get();
                let is_focused = this
                    .imp()
                    .panes
                    .borrow()
                    .get(focus)
                    .is_some_and(|p| Rc::ptr_eq(p, &pane));
                if is_focused {
                    this.notify_title();
                }
            }
        });

        self.attach_pane(pane);
    }

    /// Toggle a static cheatsheet pane on/off: closes it if one is already
    /// open, otherwise spawns one (with no child process, just fed text).
    pub fn toggle_help(&self) {
        let existing = self
            .imp()
            .panes
            .borrow()
            .iter()
            .find(|p| p.is_help())
            .cloned();
        match existing {
            Some(pane) => self.remove_pane(&pane),
            None => self.attach_pane(Rc::new(Pane::help())),
        }
    }

    fn attach_pane(&self, pane: Rc<Pane>) {
        pane.frame.set_parent(self);
        pane.terminal.set_font_scale(self.imp().font_scale.get());

        // Click-to-focus: fires in the Capture phase so it always sees the
        // press, but never claims it, so the terminal underneath still gets
        // normal click/selection behavior afterward.
        let click = GestureClick::new();
        click.set_propagation_phase(PropagationPhase::Capture);
        click.set_button(gdk::BUTTON_PRIMARY);
        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        click.connect_pressed(move |_, _n_press, _x, _y| {
            if let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) {
                let idx = this
                    .imp()
                    .panes
                    .borrow()
                    .iter()
                    .position(|p| Rc::ptr_eq(p, &pane));
                if let Some(idx) = idx {
                    this.set_focus(idx);
                }
            }
        });
        pane.frame.add_controller(click);

        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        pane.close_button.connect_clicked(move |_| {
            if let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) {
                this.close_pane(&pane);
            }
        });

        self.imp().panes.borrow_mut().push(pane);
        self.set_focus(self.imp().panes.borrow().len() - 1);
    }

    fn remove_pane(&self, pane: &Rc<Pane>) {
        let removed = {
            let mut panes = self.imp().panes.borrow_mut();
            if let Some(pos) = panes.iter().position(|p| Rc::ptr_eq(p, pane)) {
                panes.remove(pos);
                true
            } else {
                false
            }
        };
        if !removed {
            return;
        }
        pane.frame.unparent();

        let len = self.imp().panes.borrow().len();
        let focus = self.imp().focus.get();
        self.set_focus(if len == 0 { 0 } else { focus.min(len - 1) });
    }

    pub fn close_focused(&self) {
        let focus = self.imp().focus.get();
        if let Some(pane) = self.imp().panes.borrow().get(focus).cloned() {
            self.close_pane(&pane);
        }
    }

    /// Close a specific pane regardless of focus (e.g. from its own X button).
    fn close_pane(&self, pane: &Rc<Pane>) {
        if pane.is_help() {
            // No process to wait on - remove immediately.
            self.remove_pane(pane);
        } else {
            // Removal happens asynchronously via the `child-exited` signal.
            pane.hangup();
        }
    }

    pub fn focus_next(&self) {
        let len = self.imp().panes.borrow().len();
        if len == 0 {
            return;
        }
        self.set_focus((self.imp().focus.get() + 1) % len);
    }

    pub fn focus_prev(&self) {
        let len = self.imp().panes.borrow().len();
        if len == 0 {
            return;
        }
        self.set_focus((self.imp().focus.get() + len - 1) % len);
    }

    /// dwm-style "zoom": swap the focused pane into the master slot (index 0).
    pub fn promote_focused_to_master(&self) {
        let focus = self.imp().focus.get();
        if focus == 0 {
            return;
        }
        self.imp().panes.borrow_mut().swap(0, focus);
        self.reflow_children();
        self.set_focus(0);
    }

    pub fn inc_master_ratio(&self) {
        let lm = self.layout_mgr();
        let r = (lm.imp().master_ratio.get() + 0.05).min(0.9);
        lm.imp().master_ratio.set(r);
        self.queue_allocate();
    }

    pub fn dec_master_ratio(&self) {
        let lm = self.layout_mgr();
        let r = (lm.imp().master_ratio.get() - 0.05).max(0.1);
        lm.imp().master_ratio.set(r);
        self.queue_allocate();
    }

    pub fn inc_master_count(&self) {
        let len = self.imp().panes.borrow().len().max(1);
        let lm = self.layout_mgr();
        let c = (lm.imp().master_count.get() + 1).min(len);
        lm.imp().master_count.set(c);
        self.queue_allocate();
    }

    pub fn dec_master_count(&self) {
        let lm = self.layout_mgr();
        let c = (lm.imp().master_count.get().max(2)) - 1;
        lm.imp().master_count.set(c);
        self.queue_allocate();
    }

    /// Apply `scale` to every current pane's terminal (new panes pick up
    /// whatever `font_scale` holds at attach time, in `attach_pane`).
    fn set_font_scale(&self, scale: f64) {
        self.imp().font_scale.set(scale);
        for pane in self.imp().panes.borrow().iter() {
            pane.terminal.set_font_scale(scale);
        }
    }

    pub fn inc_font_scale(&self) {
        let scale = (self.imp().font_scale.get() + FONT_SCALE_STEP).min(FONT_SCALE_MAX);
        self.set_font_scale(scale);
    }

    pub fn dec_font_scale(&self) {
        let scale = (self.imp().font_scale.get() - FONT_SCALE_STEP).max(FONT_SCALE_MIN);
        self.set_font_scale(scale);
    }

    pub fn reset_font_scale(&self) {
        self.set_font_scale(1.0);
    }

    pub fn cycle_mode(&self) {
        let lm = self.layout_mgr();
        let m = lm.imp().mode.get().next();
        lm.imp().mode.set(m);
        self.queue_allocate();
    }

    /// Jump straight to Monocle (focused pane fullscreen), or back to
    /// MasterStack if already in Monocle.
    pub fn toggle_monocle(&self) {
        let lm = self.layout_mgr();
        let m = if lm.imp().mode.get() == Mode::Monocle {
            Mode::MasterStack
        } else {
            Mode::Monocle
        };
        lm.imp().mode.set(m);
        self.queue_allocate();
    }

    fn grab_focus_on_current(&self) {
        let focus = self.imp().focus.get();
        if let Some(pane) = self.imp().panes.borrow().get(focus) {
            pane.terminal.grab_focus();
        }
        self.notify_title();
    }

    /// Register a callback invoked with the focused pane's foreground-process
    /// title (e.g. so `main.rs` can mirror it onto the window titlebar).
    pub fn set_title_callback(&self, f: impl Fn(&str) + 'static) {
        *self.imp().title_cb.borrow_mut() = Some(Box::new(f));
    }

    fn notify_title(&self) {
        let focus = self.imp().focus.get();
        let title = self
            .imp()
            .panes
            .borrow()
            .get(focus)
            .and_then(|p| p.terminal.window_title())
            .map(|t| t.to_string())
            .unwrap_or_default();
        if let Some(cb) = self.imp().title_cb.borrow().as_ref() {
            cb(&title);
        }
    }
}
