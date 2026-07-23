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
        /// Adjustable Grid-mode weights: one per row, and one per column -
        /// always `cols` of them, even for a row with fewer real panes than
        /// that (a partial last row), so every cell across the whole grid
        /// stays the same size regardless of pane count; a partial row's
        /// unused trailing ratios just correspond to empty space rather
        /// than to a pane. Regenerated to all-equal whenever the pane count
        /// changes underneath them, since the whole grid shape is recomputed
        /// from `n` and the available area at that point anyway - and, for a
        /// grid nobody has dragged, whenever the resolved (cols, rows) shape
        /// flips, e.g. the window being resized from wide to tall re-orients
        /// the grid from side-by-side to stacked. Once a seam *has* been
        /// dragged (`ratios_customized`) that second trigger is off: see
        /// `ensure_grid_ratios`.
        pub row_ratios: RefCell<Vec<f64>>,
        pub col_ratios: RefCell<Vec<Vec<f64>>>,
        pub grid_shape_n: Cell<usize>,
        pub grid_shape_dims: Cell<(usize, usize)>,
        /// Whether the ratios above are the user's rather than this module's -
        /// set the moment a grid seam is dragged, cleared whenever they're
        /// regenerated. It is what `ensure_grid_ratios` reads to tell a layout
        /// somebody arranged from one that merely happened.
        pub ratios_customized: Cell<bool>,
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
                grid_shape_dims: Cell::new((0, 0)),
                ratios_customized: Cell::new(false),
            }
        }
    }

    impl TilerLayout {
        /// Regenerate all-equal Grid ratios if `n` doesn't match what the
        /// current ratios were built for, or if the ideal (cols, rows) shape
        /// for `n` panes in a `width`x`height` area has flipped since (the
        /// window changed enough to favor the other orientation). Passes the
        /// currently-in-use column count into `grid_shape` so a spawn/close
        /// only reorients the whole grid when the new shape is a clear
        /// improvement, not just a marginally squarer one - see
        /// `GRID_STABILITY_BIAS`.
        // `pub(super)` for the tests in this file, which drive it directly:
        // reaching it through `allocate` would mean real panes, and a pane is a
        // PTY with a shell in it.
        pub(super) fn ensure_grid_ratios(&self, n: usize, width: i32, height: i32) {
            // A grid whose seams have been dragged keeps both its proportions
            // and its shape for as long as the pane count holds, whatever the
            // window does around it.
            //
            // Re-orienting can't preserve a drag even in principle: the ratios
            // are shaped to the grid (one weight per row, one per column within
            // each row), so a 2x2 becoming a 1x4 has nowhere to put the four
            // column weights it was holding. Re-orienting *is* discarding them.
            // Dragging a seam is the one unambiguous statement the user makes
            // about this layout, and resizing a window is not a retraction of
            // it - so the drag outranks the reflow, and only opening or closing
            // a pane (which genuinely invalidates the arrangement) resets it.
            if self.ratios_customized.get() && self.grid_shape_n.get() == n {
                return;
            }

            let prev_cols =
                (self.grid_shape_n.get() != usize::MAX).then(|| self.grid_shape_dims.get().0);
            let shape = layout::grid_shape(n, width, height, prev_cols);
            if self.grid_shape_n.get() == n && self.grid_shape_dims.get() == shape {
                return;
            }
            let (cols, rows) = shape;
            *self.row_ratios.borrow_mut() = vec![1.0; rows];
            *self.col_ratios.borrow_mut() = vec![vec![1.0; cols]; rows];
            self.grid_shape_n.set(n);
            self.grid_shape_dims.set(shape);
            // Regenerated, so whatever was arranged here is gone with them.
            self.ratios_customized.set(false);
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
        /// Sizes exactly as the default does, and reports no baseline.
        ///
        /// GTK warns ("reported a horizontal baseline") when a widget hands back
        /// a baseline for a horizontal measurement, which is meaningless - a
        /// baseline is the line text sits on, so only the vertical measurement
        /// has one. The default aggregates its children's, and a VTE terminal
        /// has a real baseline to give, so the tiler was passing one along in
        /// both directions and being told off for it three times a launch.
        ///
        /// Panes are tiled rectangles rather than a row of labels to be aligned
        /// with each other, so there is nothing here for a baseline to mean in
        /// either direction. The minimum and natural sizes are deliberately left
        /// to the parent: they are what stops the window shrinking smaller than
        /// the panes can render into, and silencing a warning is no reason to
        /// change how the window resizes.
        fn measure(&self, widget: &Widget, orientation: gtk4::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let (minimum, natural, _, _) = self.parent_measure(widget, orientation, for_size);
            (minimum, natural, -1, -1)
        }

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
                self.ensure_grid_ratios(n, width, height);
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

/// Everything about how a group is arranged that isn't the mode - reported out
/// to whoever is keeping the model in step, via `Tiler::set_layout_callback`.
///
/// These used to be readable only from inside the layout manager, which meant
/// the app could tile by them but never report or save them. They are still
/// *owned* here rather than read back out of the model on every allocation:
/// `TilerLayout::allocate` runs inside GTK's layout pass, and reaching into a
/// `RefCell` the same pass may already have borrowed is a re-entrancy bug
/// waiting to happen. So the tiler stays the source and pushes changes out,
/// exactly as `mode` already does.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LayoutState {
    pub master_ratio: f64,
    pub master_count: usize,
    /// Index of the focused pane within this group's pane order.
    pub focus: usize,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Tiler {
        pub panes: RefCell<Vec<Rc<Pane>>>,
        pub focus: Cell<usize>,
        pub cwd: RefCell<String>,
        pub title_cb: RefCell<Option<Box<dyn Fn(&str)>>>,
        /// Invoked when a pane in this group wants the user - see
        /// `Tiler::set_attention_callback`.
        pub attention_cb: RefCell<Option<Box<dyn Fn()>>>,
        /// Invoked whenever the layout mode changes, however it changed - see
        /// `Tiler::set_mode_callback`.
        pub mode_cb: RefCell<Option<Box<dyn Fn(Mode)>>>,
        /// Invoked whenever the master ratio, master count or focus index
        /// changes - see `Tiler::set_layout_callback`.
        pub layout_cb: RefCell<Option<Box<dyn Fn(LayoutState)>>>,
        /// Invoked with the new pane count whenever a pane is attached or
        /// removed - see `Tiler::set_pane_count_callback`.
        pub count_cb: RefCell<Option<Box<dyn Fn(usize)>>>,
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

        // Every row carries a full `cols` worth of ratios (so all cells stay
        // the same size - see `TilerLayout::col_ratios`), but a partial last
        // row has real panes in only the first few of them. Only seams
        // *between two real panes* are draggable: without this, the trailing
        // empty cells of a partial row would offer phantom seams over blank
        // space, and dragging one would resize the row's real panes away from
        // the uniform cell size every other row keeps.
        let n = self.imp().panes.borrow().len();
        let cols = lm.imp().grid_shape_dims.get().0;

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
            let panes_in_row = n.saturating_sub(row_i * cols).min(cols);
            let col_spans = layout::weighted_spans(self.width(), ratios);
            for j in 0..panes_in_row.saturating_sub(1) {
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
                // Every write site reports, including this one - a ratio the
                // user dragged to is as real as one they pressed a key for, and
                // a drag that ended anywhere but where the model thinks it did
                // is a session that restores to the wrong shape.
                this.notify_layout();
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
            // Below this, there's no room to give both sides at least
            // MIN_PANE_PX; `clamp` panics if its min bound exceeds its max,
            // which is exactly what `combined - MIN_PANE_PX < MIN_PANE_PX`
            // would do here. Just don't resize rather than crash.
            if combined < 2.0 * MIN_PANE_PX {
                return;
            }
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
            // From here this grid is arranged rather than merely laid out, and
            // a window resize stops being allowed to undo it - see
            // `TilerLayout::ensure_grid_ratios`.
            lm.imp().ratios_customized.set(true);
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
        self.notify_layout();
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
            let is_focused = i == focus;
            if is_focused {
                pane.frame.add_css_class("focused");
            } else {
                pane.frame.remove_css_class("focused");
            }
            // The frame's half of it is the border and the glow; this is the
            // fill, which only reaches the screen if VTE paints it (see
            // `Pane::set_focused`).
            pane.set_focused(is_focused);
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

    /// How many panes this group is currently running.
    pub fn pane_count(&self) -> usize {
        self.imp().panes.borrow().len()
    }

    /// Spawns a pane in this group's project directory (the one it was
    /// created with) - no dialog. Opening a *different* project happens by
    /// creating a whole new project (see `crate::app::App::new_project`)
    /// rather than mixing an unrelated project's panes into this grid.
    pub fn spawn_pane_here(&self) {
        let cwd = self.imp().cwd.borrow().clone();
        self.spawn_pane_in(&cwd);
    }

    fn spawn_pane_in(&self, cwd: &str) {
        self.attach_process_pane(Pane::new(cwd));
    }

    /// Spawns a pane running `command` rather than `claude` - the update
    /// button's pull-and-rebuild script (see `crate::update::command`), which
    /// gets a pane of its own so the user can watch it work.
    ///
    /// `on_finished` is handed `true` when the command exited cleanly. The
    /// update uses that to decide whether to relaunch the app: only a script
    /// that actually got the new binary onto disk is worth restarting into.
    pub fn spawn_command_pane(
        &self,
        cwd: &str,
        command: &str,
        on_finished: impl Fn(bool) + 'static,
    ) {
        let pane = self.attach_process_pane(Pane::command(cwd, command));
        // A second handler on the same signal - `attach_process_pane` already
        // connected one to take the pane down. Both run; neither cares about
        // the other's order.
        //
        // Zero is success under either convention VTE might report the status
        // in (a raw `waitpid` status or a bare exit code), since `exit 0` is 0
        // in both, and every failure - a non-zero exit, a signal - is non-zero
        // in both.
        pane.terminal
            .connect_child_exited(move |_, status| on_finished(status == 0));
    }

    /// Wires up the signals every pane with a child process needs (close on
    /// exit, re-title on the child's title change, flag for attention when the
    /// agent rings the bell) and attaches it. The help pane skips this - it has
    /// no process behind it to exit, re-title, or ring anything.
    ///
    /// Hands the attached pane back so a caller with a further interest in it
    /// (`spawn_command_pane`, which wants to know how its child exited) can
    /// hang its own signal handlers on the same terminal.
    fn attach_process_pane(&self, pane: Pane) -> Rc<Pane> {
        let pane = Rc::new(pane);

        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        pane.terminal.connect_child_exited(move |_, _status| {
            if let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) {
                this.remove_pane(&pane);
                // An agent quitting is news too, if it happened somewhere the
                // user wasn't looking.
                this.notify_attention();
            }
        });

        // The bell is what "the agent wants you" actually looks like on the
        // wire: Claude rings it when it finishes a turn and when it stops to
        // ask something. Nothing else in a stream of terminal output
        // distinguishes "done" from "still typing", so this one byte is the
        // whole signal - `Groups` turns it into a flashing sidebar row.
        let this_weak = self.downgrade();
        pane.terminal.connect_bell(move |_| {
            if let Some(this) = this_weak.upgrade() {
                this.notify_attention();
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

        self.attach_pane(pane.clone());
        pane
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
        let pane_count = self.imp().panes.borrow().len();
        if pane_count == 1 {
            // The first pane in an empty group has to take focus: nothing else
            // is holding it, and a group whose only terminal doesn't accept
            // typing is just broken.
            self.set_focus(0);
        } else {
            // After that, spawning is a background act. You start another agent
            // *while* working in one, and having the keyboard yank itself into
            // a fresh pane mid-sentence sends the rest of that sentence
            // somewhere you weren't looking. The new pane is on screen and one
            // click (or Super+Alt+j) away, which is enough of an invitation.
            //
            // Still a re-tile and a restyle, though: the grid has one more cell
            // in it, and the new pane has to be painted as the unfocused one it
            // is rather than inherit the focused frame.
            self.update_focus_style();
            self.relayout();
        }
        self.notify_pane_count();
    }

    /// Registers a callback invoked with the pane count whenever it changes.
    /// Drives the empty state: a project with nothing running shows what to do
    /// about that rather than a blank rectangle.
    pub fn set_pane_count_callback(&self, f: impl Fn(usize) + 'static) {
        *self.imp().count_cb.borrow_mut() = Some(Box::new(f));
        self.notify_pane_count();
    }

    fn notify_pane_count(&self) {
        let count = self.imp().panes.borrow().len();
        if let Some(cb) = self.imp().count_cb.borrow().as_ref() {
            cb(count);
        }
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
        self.notify_pane_count();
    }

    /// Hangs up every pane in this project, without waiting for their
    /// `child-exited` signals - used when the whole project is being torn down
    /// (see `App::remove_project`), so the caller can drop this `Tiler` right
    /// away instead of waiting on each pane individually.
    pub fn close_all_panes(&self) {
        for pane in self.imp().panes.borrow().iter() {
            pane.hangup();
        }
    }

    pub fn close_focused(&self) {
        let focus = self.imp().focus.get();
        if let Some(pane) = self.imp().panes.borrow().get(focus).cloned() {
            self.close_pane(&pane);
        }
    }

    /// Close a specific pane regardless of focus (e.g. from its own X button).
    /// Removal happens asynchronously via the `child-exited` signal.
    fn close_pane(&self, pane: &Rc<Pane>) {
        pane.hangup();
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
        self.notify_layout();
    }

    pub fn dec_master_ratio(&self) {
        let lm = self.layout_mgr();
        let r = (lm.imp().master_ratio.get() - 0.05).max(0.1);
        lm.imp().master_ratio.set(r);
        self.queue_allocate();
        self.notify_layout();
    }

    pub fn inc_master_count(&self) {
        let len = self.imp().panes.borrow().len().max(1);
        let lm = self.layout_mgr();
        let c = (lm.imp().master_count.get() + 1).min(len);
        lm.imp().master_count.set(c);
        self.queue_allocate();
        self.notify_layout();
    }

    pub fn dec_master_count(&self) {
        let lm = self.layout_mgr();
        let c = (lm.imp().master_count.get().max(2)) - 1;
        lm.imp().master_count.set(c);
        self.queue_allocate();
        self.notify_layout();
    }

    /// Apply `scale` to every current pane's terminal (new panes pick up
    /// whatever `font_scale` holds at attach time, in `attach_pane`). Text
    /// size is a global setting (see `App::inc_font_scale`), which calls
    /// this on every group's `Tiler` in lockstep - not just the active
    /// one - so switching groups never shows a different zoom level.
    pub(crate) fn set_font_scale(&self, scale: f64) {
        self.imp().font_scale.set(scale);
        for pane in self.imp().panes.borrow().iter() {
            pane.terminal.set_font_scale(scale);
        }
    }

    /// How this group's panes are currently arranged.
    ///
    /// Public because the mode is no longer only a tiling input: the header bar
    /// reports it, so it has to be readable from outside the layout manager it
    /// used to be sealed inside. Pressing the cycle key and learning nothing was
    /// the whole problem with keeping it private.
    pub fn mode(&self) -> Mode {
        self.layout_mgr().imp().mode.get()
    }

    /// The one place the mode is written, so `mode_cb` cannot be bypassed - a
    /// header bar showing a mode the tiler isn't in is worse than one showing
    /// nothing.
    pub fn set_mode(&self, mode: Mode) {
        let lm = self.layout_mgr();
        if lm.imp().mode.get() == mode {
            return;
        }
        lm.imp().mode.set(mode);
        self.queue_allocate();
        if let Some(cb) = self.imp().mode_cb.borrow().as_ref() {
            cb(mode);
        }
    }

    /// Registers a callback invoked whenever this group's layout mode changes,
    /// by any route - the keybinding, the header bar, or the monocle toggle.
    pub fn set_mode_callback(&self, f: impl Fn(Mode) + 'static) {
        *self.imp().mode_cb.borrow_mut() = Some(Box::new(f));
    }

    /// How this group is arranged, beyond the mode.
    pub fn layout_state(&self) -> LayoutState {
        let lm = self.layout_mgr();
        LayoutState {
            master_ratio: lm.imp().master_ratio.get(),
            master_count: lm.imp().master_count.get(),
            focus: self.imp().focus.get(),
        }
    }

    /// Registers a callback invoked whenever the master ratio, master count or
    /// focus changes - by keybinding or by dragging the master seam.
    pub fn set_layout_callback(&self, f: impl Fn(LayoutState) + 'static) {
        *self.imp().layout_cb.borrow_mut() = Some(Box::new(f));
    }

    /// Reports the current arrangement outward. Called from every site that
    /// writes one of those three, so the model cannot silently fall behind.
    fn notify_layout(&self) {
        if let Some(cb) = self.imp().layout_cb.borrow().as_ref() {
            cb(self.layout_state());
        }
    }

    pub fn cycle_mode(&self) {
        self.set_mode(self.mode().next());
    }

    /// Jump straight to Monocle (focused pane fullscreen), or back to
    /// MasterStack if already in Monocle.
    pub fn toggle_monocle(&self) {
        self.set_mode(if self.mode() == Mode::Monocle {
            Mode::MasterStack
        } else {
            Mode::Monocle
        });
    }

    fn grab_focus_on_current(&self) {
        let focus = self.imp().focus.get();
        if let Some(pane) = self.imp().panes.borrow().get(focus) {
            pane.terminal.grab_focus();
        }
        self.notify_title();
    }

    /// Called when this group becomes the visible one in the sidebar's
    /// stack: re-grabs terminal focus on its current pane and re-syncs the
    /// window title, since neither happens on its own while a `Tiler` sits
    /// hidden in a background group.
    pub fn on_shown(&self) {
        self.grab_focus_on_current();
    }

    /// Register a callback invoked with the focused pane's foreground-process
    /// title (e.g. so `main.rs` can mirror it onto the window titlebar).
    pub fn set_title_callback(&self, f: impl Fn(&str) + 'static) {
        *self.imp().title_cb.borrow_mut() = Some(Box::new(f));
    }

    /// Register a callback invoked whenever any pane in this group wants the
    /// user's attention - the agent rang the bell (it finished, or it's asking
    /// something) or its process exited. `Groups` uses this to flash the
    /// group's sidebar row; it fires regardless of which pane, or which group,
    /// the user is currently looking at, and it's the listener's job to decide
    /// whether that's worth saying anything about.
    pub fn set_attention_callback(&self, f: impl Fn() + 'static) {
        *self.imp().attention_cb.borrow_mut() = Some(Box::new(f));
    }

    fn notify_attention(&self) {
        if let Some(cb) = self.imp().attention_cb.borrow().as_ref() {
            cb();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::gtk_test;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The layout callback is the only route by which the master ratio and the
    /// focus index reach the model - `TilerLayout`'s cells are private and its
    /// allocation pass is the wrong place to read them from. So a write that
    /// forgets to report is a group that tiles correctly and saves wrong, which
    /// is invisible until a session is restored.
    #[test]
    fn every_master_ratio_change_is_reported_outward() {
        gtk_test(|| {
            let tiler = Tiler::new("/tmp".to_string());
            let seen: Rc<RefCell<Vec<LayoutState>>> = Rc::new(RefCell::new(Vec::new()));

            let sink = seen.clone();
            tiler.set_layout_callback(move |state| sink.borrow_mut().push(state));

            tiler.inc_master_ratio();
            tiler.inc_master_ratio();
            tiler.dec_master_ratio();

            let seen = seen.borrow();
            assert_eq!(seen.len(), 3, "one report per write, no more and no fewer");
            // 0.55 is the starting ratio; the step is 0.05.
            let ratios: Vec<f64> = seen.iter().map(|s| s.master_ratio).collect();
            for (got, want) in ratios.iter().zip([0.60, 0.65, 0.60]) {
                assert!(
                    (got - want).abs() < 1e-9,
                    "reported ratios {ratios:?}, expected 0.60 / 0.65 / 0.60",
                );
            }
            assert_eq!(
                seen.last().map(|s| s.master_ratio),
                Some(tiler.layout_state().master_ratio),
                "the last thing reported is what the tiler actually holds",
            );
        });
    }

    /// A grid the user has arranged by dragging its seams must survive the
    /// window being resized around it - including a resize drastic enough that
    /// a from-scratch pick would choose a different shape entirely.
    ///
    /// This is the regression that motivated the flag: the old code regenerated
    /// all-equal ratios whenever the resolved shape changed, so dragging a seam
    /// and then making the window taller silently threw the drag away.
    #[test]
    fn a_dragged_grid_survives_a_window_resize() {
        gtk_test(|| {
            let lm = TilerLayout::new();
            let imp = lm.imp();

            // Four panes in a wide area: some shape, all-equal ratios.
            imp.ensure_grid_ratios(4, 1600, 900);
            let wide_shape = imp.grid_shape_dims.get();
            assert!(imp.row_ratios.borrow().iter().all(|r| *r == 1.0));
            assert!(!imp.ratios_customized.get(), "nothing dragged yet");

            // The user drags a seam.
            imp.row_ratios.borrow_mut()[0] = 1.6;
            imp.ratios_customized.set(true);
            let dragged = imp.row_ratios.borrow().clone();

            // Now the window turns tall and narrow - the shape a fresh pick
            // would want is not the one on screen.
            imp.ensure_grid_ratios(4, 600, 1600);
            assert_eq!(
                imp.grid_shape_dims.get(),
                wide_shape,
                "the arranged shape holds, because re-orienting *is* discarding",
            );
            assert_eq!(*imp.row_ratios.borrow(), dragged, "the drag survives");

            // Opening a pane genuinely invalidates the arrangement, so that -
            // and only that - resets it.
            imp.ensure_grid_ratios(5, 600, 1600);
            assert!(!imp.ratios_customized.get());
            assert!(
                imp.row_ratios.borrow().iter().all(|r| *r == 1.0),
                "a new pane count starts from equal cells again",
            );
        });
    }

    /// The flag only protects a grid somebody actually arranged. An untouched
    /// one still re-orients itself to the window, which is the behaviour the
    /// README advertises and the reason the flag exists rather than a blanket
    /// "never reorient".
    #[test]
    fn an_untouched_grid_still_reorients_with_the_window() {
        gtk_test(|| {
            let lm = TilerLayout::new();
            let imp = lm.imp();

            imp.ensure_grid_ratios(3, 1600, 400);
            let wide = imp.grid_shape_dims.get();

            imp.ensure_grid_ratios(3, 400, 1600);
            let tall = imp.grid_shape_dims.get();

            assert_ne!(
                wide, tall,
                "a wide window wants columns and a tall one wants rows",
            );
        });
    }

    /// The ratio is clamped inside the tiler rather than by whoever stores it,
    /// so the clamp has to be visible in what gets reported - a model that
    /// records 1.4 restores a master column wider than the window.
    #[test]
    fn a_reported_ratio_is_the_clamped_one() {
        gtk_test(|| {
            let tiler = Tiler::new("/tmp".to_string());
            let seen = Rc::new(RefCell::new(Vec::new()));

            let sink = seen.clone();
            tiler.set_layout_callback(move |state: LayoutState| {
                sink.borrow_mut().push(state.master_ratio)
            });

            for _ in 0..20 {
                tiler.inc_master_ratio();
            }
            assert_eq!(seen.borrow().last().copied(), Some(0.9));

            for _ in 0..40 {
                tiler.dec_master_ratio();
            }
            assert_eq!(seen.borrow().last().copied(), Some(0.1));
        });
    }
}
