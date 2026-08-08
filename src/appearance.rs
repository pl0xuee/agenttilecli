//! The numbers a person is allowed to move.
//!
//! `style.css` states everything about how this app looks except the handful of
//! values that are settings rather than decisions - the two opacities, the gap
//! between tiles, the terminal's font. A stylesheet compiled into the binary
//! cannot state those, because their whole point is that they are not fixed at
//! build time. So they live here, and the two that are colours are written back
//! out as CSS through a provider that outranks the static one.
//!
//! Process-wide, for the same reason `config` is: they are read from the layout
//! manager, from every pane, and from the window, none of which have any
//! business being handed an appearance through four signatures that never vary.
//! Unlike `config` they are also *written* - the preferences dialog moves them
//! while the app is running - so this is a `RefCell` rather than a `OnceLock`,
//! held in a thread-local because GTK is single-threaded and everything that
//! touches these is a GTK callback.
//!
//! The defaults come from `config`, which is why `install` is called from `main`
//! after the config is loaded rather than from the initialiser here: a default
//! that ignored the config file would be a config file that does nothing.

use std::cell::RefCell;

use crate::config;
use crate::session;

/// How see-through the app is allowed to get, either surface.
///
/// The floor is 0.5 rather than 0. A window that can be taken to fully
/// transparent is a window someone can lose - and the failure is silent, since
/// what they see afterwards is their desktop, which looks exactly like the app
/// having closed. Half is already further than the design intends to go.
const OPACITY_MIN: f64 = 0.5;
const OPACITY_MAX: f64 = 1.0;

/// The rack's fill while the split view holds it *over* the panes - the
/// collapsed mode below `COLLAPSE_WIDTH_PX` - rather than beside them.
///
/// Side by side, the rack's glass has nothing behind it but the desktop, which
/// is the whole arrangement `content_css` documents. Collapsed, the same glass
/// would sit on the floor and the panes, and glass over glass only ever
/// composites denser - 0.92 over 0.92 is within a percent of opaque, wearing a
/// translucency slider that claims otherwise. So the overlaid rack does not
/// pretend: it is pinned near-opaque like the dialogs (0.94) and popovers
/// (0.92), because a surface floating over content is elevation, and on this
/// ramp elevation is opacity. Deliberately not a function of `window_opacity`:
/// a sheet whose density tracked a slider about *floors* would just be the
/// compositing accident with extra steps.
pub const OVERLAY_ALPHA: f64 = 0.97;

/// The live appearance. Cloned out rather than borrowed across a call, so a
/// setter firing from inside a redraw can't panic on an outstanding borrow.
#[derive(Clone, PartialEq, Debug)]
pub struct Appearance {
    /// The workspace floor's alpha - the gutters between tiles, the strip behind
    /// the header bar, and everything else `@field` is painted on.
    pub window_opacity: f64,
    /// The terminal surface's alpha. Defaults to fully opaque, and the reason is
    /// in `style.css`'s "Glass" note: a terminal is the one surface here whose
    /// job is being read.
    pub pane_opacity: f64,
    /// Half the space between neighbouring tiles, in pixels.
    pub gap: i32,
    /// The terminal font, as a Pango description ("Fira Mono 10"). Empty means
    /// the desktop's own monospace, which is what every pane used before there
    /// was a way to say otherwise.
    pub font: String,
}

impl Appearance {
    /// The appearance the config file asks for, clamped to what is usable.
    fn from_config() -> Self {
        let config = config::get();
        Appearance {
            window_opacity: clamp_opacity(config.window_opacity),
            pane_opacity: clamp_opacity(config.pane_opacity),
            gap: config.gap.clamp(0, 40),
            font: config.font.clone(),
        }
    }

    /// The config's appearance with the session's adjustments laid over it.
    ///
    /// Two sources rather than one because they answer different questions.
    /// `config.toml` is where someone writes down what they want the app to
    /// open as; the session is where the preferences dialog remembers what they
    /// last dragged a slider to. A field the dialog has never touched stays
    /// `None` and the file keeps speaking for it, so editing the file still
    /// works on a machine that has a session - which is the one thing a config
    /// file cannot be allowed to stop doing.
    fn resolved(saved: &session::Appearance) -> Self {
        let base = Appearance::from_config();
        Appearance {
            window_opacity: saved.window_opacity.map_or(base.window_opacity, clamp_opacity),
            pane_opacity: saved.pane_opacity.map_or(base.pane_opacity, clamp_opacity),
            gap: saved.gap.map_or(base.gap, |gap| gap.clamp(0, 40)),
            font: base.font,
        }
    }

