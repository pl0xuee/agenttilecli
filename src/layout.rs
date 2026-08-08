/// How far the master column is allowed to be dragged, as a share of the width.
///
/// Named here because `layout` is what actually divides the space, and because
/// the number was previously a bare `0.1, 0.9` written out at four separate
/// sites - the two keybindings, the seam drag, and the restore path - plus a
/// fifth copy in `session`, which held a *saved* ratio to a range it had guessed
/// at. Five copies of one decision is four chances for it to stop being one
/// decision.
///
/// Not zero and one, because either end is a master column with no width or a
/// stack with none: a pane allocated nothing still exists, still holds a pty,
/// and cannot be got back to without knowing the keybinding that widens it.
pub(crate) const MASTER_RATIO_MIN: f64 = 0.1;
pub(crate) const MASTER_RATIO_MAX: f64 = 0.9;

/// Half the space between two neighbouring tiles.
///
/// `shrink` insets every side of every tile by this, so two tiles sharing a
/// seam end up `2 * gap()` apart while a tile against the window edge sits `gap()`
/// in from it. That ratio is right - an edge is one boundary and a seam is two
/// tiles' worth - but it does mean the number here reads as half of what the
/// eye actually measures between panes, which is how this came to be twice the
/// size it wanted to be.
/// Read from `appearance` rather than straight from the config, because the
/// preferences dialog moves it while the app is running and the config file is
/// only where it starts.
fn gap() -> i32 {
    crate::appearance::get().gap
}

/// The docked editor column's share of the workspace.
///
/// The editor is not one of the tiles the mode arranges - it is a fixture,
/// docked on the workspace's left so it always sits beside the project tree
/// that opens files into it, with the agents tiling in whatever remains. A
/// fixed share rather than a per-mode answer, because the editor's usefulness
/// is a function of line length, not of how many agents happen to be running.
///
/// Under half on purpose: the agents are the workspace's job and the editor is
/// a place to look at one file, so the majority of the width stays theirs.
const EDITOR_SHARE: f64 = 0.42;

