//! The layout manager: a `GtkLayoutManager` that arranges a `Tiler`'s children
//! in a grid, a master/stack column, or a single fullscreen pane.
//!
//! Pure geometry lives in `crate::layout` and is shared with tests that run
//! without a display; this is the GTK glue that feeds it the widget tree's real
//! child order and allocates what it hands back.

use std::cell::{Cell, RefCell};

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{glib, graphene, gsk, LayoutManager, Widget};

use crate::layout::{self, Mode};

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
    pub(super) handle: Handle,
    pub(super) ratio_a: f64,
    pub(super) ratio_b: f64,
    pub(super) px_a: f64,
    pub(super) px_b: f64,
}

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
        pub(crate) fn ensure_grid_ratios(&self, n: usize, width: i32, height: i32) {
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
    pub(super) fn new() -> Self {
        glib::Object::new()
    }
}