    /// What to write into the session: only the fields that differ from what
    /// the config file already says, so a value the user never changed doesn't
    /// get frozen into the session and start overriding later edits to the file.
    pub fn overrides(&self) -> session::Appearance {
        let base = Appearance::from_config();
        session::Appearance {
            window_opacity: (self.window_opacity != base.window_opacity).then_some(self.window_opacity),
            pane_opacity: (self.pane_opacity != base.pane_opacity).then_some(self.pane_opacity),
            gap: (self.gap != base.gap).then_some(self.gap),
        }
    }
}

fn clamp_opacity(value: f64) -> f64 {
    if value.is_nan() {
        return OPACITY_MAX;
    }
    value.clamp(OPACITY_MIN, OPACITY_MAX)
}

thread_local! {
    static ACTIVE: RefCell<Option<Appearance>> = const { RefCell::new(None) };
}

/// The appearance in force. Falls back to the config's own defaults when
/// `install` was never called, which is every unit test in this crate.
pub fn get() -> Appearance {
    ACTIVE.with(|active| {
        active
            .borrow_mut()
            .get_or_insert_with(Appearance::from_config)
            .clone()
    })
}

/// Reads the config's appearance and installs it. Called once, from `main`,
/// before the session has been read - so the window has a floor to paint from
/// the moment it exists.
pub fn install() {
    let loaded = Appearance::from_config();
    ACTIVE.with(|active| *active.borrow_mut() = Some(loaded));
}

/// Lays the session's saved adjustments over what the config file asked for.
/// Called once the session has been read, which is after the window is built.
pub fn restore(saved: &session::Appearance) {
    let resolved = Appearance::resolved(saved);
    ACTIVE.with(|active| *active.borrow_mut() = Some(resolved));
}

/// Replaces the live appearance, clamping whatever it is handed.
///
/// Callers are responsible for actually applying it - re-emitting the CSS and
/// repainting the panes - because this module can't reach the widgets and
/// shouldn't try to.
pub fn set(next: Appearance) {
    let next = Appearance {
        window_opacity: clamp_opacity(next.window_opacity),
        pane_opacity: clamp_opacity(next.pane_opacity),
        gap: next.gap.clamp(0, 40),
        font: next.font,
    };
    ACTIVE.with(|active| *active.borrow_mut() = Some(next));
}