/// How the workspace divides when an editor pane is docked at its left edge:
/// `(editor width, agents width)`.
///
/// No editor, no column. No agents, no division - a lone editor takes the
/// whole workspace rather than leaving 58% of the window empty beside it.
pub fn editor_split(width: i32, editor: bool, agents: usize) -> (i32, i32) {
    let width = width.max(0);
    if !editor {
        return (0, width);
    }
    if agents == 0 {
        return (width, 0);
    }
    let editor_width = (f64::from(width) * EDITOR_SHARE) as i32;
    (editor_width, width - editor_width)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Mode {
    MasterStack,
    Monocle,
    #[default]
    Grid,
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

// `pub(crate)` for one caller: the layout manager insets the docked editor's
// column with the same gap every tile gets, so the seam between the editor and
// the agents reads exactly like the seam between any two tiles.
pub(crate) fn shrink(r: Rect) -> Rect {
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
        ((width as f64) * master_ratio.clamp(MASTER_RATIO_MIN, MASTER_RATIO_MAX)) as i32
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

/// The column spans for one row of the grid: `panes_in_row` cells dividing the
/// full `width` between them, in proportion to their own weights.
///
/// This is where a partial last row is decided, and the decision changed.
///
/// It used to be that every row divided the width by the *shape's* column
/// count, so a partial row's cells kept the size a full row's had and the
/// leftover was pushed out as a margin - half of it on each side, to centre
/// what was there. The argument was that uniform cell size is worth more than
/// filled space, and that centring turns a hole in the corner into a margin.
///
/// Half of that is true. It does turn one hole into two, and two holes flanking
/// a pane do not read as a margin - they read as the two panes that failed to
/// open. Three agents in a 2x2 left a quarter of the workspace empty and drew
/// the eye straight to it, which is a great deal of a tiling window manager's
/// window to spend on a principle about cell size. Every tiling WM this app
/// sits beside on the same desktop - i3, sway, bspwm, Hyprland - fills the
/// screen, and fills it for the same reason: a workspace that leaves a quarter
/// of itself blank looks broken, not deliberate.
///
/// So the survivors stretch. A lone third pane is now twice the width of the
/// two above it, which is the cost the old comment named and declined to pay,
/// and it is worth paying: an unequal grid that fills its window looks
/// arranged, and an equal one that doesn't looks unfinished.
///
/// Taking a prefix of the weights rather than renormalising them by hand is
/// what keeps a dragged seam dragged: `weighted_spans` normalises whatever it
/// is handed, so a row whose two panes were pulled to 3:1 stays 3:1 when the
/// third pane closes and the row goes from three cells to two.
pub fn row_col_spans(width: i32, ratios: &[f64], panes_in_row: usize) -> Vec<(i32, i32)> {
    let used = panes_in_row.min(ratios.len());
    if used == 0 {
        return Vec::new();
    }
    weighted_spans(width, &ratios[..used])
}

/// Even grid: every row divides the width between the panes actually in it, and
/// every column the height between its rows. A full grid gives every pane an
/// identical cell; a partial last row shares the width between the panes it has
/// rather than leaving the missing cells as empty space - see `row_col_spans`.
fn grid(n: usize, width: i32, height: i32) -> Vec<Rect> {
    let (cols, rows) = grid_shape(n, width, height, None);
    let equal = vec![1.0; cols];

    let mut rects = Vec::with_capacity(n);
    let mut remaining = n;
    for (y, h) in spans(height, rows) {
        let items_in_row = remaining.min(cols);
        remaining -= items_in_row;
        for (x, w) in row_col_spans(width, &equal, items_in_row) {
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
    // partial one and divide the width between only the panes it has - see
    // `row_col_spans`.
    let mut remaining = n;
    for (row_i, (y, h)) in weighted_spans(height, row_ratios).into_iter().enumerate() {
        let ratios = col_ratios.get(row_i).map(Vec::as_slice).unwrap_or(&[]);
        let items_in_row = remaining.min(ratios.len());
        remaining -= items_in_row;
        for (x, w) in row_col_spans(width, ratios, items_in_row) {
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

    /// The docked editor's column: present only when both sides of the split
    /// have something to show, always summing back to the width it divided,
    /// and always leaving the agents the larger share.
    #[test]
    fn the_editor_column_only_exists_when_both_sides_do() {
        assert_eq!(editor_split(1000, false, 3), (0, 1000));
        assert_eq!(editor_split(1000, true, 0), (1000, 0));

        let (editor, agents) = editor_split(1000, true, 3);
        assert_eq!(editor + agents, 1000, "the split must cover the workspace");
        assert!(editor > 0, "a docked editor gets a real column");
        assert!(
            agents > editor,
            "the agents keep the larger share - they are the workspace's job",
        );

        // A degenerate width stays degenerate rather than going negative.
        assert_eq!(editor_split(-50, true, 2), (0, 0));
    }

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

    /// Every row of the grid spans the full width, including a partial one.
    ///
    /// This replaces `grid_cells_are_uniform_size_regardless_of_pane_count`,
    /// which pinned the opposite: a lone third pane kept the width of the two
    /// above it and the leftover was centred out as margin. That left a quarter
    /// of the workspace blank, which is the thing this changed for - see
    /// `row_col_spans`. Cells are still uniform *within* a full row; what is no
    /// longer promised is uniformity across a partial one.
    #[test]
    fn every_grid_row_fills_the_width() {
        let rects = compute(3, 0, Mode::Grid, 1, 0.55, 800, 600);
        assert_eq!(rects.len(), 3);

        // The full row: two equal cells, spanning the width between them.
        assert_eq!(rects[0].width, rects[1].width, "a full row divides evenly");
        assert_eq!(rects[0].x, gap(), "and starts at the left margin");
        assert_eq!(
            rects[1].x + rects[1].width,
            800 - gap(),
            "and ends at the right one",
        );

        // The partial row: one pane, taking all of it.
        let lone = rects[2];
        assert_eq!(lone.x, gap(), "the lone pane starts at the left margin too");
        assert_eq!(
            lone.x + lone.width,
            800 - gap(),
            "and reaches the right edge instead of stopping short",
        );
        assert!(
            lone.width > rects[0].width,
            "which makes it wider than the two above it - the trade this makes",
        );
        assert_eq!(lone.height, rects[0].height, "rows still divide evenly");
    }

    /// A dragged seam survives its row losing a pane.
    ///
    /// `row_col_spans` takes a *prefix* of the weights rather than rebuilding
    /// them, and `weighted_spans` normalises whatever it is handed - so two
    /// panes left at 3:1 in a three-cell row stay at 3:1 when they are the only
    /// two left, instead of snapping back to even.
    #[test]
    fn a_shortened_row_keeps_the_proportions_it_was_dragged_to() {
        let spans = row_col_spans(800, &[3.0, 1.0, 1.0], 2);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].0, 0, "the row still starts at the left");
        assert_eq!(
            spans[1].0 + spans[1].1,
            800,
            "and the two of them still cover the width",
        );
        assert_eq!(spans[0].1, 600, "3:1 of 800 is 600");
        assert_eq!(spans[1].1, 200);
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

    /// No pane count leaves a hole in the workspace.
    ///
    /// The property `every_grid_row_fills_the_width` checks for three panes,
    /// swept across the counts and window shapes a person actually opens. This
    /// is the guarantee the centred layout could not make, and the reason it
    /// went: every row reaches both margins, so the only empty space in a grid
    /// is the gutters between tiles.
    #[test]
    fn no_grid_leaves_empty_space_at_the_end_of_a_row() {
        const WINDOWS: &[(i32, i32)] = &[(1235, 860), (900, 860), (600, 1200), (1600, 700)];
        for &(w, h) in WINDOWS {
            for n in 1..=9usize {
                let rects = compute(n, 0, Mode::Grid, 1, 0.55, w, h);
                let (cols, _) = grid_shape(n, w, h, None);

                // Group the rects back into rows and check each one's extent.
                for (row_i, row) in rects.chunks(cols).enumerate() {
                    let first = row.first().expect("a row with no panes in it");
                    let last = row.last().expect("a row with no panes in it");
                    assert_eq!(
                        first.x,
                        gap(),
                        "{n} panes in {w}x{h}: row {row_i} starts short of the margin",
                    );
                    assert_eq!(
                        last.x + last.width,
                        w - gap(),
                        "{n} panes in {w}x{h}: row {row_i} stops short of the right edge",
                    );
                }
            }
        }
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
