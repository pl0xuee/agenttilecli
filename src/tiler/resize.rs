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

    /// Snapshots what a grid drag needs to remember about the seam it grabbed:
    /// the weight either side of it, and the pixel extent each of those weights
    /// currently resolves to.
    ///
    /// `None` when the grid has no such seam. `handle_at` has just said it does,
    /// so this is belt to that braces - the bounds check that earns its keep is
    /// the one in `drag_update`, by which time the grid has had the chance to
    /// change shape underneath the drag.
    ///
    /// The generation is stamped in here, alongside the numbers it certifies,
    /// because that is what makes it trustworthy: it is read from the same
    /// borrow that produced the ratios, so there is no window between measuring
    /// the grid and recording which grid was measured.
    fn grid_seam_state(&self, handle: Handle) -> Option<GridDragState> {
        let lm = self.layout_mgr();
        let generation = lm.imp().grid_generation.get();
        match handle {
            Handle::GridRow(i) => {
                let ratios = lm.imp().row_ratios.borrow();
                let spans = layout::weighted_spans(self.height(), &ratios);
                seam_state(handle, &ratios, &spans, i, generation)
            }
            Handle::GridCol(row_i, j) => {
                let col_ratios = lm.imp().col_ratios.borrow();
                let ratios = col_ratios.get(row_i)?;
                let spans = layout::weighted_spans(self.width(), ratios);
                seam_state(handle, ratios, &spans, j, generation)
            }
            Handle::Master => None,
        }
    }

    /// Wires up mouse drag-to-resize for whichever seam is under the
    /// pointer (the master/stack divider, or a grid row/column seam) plus a
    /// resize cursor on hover, so dragging is discoverable without reading
    /// the help pane.
    pub(super) fn setup_resize(&self) {
        let drag = GestureDrag::new();
        drag.set_propagation_phase(PropagationPhase::Capture);
        drag.set_button(gdk::BUTTON_PRIMARY);

        // Every closure below holds the tiler weakly, and each of them has to.
        // These controllers are added to the very widget their closures reach
        // back into (`add_controller`, at the bottom of this function), so a
        // strong `self.clone()` here closes a Tiler -> controller -> closure ->
        // Tiler reference cycle, and GObject has no cycle collector to break one.
        // Closing a project (`App::remove_project`) would then leak its tiler,
        // its layout manager and both of these controllers, and `imp::dispose`
        // would never run - so the panes that method unparents would stay
        // parented to a widget nothing can reach. `panes.rs` holds its pane
        // controllers the same way, for the same reason. These are not
        // clone-avoidance for its own sake; do not turn them back into clones.
        let this = self.downgrade();
        drag.connect_drag_begin(move |gesture, x, y| {
            let Some(this) = this.upgrade() else {
                return;
            };
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
                Some(handle @ (Handle::GridRow(_) | Handle::GridCol(_, _))) => {
                    let Some(state) = this.grid_seam_state(handle) else {
                        return;
                    };
                    *this.imp().grid_drag.borrow_mut() = Some(state);
                    gesture.set_state(EventSequenceState::Claimed);
                }
                None => {}
            }
        });

        let this = self.downgrade();
        drag.connect_drag_update(move |_, offset_x, offset_y| {
            let Some(this) = this.upgrade() else {
                return;
            };
            if this.imp().resizing.get() {
                let width = this.imp().drag_start_width.get();
                if width <= 0 {
                    return;
                }
                let ratio = this.imp().drag_start_ratio.get() + offset_x / width as f64;
                this.layout_mgr()
                    .imp()
                    .master_ratio
                    .set(ratio.clamp(
                        crate::layout::MASTER_RATIO_MIN,
                        crate::layout::MASTER_RATIO_MAX,
                    ));
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

            // The seam is looked up rather than indexed, because the grid it was
            // grabbed from can be thrown away underneath the drag and nothing
            // tells the gesture. `TilerLayout::ensure_grid_ratios` rebuilds both
            // ratio vectors from scratch whenever the pane count or the resolved
            // (cols, rows) changes, and that needs no help from the user: an
            // agent in another pane exiting - `/exit`, Ctrl+D, a crash - removes
            // its pane, which queues an allocation, which reshapes the grid
            // between two motion events. Five panes in a wide window is a 2x3
            // grid holding three row weights; the fourth agent quitting makes it
            // a 2x2 holding two, while the hand is still on the seam that used
            // to be between rows 1 and 2. A row count that shrinks does the same
            // to `col_ratios`' outer index.
            //
            // Reaching past the end there is not a failed drag, it is a dead app:
            // a panic in a GTK signal closure has C frames between it and any
            // catch, so it cannot unwind and aborts the process instead ("thread
            // caused non-unwinding panic. aborting."), taking the window and every
            // agent in every project with it - over a mouse gesture.
            //
            // A seam that is no longer there ends the drag rather than moving
            // something else: `px_a`/`px_b` were measured against a grid that has
            // since been rebuilt, so there is nothing left for this offset to be
            // an offset *from*. Releasing and grabbing again starts a fresh drag
            // against the grid that is actually on screen.
            //
            // Bounds alone are not enough to notice that, though, and this is the
            // half that took a second bug to find. A rebuild does not have to
            // change the grid's *size* - `grid_shape` is scored from the pane
            // count and the window, so a close can land back on the shape it
            // started from. Six panes in a 1470x890 window is three columns over
            // two rows; one agent exiting makes five panes, and the stability bias
            // keeps three columns over two rows. Same lengths, same indices, every
            // bounds check satisfied - and all four values regenerated to 1.0.
            //
            // The drag would then be perfectly willing to continue. It still holds
            // the weights and pixel extents it measured off the *previous* grid, so
            // `sum` is the total the two dragged rows used to share, and the next
            // motion event redistributes that total across two rows that have since
            // been reset to equal. The write is in bounds and the arithmetic is
            // sound; the answer is simply about a grid that is gone, and the seam
            // jumps out from under the pointer to a position the user never asked
            // for. A wrong ratio that looks plausible is the failure that survives
            // review, which is why the generation is checked rather than the shape:
            // it counts rebuilds, so it cannot be fooled by one that happens to
            // arrive at the same dimensions.
            let lm = this.layout_mgr();
            let moved = if state.generation != lm.imp().grid_generation.get() {
                false
            } else {
                match state.handle {
                    Handle::GridRow(i) => {
                        let mut ratios = lm.imp().row_ratios.borrow_mut();
                        write_seam(&mut ratios, i, new_ratio_a, new_ratio_b)
                    }
                    Handle::GridCol(row_i, j) => {
                        let mut col_ratios = lm.imp().col_ratios.borrow_mut();
                        col_ratios
                            .get_mut(row_i)
                            .is_some_and(|ratios| write_seam(ratios, j, new_ratio_a, new_ratio_b))
                    }
                    Handle::Master => return,
                }
            };
            if !moved {
                *this.imp().grid_drag.borrow_mut() = None;
                return;
            }
            this.queue_allocate();
        });

        let this = self.downgrade();
        drag.connect_drag_end(move |_, _, _| {
            let Some(this) = this.upgrade() else {
                return;
            };
            this.imp().resizing.set(false);
            *this.imp().grid_drag.borrow_mut() = None;
        });

        self.add_controller(drag);

        let motion = EventControllerMotion::new();
        let this = self.downgrade();
        motion.connect_motion(move |_, x, y| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let cursor = match this.handle_at(x, y) {
                Some(Handle::Master) | Some(Handle::GridCol(_, _)) => Some("col-resize"),
                Some(Handle::GridRow(_)) => Some("row-resize"),
                None => None,
            };
            this.set_cursor_from_name(cursor);
        });
        let this = self.downgrade();
        motion.connect_leave(move |_| {
            if let Some(this) = this.upgrade() {
                this.set_cursor_from_name(None);
            }
        });
        self.add_controller(motion);
    }
}