/// The dynamic half of the stylesheet: the text size, and the two chrome
/// surfaces whose alpha is a setting.
///
/// The font size lands on `.scaled-content` because the content chrome scales
/// with the panes and the rack deliberately doesn't. The two fills are here
/// rather than in `style.css` because their alpha is the setting, and a
/// stylesheet compiled into the binary can't hold one.
///
/// **They share an alpha, and that is the whole of why this is one function.**
/// The ramp says @field is the floor and @rack sits a rung above it, below the
/// panes - chrome recesses, content stands out. Translucency cannot preserve an
/// ordering like that on its own: composited over an unknown desktop, the
/// surface with the lower alpha is dragged further toward whatever is behind the
/// window, so a rack at 0.78 over a floor at 0.92 comes out *lighter* than the
/// floor it is supposed to sit below, and lighter than the panes it is supposed
/// to recede from. Given the same alpha, both surfaces are pulled the same
/// distance in the same direction and the ramp's ordering survives whatever the
/// user's wallpaper happens to be.
///
/// The panes are here too now, and for a while they weren't - the note that used
/// to sit on this line said they took no alpha from here at all, because they
/// were painted by VTE rather than by CSS. That was true and it was also the
/// whole bug: `pane_opacity` was handed to VTE as the alpha on the terminal's
/// background colour, and VTE's GTK4 backend discards it and clears its surface
/// opaquely regardless. The slider in the preferences dialog moved a number that
/// reached the terminal and died there, which is the worst kind of setting - one
/// that looks implemented.
///
/// So the pane's fill is a CSS fill like every other surface in this window, and
/// `pane::apply_theme` tells VTE not to clear its own background when there is
/// glass to see through. What the terminal sits on is then `.pane`'s fill, at
/// the alpha written here.
///
/// The two pane rules carry *different* colours at the *same* alpha, which is
/// the elevation ladder surviving translucency again: @tile-lit has to stay
/// above @tile after both have been dragged toward the desktop, and equal alphas
/// are what guarantees they are dragged the same distance.
///
/// The floor is emitted twice and in neither of the obvious places, which needs
/// saying. It used to be one rule on `.scaled-content` - the toolbar view holding
/// the header and every tiler - and that single fill sat underneath the panes,
/// which capped how glassy a pane could be at whatever the floor was (alpha only
/// climbs: 0.6 over 0.92 is 0.968). Cutting it up is what makes `pane_opacity`
/// mean its own number. So the workspace floor is now painted by `Tiler` itself,
/// masked to the gutters, and what is left here are the two regions of the
/// content half that no tiler covers: the header bar, and the empty state that
/// stands in for a project with nothing running.
///
/// `.workspace-floor` is that empty state (see `App::build_empty_state`). It is a
/// sibling of the tiler rather than a child, so it cannot inherit anything from
/// it and has to be given a fill of its own, or a project with no agents would
/// open onto a window with nothing in it but a desktop.
///
/// It is painted in @tile at `pane_opacity` - the *pane* surface, not the floor,
/// despite the class name it has carried since it was written. That was a bug
/// you could see: an empty project showed @field through 0.92 of glass, at
/// (12,16,19) against a desktop, and starting a single agent replaced the whole
/// workspace with one opaque tile at (31,40,47). Twenty-one points of lightness,
/// across the entire right-hand side of the window, on starting an agent - which
/// reads exactly like the application changing its background colour, and was
/// reported as such.
///
/// The floor is not wrong about what it is; it was wrong about where it goes.
/// Its job is the gutter *between* tiles - a recess the plate sits above - and a
/// workspace with no tiles in it has no gutters to draw. What it had instead was
/// a full-window recess with nothing raised out of it, which is a hole. No
/// editor does this: VS Code and Zed both paint an empty editor in the same
/// colour as a full one, because the surface a workspace is made of should not
/// depend on whether anything is on it yet.
///
/// So the empty state now stands where a pane would, on the surface a pane
/// would have, and starting the first agent changes the tone by the five points
/// between @tile and @tile-lit rather than by twenty-one.
///
/// The *colour* is the pane's and the *alpha* is the chrome's, which looks like
/// having it both ways and is the point. Almost all of that twenty-one points
/// was the colour: @field sits sixteen rungs below @tile before any alpha is
/// applied, so no opacity setting could have closed it. The alpha was worth
/// three of them. Taking @tile at `window_opacity` therefore fixes the jump
/// while leaving this the one screen in the app you can still see your desktop
/// through - which is what it was for, and what it would have lost by taking
/// `pane_opacity` and going opaque at the shipped default.
///
/// It does mean a bright wallpaper still lightens this surface a little where
/// it would not lighten a pane, since the pane above it is opaque and this is
/// not. That difference is the glass doing exactly what the slider says it
/// does, it is a few points rather than twenty-one, and anyone who wants it
/// gone can close it from the other end by lowering `pane_opacity` until the
/// panes are glass too.
///
/// `.top-bar` is libadwaita's own class on the revealer `AdwToolbarView` puts its
/// top bars in, and it is deliberately not `headerbar` - which is what this rule
/// said first, and which left a visible hole. The revealer spans the full strip
/// (y 0..46 in a default window); the header bar inside it paints a shorter box,
/// so a 1px line above it and a 3px band below it belonged to no surface at all
/// once the floor stopped being painted behind everything. That reads as a bright
/// seam across the window under the header, over any wallpaper lighter than the
/// chrome.
///
/// Exactly one of the two may paint it, for the reason this whole function
/// exists: two translucent fills stacked composite to more opaque than either, so
/// a floor on the revealer *and* on the header bar would make the strip visibly
/// denser than the gutters it is supposed to match. The revealer is the one that
/// covers the whole region, so the revealer is the one that paints; the header bar
/// itself stays `transparent` (see `headerbar_bg_color` in `style.css`).
/// How dark a halo the chrome's text carries at a given surface opacity.
///
/// Nothing at 1.0, rising as the fill thins. The whole type palette in this app
/// assumes it is being read against a dark surface - @faint and @muted are
/// chosen to sit *quietly* on @rack, which only works while @rack is actually
/// there. At `window_opacity` 0.5 it half isn't, and what shows through instead
/// is whatever the desktop happens to be. Measured over a bright wallpaper at
/// 0.5, with the window's own render composited onto it:
///
///     PROJECTS heading      2.69:1
///     project row name      2.46:1
///     "Open a project..."   1.12:1
///     version footer        1.00:1   <- glyph and surface the same colour
///
/// 1.00:1 is not "hard to read", it is not drawn. And the fix cannot be to
/// brighten the type, because the direction of the problem depends on the
/// wallpaper: brighter text helps over a dark one and makes it worse over a
/// bright one. What is missing is not lightness, it is *ground*.
///
/// So the text carries its own. A dark halo under every glyph is the ground the
/// fill stopped providing, it costs nothing when the fill is doing its job
/// (0.0 at full opacity), and it is what desktop icon labels have always done
/// for exactly this reason - text over a picture nobody chose.
///
/// The curve is linear in the missing fill and capped below 1.0: a halo dark
/// enough to guarantee contrast over pure white would be a black outline around
/// every label, which reads as embossed rather than legible. 0.85 at the 0.5
/// floor is where the type stays clean and the worst case still resolves.
fn text_halo(opacity: f64) -> f64 {
    ((1.0 - opacity) * 1.7).clamp(0.0, 0.85)
}

