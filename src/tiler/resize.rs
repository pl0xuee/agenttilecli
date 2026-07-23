//! Grabbing and dragging the seams between panes.
//!
//! Every divider in a tiled window is draggable: the master/stack boundary in
//! MasterStack mode, and both axes of seam in Grid mode. What a drag writes is
//! a ratio rather than a width, so the arrangement survives the window being
//! resized around it - see `TilerLayout::ensure_grid_ratios`.

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, EventControllerMotion, EventSequenceState, GestureDrag, PropagationPhase};

use super::{GridDragState, Handle, Tiler};
use crate::layout::{self, Mode};

/// Pixel tolerance (in either direction) for grabbing a divider with the mouse.
const RESIZE_HANDLE_PX: f64 = 10.0;
/// Never let a mouse-drag squeeze a pane below this many pixels.
const MIN_PANE_PX: f64 = 40.0;

impl Tiler {
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
    pub(super) fn setup_resize(&self) {
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
}
