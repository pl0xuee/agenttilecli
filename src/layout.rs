/// Half the space between two neighbouring tiles.
///
/// `shrink` insets every side of every tile by this, so two tiles sharing a
/// seam end up `2 * gap()` apart while a tile against the window edge sits `gap()`
/// in from it. That ratio is right - an edge is one boundary and a seam is two
/// tiles' worth - but it does mean the number here reads as half of what the
/// eye actually measures between panes, which is how this came to be twice the
/// size it wanted to be.
fn gap() -> i32 {
    crate::config::get().gap.clamp(0, 40)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Mode {
    MasterStack,
    Monocle,
    Grid,
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Grid
    }
}

impl Mode {
    pub fn next(self) -> Self {
        match self {
            Mode::Grid => Mode::MasterStack,
            Mode::MasterStack => Mode::Monocle,
            Mode::Monocle => Mode::Grid,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

fn shrink(r: Rect) -> Rect {
    let gap = gap();
    Rect {
        x: r.x + gap,
        y: r.y + gap,
        width: (r.width - 2 * gap).max(0),
        height: (r.height - 2 * gap).max(0),
    }
}

/// Split `total` into `count` contiguous spans covering it exactly (last span absorbs remainder).
fn spans(total: i32, count: usize) -> Vec<(i32, i32)> {
    if count == 0 {
        return Vec::new();
    }
    let step = total / count as i32;
    (0..count)
        .map(|i| {
            let start = i as i32 * step;
            let len = if i == count - 1 { total - start } else { step };
            (start, len)
        })
        .collect()
}

/// Compute geometry for `n` panes (in stack order) within a `width`x`height` area.
/// `focus` is only consulted by `Mode::Monocle`.
pub fn compute(
    n: usize,
    focus: usize,
    mode: Mode,
    master_count: usize,
    master_ratio: f64,
    width: i32,
    height: i32,
) -> Vec<Rect> {
    if n == 0 || width <= 0 || height <= 0 {
        return vec![Rect::default(); n];
    }

    match mode {
        Mode::Monocle => {
            let focus = focus.min(n - 1);
            (0..n)
                .map(|i| {
                    if i == focus {
                        shrink(Rect {
                            x: 0,
                            y: 0,
                            width,
                            height,
                        })
                    } else {
                        Rect::default()
                    }
                })
                .collect()
        }
        Mode::Grid => grid(n, width, height),
        Mode::MasterStack => master_stack(n, master_count, master_ratio, width, height),
    }
}

fn master_stack(n: usize, master_count: usize, master_ratio: f64, width: i32, height: i32) -> Vec<Rect> {
    let master_count = master_count.clamp(1, n);
    let stack_count = n - master_count;

    let master_width = if stack_count == 0 {
        width
    } else {
        ((width as f64) * master_ratio.clamp(0.1, 0.9)) as i32
    };
    let stack_width = width - master_width;

    let mut rects = Vec::with_capacity(n);

    for (y, h) in spans(height, master_count) {
        rects.push(shrink(Rect {
            x: 0,
            y,
            width: master_width,
            height: h,
        }));
    }
    for (y, h) in spans(height, stack_count) {
        rects.push(shrink(Rect {
            x: master_width,
            y,
            width: stack_width,
            height: h,
        }));
    }

    rects
}

/// How far right to shift a row holding `used` of the available cells, so the
/// ones it does hold sit centred across the width rather than pushed against
/// the left.
///
/// Only a partial last row is ever off-centre, and only because the cells it
/// isn't using are all at one end. Every cell keeps the size it would have had
/// - stretching the survivors to fill the gap is the thing this layout
/// deliberately doesn't do, since it would make a lone third pane twice the
/// size of the two above it - so the leftover width is real either way. Putting
/// half of it on each side turns it from a hole in the corner into a margin,
/// which is what stops three panes reading as "four panes, one missing".
fn centering_offset(spans: &[(i32, i32)], used: usize, width: i32) -> i32 {
    if used == 0 || used >= spans.len() {
        return 0;
    }
    let first = spans[0].0;
    let last = spans[used - 1];
    let extent = last.0 + last.1 - first;
    (width - extent) / 2
}

/// Even grid: every pane gets an equal-size cell, sized from the full
/// `cols`x`rows` shape rather than from how many panes happen to land in
/// each row - so cell size stays identical for every pane no matter how
/// many panes there are. A partial last row keeps that size instead of
/// stretching its panes wider to fill the leftover (which would make them a
/// different size than every other pane), and is centred in it rather than
/// left-aligned - see `centering_offset`.
fn grid(n: usize, width: i32, height: i32) -> Vec<Rect> {
    let (cols, rows) = grid_shape(n, width, height, None);
    let row_spans = spans(height, rows);
    let col_spans = spans(width, cols);

    let mut rects = Vec::with_capacity(n);
    let mut remaining = n;
    for (y, h) in row_spans {
        let items_in_row = remaining.min(cols);
        remaining -= items_in_row;
        let offset = centering_offset(&col_spans, items_in_row, width);
        for &(x, w) in col_spans.iter().take(items_in_row) {
            rects.push(shrink(Rect {
                x: x + offset,
                y,
                width: w,
                height: h,
            }));
        }
    }
    rects
}

/// How strongly `grid_shape` favors keeping `prev_cols`' column count over
/// switching to a merely-somewhat-squarer alternative. Without this, adding
/// or closing a single pane can pick a completely different column count
/// from scratch (the scoring landscape shifts with `n`), which reshuffles
/// *every* pane's position and size even though only one pane actually
/// changed - the existing ones jump around for no reason a user watching
/// the screen can see. The bias only damps that churn for small changes; a
/// real aspect-ratio flip (see `grid_shape_flips_orientation_with_window_shape`)
/// still produces a score gap far bigger than this and reorients anyway. It
/// also never applies if keeping `prev_cols` would waste more empty cells
/// than picking fresh would (see `grid_shape`'s `reference_waste`) - since
/// `grid`/`grid_weighted` size every cell identically now, that waste is
/// visibly empty space, not just a rounding nicety worth damping churn for.
const GRID_STABILITY_BIAS: f64 = 1.0;

/// How strongly `grid_shape` penalizes empty cells (a partial last row/
/// column) relative to squareness.
///
/// Set so that a packed shape wins on equal terms but does not win when it
/// would have to elongate its cells to pack. At 0.5 it did: three panes in a
/// wide window packed into 3x1 - three tall slots 412px wide against 860px of
/// height - because one empty cell cost more than doubling the cells' aspect
/// ratio. 2x2 with the spare cell centred gives 618x430 instead.
///
/// 0.25 is where the curve flattens. Swept against "how far is the shape we
/// picked from the squarest one available", the worst case across every pane
/// count and window shape worth checking improves from 1.91x at 0.5 to 1.64x
/// here, and 0.2 buys 0.01 more for three additional stranded cells. Centring a
/// partial row (see `centering_offset`) is what makes the trade affordable at
/// all: an empty cell reads as a margin now rather than as a hole.
const WASTE_WEIGHT: f64 = 0.25;

/// The (columns, rows) shape `grid`/`grid_weighted` use for `n` panes,
/// chosen so cells stay as close to square as possible for the given
/// `width`x`height` area while leaving as few cells empty as it reasonably
/// can (see `WASTE_WEIGHT`). This is what makes the grid orient itself to
/// whatever shape the window currently is - a wide window favors more
/// columns (panes side by side), a tall one favors more rows (panes
/// stacked) - instead of always laying out the same way regardless of the
/// window's own aspect ratio.
///
/// `prev_cols`, when given, is the column count the grid was already using
/// (see `GRID_STABILITY_BIAS`) - pass `None` when there's no prior layout to
/// stay consistent with (e.g. the very first pane).
pub fn grid_shape(n: usize, width: i32, height: i32, prev_cols: Option<usize>) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    if width <= 0 || height <= 0 {
        let cols = (n as f64).sqrt().ceil() as usize;
        return (cols, n.div_ceil(cols));
    }

    let score_of = |cols: usize| {
        let rows = n.div_ceil(cols);
        let cell_ratio = (width as f64 / cols as f64) / (height as f64 / rows as f64);
        let waste = cols * rows - n;
        (rows, waste, cell_ratio.ln().abs() + waste as f64 * WASTE_WEIGHT)
    };

    // First pass: the shape a from-scratch pick (ignoring `prev_cols`
    // entirely) would settle for. Its waste is the ceiling `prev_cols` has to
    // stay at or under to still earn the stability bias in the second pass -
    // keeping a shape that strands *more* cells empty than picking fresh
    // would isn't worth avoiding a reshuffle for.
    let mut reference_waste = usize::MAX;
    let mut best_score = f64::MAX;
    for cols in 1..=n {
        let (_, waste, score) = score_of(cols);
        if score < best_score {
            best_score = score;
            reference_waste = waste;
        }
    }

    // Second pass: same scores, but `prev_cols` gets the stability bias if it
    // isn't wasting more cells than the fresh pick above would.
    let mut best = (1, n);
    let mut best_score = f64::MAX;
    for cols in 1..=n {
        let (rows, waste, mut score) = score_of(cols);
        if prev_cols == Some(cols) && waste <= reference_waste {
            score -= GRID_STABILITY_BIAS;
        }
        if score < best_score {
            best_score = score;
            best = (cols, rows);
        }
    }
    best
}

/// Split `total` into spans proportional to `ratios` (which need not sum to
/// 1 - they're normalized here). The last span absorbs any rounding
/// remainder so spans always cover `total` exactly.
pub fn weighted_spans(total: i32, ratios: &[f64]) -> Vec<(i32, i32)> {
    if ratios.is_empty() {
        return Vec::new();
    }
    let sum: f64 = ratios.iter().sum();
    let mut x = 0;
    let mut out = Vec::with_capacity(ratios.len());
    for (i, r) in ratios.iter().enumerate() {
        let w = if i == ratios.len() - 1 {
            total - x
        } else {
            ((total as f64) * (r / sum)) as i32
        };
        out.push((x, w));
        x += w;
    }
    out
}

/// Grid layout driven by adjustable ratios (one weight per row, and one
/// weight per column within each row, since a partial last row can have
/// fewer columns than the rest) instead of always-equal division. Passing
/// all-equal ratios reproduces `grid`'s output exactly.
pub fn grid_weighted(
    n: usize,
    width: i32,
    height: i32,
    row_ratios: &[f64],
    col_ratios: &[Vec<f64>],
) -> Vec<Rect> {
    if n == 0 || width <= 0 || height <= 0 {
        return vec![Rect::default(); n];
    }
    let mut rects = Vec::with_capacity(n);
    // How many real panes are still to be placed. This function is handed one
    // weight per column for *every* row, including a last row that holds fewer
    // panes than that, so it has to count them itself to know which row is the
    // partial one and centre it - see `centering_offset`.
    let mut remaining = n;
    for (row_i, (y, h)) in weighted_spans(height, row_ratios).into_iter().enumerate() {
        let ratios = col_ratios.get(row_i).map(Vec::as_slice).unwrap_or(&[]);
        let col_spans = weighted_spans(width, ratios);
        let items_in_row = remaining.min(col_spans.len());
        remaining -= items_in_row;
        let offset = centering_offset(&col_spans, items_in_row, width);
        for &(x, w) in col_spans.iter() {
            rects.push(shrink(Rect {
                x: x + offset,
                y,
                width: w,
                height: h,
            }));
        }
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_pane_fills_area() {
        let rects = compute(1, 0, Mode::MasterStack, 1, 0.55, 800, 600);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0], shrink(Rect { x: 0, y: 0, width: 800, height: 600 }));
    }

    #[test]
    fn master_stack_splits_columns() {
        let rects = compute(3, 0, Mode::MasterStack, 1, 0.5, 1000, 600);
        assert_eq!(rects.len(), 3);
        // master column
        assert_eq!(rects[0].x, gap());
        assert_eq!(rects[0].width, 500 - 2 * gap());
        assert_eq!(rects[0].height, 600 - 2 * gap());
        // stack column, two panes stacked vertically
        assert_eq!(rects[1].x, 500 + gap());
        assert_eq!(rects[2].x, 500 + gap());
        assert_eq!(rects[1].y, gap());
        assert!(rects[2].y > rects[1].y);
        // stack panes fill the full stack height between them
        assert_eq!(rects[1].height + rects[2].height, 600 - 4 * gap());
    }

    #[test]
    fn master_stack_no_stack_uses_full_width() {
        let rects = compute(2, 0, Mode::MasterStack, 2, 0.55, 800, 600);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].width, 800 - 2 * gap());
        assert_eq!(rects[1].width, 800 - 2 * gap());
    }

    #[test]
    fn monocle_only_shows_focused() {
        let rects = compute(3, 1, Mode::Monocle, 1, 0.55, 800, 600);
        assert_eq!(rects[0], Rect::default());
        assert_eq!(rects[1], shrink(Rect { x: 0, y: 0, width: 800, height: 600 }));
        assert_eq!(rects[2], Rect::default());
    }

    #[test]
    fn grid_covers_all_panes() {
        let rects = compute(4, 0, Mode::Grid, 1, 0.55, 800, 600);
        assert_eq!(rects.len(), 4);
        for r in &rects {
            assert!(r.width > 0 && r.height > 0);
        }
    }

    #[test]
    fn master_count_clamped_to_n() {
        let rects = compute(2, 0, Mode::MasterStack, 5, 0.55, 800, 600);
        assert_eq!(rects.len(), 2);
        // both panes should end up in the master column since master_count clamps to n
        assert_eq!(rects[0].width, rects[1].width);
    }

    #[test]
    fn grid_weighted_equal_ratios_matches_grid() {
        let (cols, rows) = grid_shape(3, 800, 600, None);
        let row_ratios = vec![1.0; rows];
        // Every row gets `cols` ratios (not just however many panes actually
        // land in it) - matching how `grid` itself now sizes a partial
        // row's cells the same as every other row's, rather than
        // stretching them to fill the leftover width.
        let col_ratios: Vec<Vec<f64>> = vec![vec![1.0; cols]; rows];

        // Padding every row out to `cols` ratios means `grid_weighted` (which
        // doesn't know `n`, just the ratios it's handed) also returns
        // `rows * cols` rects - the trailing ones for a partial row are
        // exactly what `grid` itself leaves out via `n`, so only the first
        // `n` need to match (the real, tiler.rs.allocate()'s own `zip`
        // against actual children drops the rest the same way).
        let weighted = grid_weighted(3, 800, 600, &row_ratios, &col_ratios);
        let plain = compute(3, 0, Mode::Grid, 1, 0.55, 800, 600);
        assert_eq!(&weighted[..3], &plain[..]);
    }

    #[test]
    fn grid_cells_are_uniform_size_regardless_of_pane_count() {
        // 3 panes in a squarish area picks a 2x2 shape with a partial last
        // row (2 panes, then 1) - all three panes must still end up exactly
        // the same size as each other, not the lone third pane stretched to
        // fill the row's full width.
        let rects = compute(3, 0, Mode::Grid, 1, 0.55, 800, 600);
        assert_eq!(rects.len(), 3);
        for r in &rects[1..] {
            assert_eq!(r.width, rects[0].width);
            assert_eq!(r.height, rects[0].height);
        }
    }

    /// No arrangement is allowed to be much worse-proportioned than the best one
    /// available for that many panes in that window.
    ///
    /// This is the property the whole `grid_shape` scorer exists to hold, and
    /// the one it was quietly failing: three panes in a wide window packed into
    /// 3x1 - three slivers 412px wide against 860px of height - because a single
    /// empty cell was scored as costing more than doubling every cell's aspect
    /// ratio.
    ///
    /// The bound is a ratio against the squarest shape rather than an absolute,
    /// because some of the skew is the window's own: one pane in a 1600x700
    /// window is 1600x700 and no scorer can help that.
    #[test]
    fn no_grid_is_far_worse_proportioned_than_it_had_to_be() {
        const WINDOWS: &[(i32, i32)] = &[
            (1235, 860),
            (900, 860),
            (600, 1200),
            (1600, 700),
            (1000, 1000),
            (1400, 600),
            (700, 1400),
            (1920, 1080),
            (1235, 500),
        ];
        let skew = |cols: usize, rows: usize, w: i32, h: i32| {
            let ar = (f64::from(w) / cols as f64) / (f64::from(h) / rows as f64);
            ar.max(1.0 / ar)
        };

        for &(w, h) in WINDOWS {
            for n in 1..=8usize {
                let (cols, rows) = grid_shape(n, w, h, None);
                let got = skew(cols, rows, w, h);
                let best = (1..=n)
                    .map(|c| skew(c, n.div_ceil(c), w, h))
                    .fold(f64::MAX, f64::min);
                assert!(
                    got <= best * 1.7,
                    "{n} panes in {w}x{h} chose {cols}x{rows} (skew {got:.2}) \
                     when {best:.2} was available",
                );
            }
        }
    }

    /// Three panes in a 2x2 shape leave one cell empty, and an empty cell in
    /// the bottom-right corner reads as a pane that failed to open. Centring the
    /// partial row turns the same leftover width into a margin.
    #[test]
    fn a_partial_last_row_is_centred_rather_than_left_aligned() {
        let rects = compute(3, 0, Mode::Grid, 1, 0.55, 800, 600);
        assert_eq!(rects.len(), 3);

        // The two full-row panes are untouched: one against the left margin,
        // one against the right.
        let left_margin = rects[0].x;
        let right_margin = 800 - (rects[1].x + rects[1].width);
        assert_eq!(left_margin, right_margin, "the full row still spans the width");

        // The lone pane on the last row sits with equal space either side of it
        // - and is still exactly the size of the two above it.
        let lone = rects[2];
        let left = lone.x;
        let right = 800 - (lone.x + lone.width);
        assert!(
            (left - right).abs() <= 1,
            "partial row off-centre: {left} left vs {right} right",
        );
        assert_eq!(lone.width, rects[0].width, "and it is not stretched");
        assert!(lone.x > rects[0].x, "it moved right of the left column");
    }

    /// A row that is full has nothing to centre, and must not be nudged.
    #[test]
    fn a_full_grid_is_not_shifted() {
        let rects = compute(4, 0, Mode::Grid, 1, 0.55, 800, 600);
        assert_eq!(rects.len(), 4);
        assert_eq!(rects[0].x, rects[2].x, "columns line up down the grid");
        assert_eq!(rects[1].x, rects[3].x);
        assert_eq!(rects[0].x, gap(), "and the first column keeps its margin");
    }

    #[test]
    fn grid_weighted_respects_custom_ratios() {
        // Two side-by-side panes, dragged so the first takes 3x the second.
        let rects = grid_weighted(2, 800, 600, &[1.0], &[vec![3.0, 1.0]]);
        assert_eq!(rects.len(), 2);
        assert!(rects[0].width > rects[1].width * 2);
    }

    #[test]
    fn grid_shape_flips_orientation_with_window_shape() {
        // Wide window: 2 panes side by side.
        assert_eq!(grid_shape(2, 1200, 400, None), (2, 1));
        // Same 2 panes, tall window: stacked instead.
        assert_eq!(grid_shape(2, 400, 1200, None), (1, 2));
    }

    #[test]
    fn grid_shape_stays_put_for_a_marginally_squarer_alternative() {
        // 4 panes at 2 cols is already in use; 5 panes in the same
        // roughly-square area scores only marginally better at 3 cols, so
        // the existing 2-column arrangement should win rather than
        // reshuffling every pane's position for a small squareness gain.
        assert_eq!(grid_shape(5, 900, 900, Some(2)), (2, 3));
        // A 16:10-ish window (the app's own default size) growing from 4
        // panes (2 cols) to 5: 3 cols scores a bit better here, but not by
        // enough to justify reshuffling every existing pane.
        assert_eq!(grid_shape(5, 1280, 854, Some(2)), (2, 3));
        // But a real aspect-ratio flip still overrides the bias.
        assert_eq!(grid_shape(5, 2000, 300, Some(2)), (5, 1));
    }

    #[test]
    fn grid_shape_reorients_rather_than_accumulating_empty_cells() {
        // Growing one pane at a time from 4 to 9 in a 16:10-ish window
        // (mirroring how `Tiler` feeds its own previous column count back
        // in on every spawn) used to get stuck at 4 cols once picked for 7
        // panes: at 9 panes that's a 4x3 shape with 3 empty cells, even
        // though a fully-packed 3x3 shape (0 empty cells) was right there.
        // The stability bias must not keep a shape that wastes more cells
        // than a fresh pick would.
        let mut cols = None;
        for n in 4..=9 {
            cols = Some(grid_shape(n, 1470, 890, cols).0);
        }
        let (cols, rows) = grid_shape(9, 1470, 890, cols);
        assert_eq!(cols * rows, 9, "expected a fully-packed shape for 9 panes, got {cols}x{rows}");
    }
}