/// How far the quiet type is dragged toward @text as the surface thins.
///
/// The halo above is half the answer and measurably the smaller half - it took
/// the version footer over a white wallpaper from 1.00:1 to 1.55:1, which is
/// the difference between invisible and nearly invisible. The arithmetic is why:
/// at alpha `a` over white, the surface lands at `255 - a*(255 - fill)`, so at
/// 0.5 it is at least 127 *whatever colour the fill is*. No shadow rescues light
/// grey on light grey; the ground cannot be made dark from underneath.
///
/// What is left is the figure. @faint and @muted are quiet because they are only
/// a little lighter than @rack, and that relationship is the thing translucency
/// destroys - against a mid-tone composite they are not quiet, they are gone. So
/// as the fill thins they climb toward @text, which is the one ink in the
/// palette with enough range to survive a surface the app does not control.
///
/// This does flatten the type hierarchy at the bottom of the range, and that is
/// the right trade: a footnote that is dimmer than a heading is worth having,
/// and a footnote that is not drawn at all is not. At the shipped default
/// (0.92) the lift is 0.13 - a nudge nobody will see - and it only becomes a
/// real change below about 0.8, which is where the surface stops being one.
fn text_lift(opacity: f64) -> f64 {
    ((1.0 - opacity) * 1.6).clamp(0.0, 1.0)
}

