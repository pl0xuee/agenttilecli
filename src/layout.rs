const GAP: i32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    Rect {
        x: r.x + GAP,
        y: r.y + GAP,
        width: (r.width - 2 * GAP).max(0),
        height: (r.height - 2 * GAP).max(0),
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

/// Even grid: every pane gets an equal-size cell. Rows are split evenly by
/// height; within each row, width is split evenly among only the panes that
/// land in that row, so a partial last row stretches to fill the width
/// instead of leaving dead space next to a fixed column width.
fn grid(n: usize, width: i32, height: i32) -> Vec<Rect> {
    let (cols, rows) = grid_shape(n, width, height);
    let row_spans = spans(height, rows);

    let mut rects = Vec::with_capacity(n);
    let mut remaining = n;
    for (y, h) in row_spans {
        let items_in_row = remaining.min(cols);
        remaining -= items_in_row;
        for (x, w) in spans(width, items_in_row) {
            rects.push(shrink(Rect {
                x,
                y,
                width: w,
                height: h,
            }));
        }
    }
    rects
}

/// The (columns, rows) shape `grid`/`grid_weighted` use for `n` panes,
/// chosen so cells stay as close to square as possible for the given
/// `width`x`height` area. This is what makes the grid orient itself to
/// whatever shape the window currently is - a wide window favors more
/// columns (panes side by side), a tall one favors more rows (panes
/// stacked) - instead of always laying out the same way regardless of the
/// window's own aspect ratio. Among shapes that are equally square, the one
/// wasting fewer cells (i.e. a smaller partial last row/column) wins.
pub fn grid_shape(n: usize, width: i32, height: i32) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    if width <= 0 || height <= 0 {
        let cols = (n as f64).sqrt().ceil() as usize;
        return (cols, n.div_ceil(cols));
    }

    let mut best = (1, n);
    let mut best_score = f64::MAX;
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let cell_ratio =
            (width as f64 / cols as f64) / (height as f64 / rows as f64);
        let waste = (cols * rows - n) as f64;
        let score = cell_ratio.ln().abs() + waste * 0.05;
        if score < best_score {
            best_score = score;
            best = (cols, rows);
        }
    }
    best
}

/// How many panes land in each row of the grid (the last row may be partial).
pub fn row_item_counts(n: usize, cols: usize, rows: usize) -> Vec<usize> {
    let mut remaining = n;
    (0..rows)
        .map(|_| {
            let c = remaining.min(cols);
            remaining -= c;
            c
        })
        .collect()
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
    for (row_i, (y, h)) in weighted_spans(height, row_ratios).into_iter().enumerate() {
        let ratios = col_ratios.get(row_i).map(Vec::as_slice).unwrap_or(&[]);
        for (x, w) in weighted_spans(width, ratios) {
            rects.push(shrink(Rect {
                x,
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
        assert_eq!(rects[0].x, GAP);
        assert_eq!(rects[0].width, 500 - 2 * GAP);
        assert_eq!(rects[0].height, 600 - 2 * GAP);
        // stack column, two panes stacked vertically
        assert_eq!(rects[1].x, 500 + GAP);
        assert_eq!(rects[2].x, 500 + GAP);
        assert_eq!(rects[1].y, GAP);
        assert!(rects[2].y > rects[1].y);
        // stack panes fill the full stack height between them
        assert_eq!(rects[1].height + rects[2].height, 600 - 4 * GAP);
    }

    #[test]
    fn master_stack_no_stack_uses_full_width() {
        let rects = compute(2, 0, Mode::MasterStack, 2, 0.55, 800, 600);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].width, 800 - 2 * GAP);
        assert_eq!(rects[1].width, 800 - 2 * GAP);
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
        let (cols, rows) = grid_shape(3, 800, 600);
        let counts = row_item_counts(3, cols, rows);
        let row_ratios = vec![1.0; rows];
        let col_ratios: Vec<Vec<f64>> = counts.iter().map(|&c| vec![1.0; c]).collect();

        let weighted = grid_weighted(3, 800, 600, &row_ratios, &col_ratios);
        let plain = compute(3, 0, Mode::Grid, 1, 0.55, 800, 600);
        assert_eq!(weighted, plain);
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
        assert_eq!(grid_shape(2, 1200, 400), (2, 1));
        // Same 2 panes, tall window: stacked instead.
        assert_eq!(grid_shape(2, 400, 1200), (1, 2));
    }
}