/// The two values at `i` and `i + 1`, or `None` when the slice doesn't reach
/// that far.
///
/// A slice pattern rather than two index expressions, because indexing is the
/// one thing the drag handlers must never do to a ratio vector - see the write
/// in `drag_update` for what an out-of-bounds costs here.
fn seam_pair<T: Copy>(values: &[T], i: usize) -> Option<(T, T)> {
    match values.get(i..) {
        Some([a, b, ..]) => Some((*a, *b)),
        _ => None,
    }
}

/// The drag state for seam `i` of one axis: its two weights, and the two pixel
/// extents from the matching `weighted_spans`. `None` if either runs short.
fn seam_state(
    handle: Handle,
    ratios: &[f64],
    spans: &[(i32, i32)],
    i: usize,
    generation: u64,
) -> Option<GridDragState> {
    let (ratio_a, ratio_b) = seam_pair(ratios, i)?;
    let ((_, px_a), (_, px_b)) = seam_pair(spans, i)?;
    Some(GridDragState {
        handle,
        ratio_a,
        ratio_b,
        px_a: px_a as f64,
        px_b: px_b as f64,
        generation,
    })
}

/// Writes the pair of weights either side of seam `i`, reporting whether there
/// was still a seam `i` to write to.
///
/// `false` means the grid was rebuilt out from under an in-flight drag and this
/// seam went with it. The caller's job is then to abandon the drag, not to pick
/// a different seam to move.
fn write_seam(ratios: &mut [f64], i: usize, before: f64, after: f64) -> bool {
    match ratios.get_mut(i..) {
        Some([a, b, ..]) => {
            *a = before;
            *b = after;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::gtk_test;

    /// A seam is two weights, so a vector that can't produce two from `i` has no
    /// seam `i` - and the last weight in a vector is not the near side of
    /// anything.
    #[test]
    fn a_seam_needs_two_weights_to_be_a_seam() {
        let mut ratios = vec![1.0, 1.0, 1.0];

        assert!(write_seam(&mut ratios, 1, 1.4, 0.6));
        assert_eq!(ratios, vec![1.0, 1.4, 0.6]);

        assert!(
            !write_seam(&mut ratios, 2, 9.0, 9.0),
            "index 2 of three weights is the last row; there is no row after it \
             to trade pixels with",
        );
        assert!(!write_seam(&mut ratios, 7, 9.0, 9.0), "past the end entirely");
        assert!(!write_seam(&mut [], 0, 9.0, 9.0), "no grid at all");
        assert_eq!(
            ratios,
            vec![1.0, 1.4, 0.6],
            "a refused write must not have written anything",
        );

        assert_eq!(seam_pair(&ratios, 1), Some((1.4, 0.6)));
        assert_eq!(seam_pair(&ratios, 2), None);
    }

    /// The regression test for a crash that killed the whole app, not just the
    /// drag: an agent exiting in another pane rebuilds the grid under an
    /// in-flight seam drag, and the next motion event used to index the ratio
    /// vector it had been rebuilt out of.
    ///
    /// Note what a failure looks like here. The write happens inside a GTK signal
    /// closure, so a panic in it cannot unwind through the C frames that called
    /// it and aborts instead - with the bug back, this test doesn't fail, it
    /// takes the entire test binary down with it. That is the same abort a user
    /// got, and the reason this is worth a test rather than a bounds check
    /// nobody checks.
    #[test]
    fn a_pane_closing_mid_drag_cannot_abort_the_process() {
        gtk_test(|| {
            let tiler = Tiler::new("/tmp".to_string());
            let lm = tiler.layout_mgr();

            // Four panes in this window is a 2x2; opening a fifth keeps the two
            // columns (`GRID_STABILITY_BIAS` - reshuffling every pane on screen
            // to gain a little squareness is worse than a partial row) and so
            // makes it three rows of two. Three row weights, and a seam between
            // rows 1 and 2 for the user to grab.
            lm.imp().ensure_grid_ratios(4, 1470, 890);
            lm.imp().ensure_grid_ratios(5, 1470, 890);
            assert_eq!(
                lm.imp().row_ratios.borrow().len(),
                3,
                "five panes in a 1470x890 window that already held four should \
                 be three rows of two",
            );
            *tiler.imp().grid_drag.borrow_mut() = Some(GridDragState {
                handle: Handle::GridRow(1),
                ratio_a: 1.0,
                ratio_b: 1.0,
                px_a: 300.0,
                px_b: 300.0,
                // Stamped now, as `grid_seam_state` would at drag-begin: the grid
                // the user grabbed is the one standing here, before the close.
                generation: lm.imp().grid_generation.get(),
            });

            // The agent in some other pane types `/exit`. `remove_pane` ->
            // `queue_allocate` -> `allocate` -> here, arriving between two motion
            // events of a drag nobody has let go of.
            lm.imp().ensure_grid_ratios(4, 1470, 890);
            assert_eq!(
                lm.imp().row_ratios.borrow().len(),
                2,
                "four panes is a 2x2, so the dragged seam no longer exists",
            );

            drag_gesture(&tiler).emit_by_name::<()>("drag-update", &[&0.0f64, &30.0f64]);

            assert!(
                tiler.imp().grid_drag.borrow().is_none(),
                "the drag should have been abandoned once its seam stopped \
                 existing - its pixel snapshot describes a grid that is gone",
            );
            assert_eq!(
                *lm.imp().row_ratios.borrow(),
                vec![1.0, 1.0],
                "a drag with nowhere to land must not land somewhere else instead",
            );
        });
    }

    /// The half of that the bounds check cannot see, and the reason the drag
    /// carries a generation rather than a shape.
    ///
    /// A pane closing does not have to change the grid's dimensions. `grid_shape`
    /// scores the shape from the pane count and the window, and the stability bias
    /// exists precisely to keep the column count across a change in pane count -
    /// so six panes over three columns and two rows becomes five panes over three
    /// columns and two rows. Nothing about the ratio vectors' *lengths* moves, so
    /// every index an in-flight drag holds is still valid and `write_seam` accepts
    /// all of it. What did move is the contents: `ensure_grid_ratios` regenerated
    /// them to all-1.0, because its early return needs the pane count to be
    /// unchanged and here it isn't.
    ///
    /// So the drag has a seam to write to, and writes the wrong thing to it. It is
    /// still holding `px_a`/`px_b` and `ratio_a`/`ratio_b` from the arrangement the
    /// user had dragged the grid into, and it spends that stale total on two rows
    /// that have since been reset to equal - the seam leaves the pointer and lands
    /// somewhere nobody asked for. No crash, no panic, nothing in a log: just a
    /// layout that silently disagrees with the mouse, which is why this needs a
    /// test of its own rather than riding on the abort one above.
    #[test]
    fn a_reshape_that_keeps_its_shape_still_abandons_the_drag() {
        gtk_test(|| {
            let tiler = Tiler::new("/tmp".to_string());
            let lm = tiler.layout_mgr();

            // Six panes in this window is three columns over two rows.
            lm.imp().ensure_grid_ratios(6, 1470, 890);
            assert_eq!(
                lm.imp().grid_shape_dims.get(),
                (3, 2),
                "six panes in a 1470x890 window should be three columns of two",
            );

            // The user drags the one row seam a long way down, and keeps hold of
            // it. This is the arrangement the drag state below describes.
            lm.imp().row_ratios.borrow_mut()[0] = 1.5;
            lm.imp().row_ratios.borrow_mut()[1] = 0.5;
            let state = GridDragState {
                handle: Handle::GridRow(0),
                ratio_a: 1.5,
                ratio_b: 0.5,
                px_a: 667.0,
                px_b: 223.0,
                generation: lm.imp().grid_generation.get(),
            };
            *tiler.imp().grid_drag.borrow_mut() = Some(state);

            // An agent exits. Five panes, and the stability bias keeps the three
            // columns - so the grid is the same shape it was a moment ago.
            lm.imp().ensure_grid_ratios(5, 1470, 890);
            assert_eq!(
                lm.imp().grid_shape_dims.get(),
                (3, 2),
                "the whole point of this case: the shape did not change, so a \
                 drag comparing shapes would notice nothing",
            );
            assert_eq!(
                *lm.imp().row_ratios.borrow(),
                vec![1.0, 1.0],
                "...and yet the ratios were regenerated, because the early \
                 return in `ensure_grid_ratios` needs an unchanged pane count",
            );
            assert_ne!(
                lm.imp().grid_generation.get(),
                state.generation,
                "a rebuild has to be countable even when it is invisible in the \
                 dimensions",
            );

            drag_gesture(&tiler).emit_by_name::<()>("drag-update", &[&0.0f64, &10.0f64]);

            assert_eq!(
                *lm.imp().row_ratios.borrow(),
                vec![1.0, 1.0],
                "a stale drag must write nothing at all - spending its old total \
                 on the new rows is the seam jumping out from under the pointer",
            );
            assert!(
                tiler.imp().grid_drag.borrow().is_none(),
                "the drag measured itself against a grid that has been replaced; \
                 it has to be abandoned even though its seam still exists",
            );
        });
    }

    /// The other half of that guard: a drag whose seam is still there has to go
    /// on working. A bail-out broad enough to catch ordinary drags would trade a
    /// crash for a resize handle that silently does nothing.
    #[test]
    fn a_drag_whose_seam_survives_still_moves_it() {
        gtk_test(|| {
            let tiler = Tiler::new("/tmp".to_string());
            let lm = tiler.layout_mgr();

            lm.imp().ensure_grid_ratios(4, 1470, 890);
            *tiler.imp().grid_drag.borrow_mut() = Some(GridDragState {
                handle: Handle::GridRow(0),
                ratio_a: 1.0,
                ratio_b: 1.0,
                px_a: 400.0,
                px_b: 400.0,
                generation: lm.imp().grid_generation.get(),
            });

            drag_gesture(&tiler).emit_by_name::<()>("drag-update", &[&0.0f64, &80.0f64]);

            let ratios = lm.imp().row_ratios.borrow().clone();
            assert!(
                ratios[0] > ratios[1],
                "dragging the seam downward should grow the row above it, got {ratios:?}",
            );
            assert!(
                tiler.imp().grid_drag.borrow().is_some(),
                "a seam that is still there has no business cancelling the drag",
            );
        });
    }

    /// Neither controller may keep its tiler alive: both are installed on the
    /// widget their closures reach back into, so a strong capture is a cycle
    /// GObject cannot collect, and a closed project (`App::remove_project`) would
    /// leak its tiler, its layout manager, and every pane still parented to it.
    #[test]
    fn a_tiler_is_freed_when_its_project_closes() {
        gtk_test(|| {
            let tiler = Tiler::new("/tmp".to_string());
            let alive = tiler.downgrade();

            drop(tiler);

            assert!(
                alive.upgrade().is_none(),
                "the tiler outlived the last reference to it: something in \
                 `setup_resize` is holding it in a cycle",
            );
        });
    }

    /// The drag gesture `setup_resize` installed, so a test can hand it the
    /// motion a mouse would - there is no pointer to drag in a test, and the
    /// handler is the code under test.
    fn drag_gesture(tiler: &Tiler) -> GestureDrag {
        let controllers = tiler.observe_controllers();
        (0..controllers.n_items())
            .filter_map(|i| controllers.item(i))
            .find_map(|c| c.downcast::<GestureDrag>().ok())
            .expect("setup_resize adds a GestureDrag to every Tiler")
    }
}