pub fn content_css(font_scale: f64) -> String {
    let Appearance {
        window_opacity,
        pane_opacity,
        ..
    } = get();
    // Two haloes, because the two halves of the window thin out independently:
    // the drawer and the rail follow `window_opacity`, while the pane chrome
    // sits on a surface that follows `pane_opacity`.
    let chrome_halo = text_halo(window_opacity);
    let pane_halo = text_halo(pane_opacity);
    // The lift follows whichever surface the text is actually sitting on. The
    // drawer, the rail and the header strip all thin with `window_opacity`; only
    // the labels inside a tile ride `pane_opacity`.
    let chrome_lift = text_lift(window_opacity);
    let pane_lift = text_lift(pane_opacity);
    format!(
        ".scaled-content {{ font-size: {font_scale}em; }}\n\
         .sidebar, .rail {{ text-shadow: 0 0 3px alpha(@shadow, {chrome_halo:.3}), \
           0 1px 2px alpha(@shadow, {chrome_halo:.3}); }}\n\
         .scaled-content {{ text-shadow: 0 0 3px alpha(@shadow, {pane_halo:.3}), \
           0 1px 2px alpha(@shadow, {pane_halo:.3}); }}\n\
         .sidebar-version, .sidebar-add, .sidebar-header-count, .sidebar-tree-note, \
         .empty-hint {{ color: mix(@faint, @text, {chrome_lift:.3}); }}\n\
         .sidebar-header-label, .header-title-sub, .header-action, .rack-overflow, \
         .sidebar-row-icon, button.sidebar-agent, button.sidebar-tree-file \
         {{ color: mix(@muted, @text, {chrome_lift:.3}); }}\n\
         .sidebar-agent-label, button.sidebar-tree-dir, .empty-description \
         {{ color: mix(@dim, @text, {chrome_lift:.3}); }}\n\
         .pane-head-label {{ color: mix(@muted, @text, {pane_lift:.3}); }}\n\
         .pane-close, .pane-editor-action {{ color: mix(@faint, @text, {pane_lift:.3}); }}\n\
         .scaled-content .top-bar {{ background-color: alpha(@field, {window_opacity:.3}); }}\n\
         .workspace-floor {{ background-color: alpha(@tile, {window_opacity:.3}); }}\n\
         .sidebar {{ background-color: alpha(@rack, {window_opacity:.3}); }}\n\
         .sidebar.overlay {{ background-color: alpha(@rack, {OVERLAY_ALPHA:.3}); }}\n\
         .rail {{ background-color: alpha(@rack, {window_opacity:.3}); }}\n\
         .pane {{ background-color: alpha(@tile, {pane_opacity:.3}); }}\n\
         .pane.focused {{ background-color: alpha(@tile-lit, {pane_opacity:.3}); }}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clamp is the only thing between a hand-edited config and a window
    /// nobody can find, so it holds at both ends and through the value floats
    /// have that integers don't.
    #[test]
    fn opacity_is_clamped_into_a_window_you_can_still_see() {
        assert_eq!(clamp_opacity(0.0), OPACITY_MIN);
        assert_eq!(clamp_opacity(-3.0), OPACITY_MIN);
        assert_eq!(clamp_opacity(2.5), OPACITY_MAX);
        assert_eq!(clamp_opacity(0.8), 0.8);
        assert_eq!(
            clamp_opacity(f64::NAN),
            OPACITY_MAX,
            "a NaN opacity has to land on opaque - clamp returns NaN unchanged, \
             and a NaN alpha renders as a window that isn't there",
        );
    }

    /// The rule has to name both properties and the colour has to arrive as an
    /// `alpha()` of the ramp rather than a hex, or a change to `@field` would
    /// stop reaching the floor.
    #[test]
    fn the_dynamic_rule_carries_the_size_and_the_floor() {
        set(Appearance {
            window_opacity: 0.9,
            pane_opacity: 1.0,
            gap: 6,
            font: String::new(),
        });
        let css = content_css(1.25);
        assert!(css.contains("font-size: 1.25em"), "{css}");
        assert!(css.contains("alpha(@field, 0.900)"), "{css}");
    }

    /// Both legibility curves have to be *nothing* at full opacity and real at
    /// the floor, and they have to be nothing at exactly 1.0 rather than nearly
    /// nothing - an opaque window has a perfectly good ground of its own, and a
    /// halo on it is an outline round every label for no reason.
    #[test]
    fn the_legibility_compensation_costs_nothing_on_an_opaque_window() {
        assert_eq!(text_halo(1.0), 0.0, "an opaque surface needs no halo");
        assert_eq!(text_lift(1.0), 0.0, "nor any lift");

        // The shipped default is a nudge nobody should be able to see.
        assert!(text_halo(0.92) < 0.15, "halo at the default: {}", text_halo(0.92));
        assert!(text_lift(0.92) < 0.15, "lift at the default: {}", text_lift(0.92));

        // And at the floor, where the fill has stopped being a surface, both
        // are doing real work - but the halo stays short of a black outline.
        assert!(text_halo(OPACITY_MIN) > 0.6);
        assert!(text_halo(OPACITY_MIN) <= 0.85, "a halo that dark reads as embossed");
        assert!(text_lift(OPACITY_MIN) > 0.6);
        assert!(text_lift(OPACITY_MIN) <= 1.0, "never past @text itself");

        // Monotonic, so dragging the slider never brightens the type twice for
        // one step or reverses on some interval.
        let mut previous = (0.0, 0.0);
        for step in 0..=50 {
            let opacity = 1.0 - f64::from(step) * 0.01;
            let (halo, lift) = (text_halo(opacity), text_lift(opacity));
            assert!(halo >= previous.0 && lift >= previous.1, "not monotonic at {opacity}");
            previous = (halo, lift);
        }
    }

    /// An empty workspace and a workspace with one agent in it have to be the
    /// same colour.
    ///
    /// This is a bug that shipped and was reported from a screenshot: the empty
    /// state took `@field` at `window_opacity` while a pane took `@tile-lit` at
    /// `pane_opacity`, so starting a single agent swapped the entire right-hand
    /// side of the window from (12,16,19) to (31,40,47). Twenty-one points, over
    /// the whole workspace, with nothing in the interface to explain it - which
    /// reads as the application changing its own background.
    ///
    /// Asserted on the emitted rule rather than on rendered pixels because that
    /// is where the mistake was: both surfaces were correct about their own
    /// colour and wrong about being different ones.
    #[test]
    fn an_empty_workspace_is_the_same_surface_as_a_full_one() {
        set(Appearance {
            window_opacity: 0.8,
            pane_opacity: 0.95,
            gap: 6,
            font: String::new(),
        });
        let css = content_css(1.0);

        // The empty state takes the pane's *colour* - which is where twenty of
        // the twenty-one points lived - at the chrome's *alpha*, so it stays
        // the one screen you can see the desktop through.
        assert!(
            css.contains(".workspace-floor { background-color: alpha(@tile, 0.800); }"),
            "the empty state must stand on the pane surface, and stay glass:\n{css}",
        );
        // ...and emphatically not the floor's, which is what the gutters
        // between real tiles are for.
        assert!(
            !css.contains(".workspace-floor { background-color: alpha(@field"),
            "the empty state is back on the floor - see this test's doc:\n{css}",
        );
        // The floor itself still exists, for the region that genuinely is one.
        assert!(css.contains(".top-bar { background-color: alpha(@field, 0.800); }"), "{css}");
    }

    /// A session that has never seen the dialog must leave the config file
    /// entirely in charge.
    #[test]
    fn an_untouched_session_defers_to_the_config() {
        assert_eq!(
            Appearance::resolved(&session::Appearance::default()),
            Appearance::from_config(),
        );
    }

    /// And a field the dialog *has* touched wins over the file, for that field
    /// alone.
    #[test]
    fn a_saved_adjustment_overrides_only_its_own_field() {
        let base = Appearance::from_config();
        let resolved = Appearance::resolved(&session::Appearance {
            window_opacity: Some(0.7),
            pane_opacity: None,
            gap: None,
        });
        assert_eq!(resolved.window_opacity, 0.7);
        assert_eq!(resolved.pane_opacity, base.pane_opacity);
        assert_eq!(resolved.gap, base.gap);
    }

    /// A session is a file on disk that anything could have written, so its
    /// values go through the same clamp the config's do.
    #[test]
    fn a_saved_adjustment_is_still_clamped() {
        let resolved = Appearance::resolved(&session::Appearance {
            window_opacity: Some(-1.0),
            pane_opacity: Some(9.0),
            gap: Some(4000),
        });
        assert_eq!(resolved.window_opacity, OPACITY_MIN);
        assert_eq!(resolved.pane_opacity, OPACITY_MAX);
        assert_eq!(resolved.gap, 40);
    }

    /// The half of the split that keeps the config file working: a value equal
    /// to what the file already says is not written to the session at all.
    ///
    /// Without this every field would be saved on the first adjustment of any
    /// one of them, and from then on the config file would be dead - edits to it
    /// silently overridden by a session recording values the user never chose.
    #[test]
    fn only_a_changed_value_is_written_to_the_session() {
        let mut appearance = Appearance::from_config();
        assert_eq!(
            appearance.overrides(),
            session::Appearance::default(),
            "an unmodified appearance writes nothing",
        );

        appearance.gap += 3;
        let overrides = appearance.overrides();
        assert_eq!(overrides.gap, Some(appearance.gap));
        assert_eq!(overrides.window_opacity, None);
        assert_eq!(overrides.pane_opacity, None);
    }

    /// The strip behind the header bar is painted by its container, and by only
    /// its container.
    ///
    /// Both halves of that sentence are a bug that happened. Painting `headerbar`
    /// instead of `.top-bar` covers a shorter box than the strip, and since the
    /// floor is no longer painted behind everything, the 1px above it and the 3px
    /// below it end up painted by nothing at all - a bright seam across the window
    /// wherever the desktop is lighter than the chrome. Painting *both* would
    /// stack two translucent fills into something denser than the gutters the strip
    /// is supposed to match, which is the trap the rest of this module is about.
    #[test]
    fn the_header_strip_is_painted_once_by_its_container() {
        set(Appearance {
            window_opacity: 0.9,
            pane_opacity: 1.0,
            gap: 6,
            font: String::new(),
        });
        let css = content_css(1.0);
        assert!(
            css.contains(".top-bar {") && css.contains("alpha(@field, 0.900)"),
            "the top-bar strip has no floor, so it will show the desktop: {css}",
        );
        assert!(
            !css.contains("headerbar"),
            "the header bar paints the strip as well as its container, which \
             composites to denser than the floor it should match: {css}",
        );
    }

    /// The pane fills have to be here, and this is the load-bearing test of the
    /// pair.
    ///
    /// `pane::apply_theme` stops VTE clearing its own surface whenever the pane
    /// opacity is below 1.0, on the understanding that something else is painting
    /// what the text sits on. That something is these two rules. Delete them and
    /// the app does not fall back to an opaque terminal - it renders agent output
    /// onto whatever happens to be behind the window, which is unreadable, and
    /// nothing in the build says so.
    #[test]
    fn a_glass_pane_is_given_a_fill_to_sit_on() {
        set(Appearance {
            window_opacity: 1.0,
            pane_opacity: 0.6,
            gap: 6,
            font: String::new(),
        });
        let css = content_css(1.0);
        assert!(
            css.contains(".pane {") && css.contains("alpha(@tile, 0.600)"),
            "an unfocused glass pane has no fill behind its text: {css}",
        );
        assert!(
            css.contains(".pane.focused {") && css.contains("alpha(@tile-lit, 0.600)"),
            "a focused glass pane has no fill behind its text: {css}",
        );
    }

    /// The two pane surfaces share an alpha for the same reason the floor and the
    /// rack do - see the note on `content_css`.
    #[test]
    fn the_two_pane_surfaces_are_equally_transparent() {
        set(Appearance {
            window_opacity: 1.0,
            pane_opacity: 0.7,
            gap: 6,
            font: String::new(),
        });
        let css = content_css(1.0);
        assert!(css.contains("alpha(@tile, 0.700)"), "{css}");
        assert!(css.contains("alpha(@tile-lit, 0.700)"), "{css}");
    }

    /// The floor and the rack have to be emitted at the *same* alpha, or the
    /// ramp's ordering stops surviving contact with the user's wallpaper: the
    /// glassier of the two is dragged further toward whatever is behind the
    /// window, and the rack ends up sitting visually above the panes it is
    /// supposed to recede from. This is the bug the first build of the glass
    /// actually had, so it is worth a test rather than a comment.
    #[test]
    fn the_floor_and_the_rack_are_equally_transparent() {
        set(Appearance {
            window_opacity: 0.75,
            pane_opacity: 1.0,
            gap: 6,
            font: String::new(),
        });
        let css = content_css(1.0);
        assert!(css.contains("alpha(@field, 0.750)"), "{css}");
        assert!(css.contains("alpha(@rack, 0.750)"), "{css}");
        // The rail is the third surface in the same law: rack colour, floor
        // alpha. A rail denser than the drawer beside it reads as two racks.
        assert!(
            css.contains(".rail { background-color: alpha(@rack, 0.750); }"),
            "{css}"
        );
    }

    /// The overlaid rack is pinned, not scaled: whatever the floor slider says,
    /// the collapsed sheet is emitted at `OVERLAY_ALPHA` and nothing else.
    /// Emitted at a slider value whose own alpha would be visibly different,
    /// so the two rules can't be satisfied by one another.
    #[test]
    fn the_overlaid_rack_is_pinned_near_opaque() {
        set(Appearance {
            window_opacity: 0.6,
            pane_opacity: 1.0,
            gap: 6,
            font: String::new(),
        });
        let css = content_css(1.0);
        assert!(
            css.contains(".sidebar.overlay { background-color: alpha(@rack, 0.970); }"),
            "the collapsed rack has no elevated-sheet fill, so it will composite \
             its slider glass onto the panes beneath it: {css}",
        );
        assert!(
            css.contains(".sidebar { background-color: alpha(@rack, 0.600); }"),
            "the side-by-side rack should still follow the slider: {css}",
        );
    }
}
