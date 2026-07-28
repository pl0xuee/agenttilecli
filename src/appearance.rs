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

/// How see-through the app is allowed to get, either surface.
///
/// The floor is 0.5 rather than 0. A window that can be taken to fully
/// transparent is a window someone can lose - and the failure is silent, since
/// what they see afterwards is their desktop, which looks exactly like the app
/// having closed. Half is already further than the design intends to go.
const OPACITY_MIN: f64 = 0.5;
const OPACITY_MAX: f64 = 1.0;

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

/// Reads the config's appearance and installs it. Called once, from `main`.
pub fn install() {
    let loaded = Appearance::from_config();
    ACTIVE.with(|active| *active.borrow_mut() = Some(loaded));
}

/// Replaces the live appearance, clamping whatever it is handed.
///
/// Callers are responsible for actually applying it - re-emitting the CSS and
/// repainting the panes - because this module can't reach the widgets and
/// shouldn't try to.
// Its caller is the preferences dialog, which is the next thing to be built;
// until then the only thing that moves the appearance is the config file, read
// once at startup. The allow comes off with the dialog.
#[allow(dead_code)]
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
/// The panes take no alpha from here at all - they are opaque, they are painted
/// by VTE, and they are the reason the ordering has something to stay below.
pub fn content_css(font_scale: f64) -> String {
    let Appearance { window_opacity, .. } = get();
    format!(
        ".scaled-content {{ font-size: {font_scale}em; \
         background-color: alpha(@field, {window_opacity:.3}); }}\n\
         .sidebar {{ background-color: alpha(@rack, {window_opacity:.3}); }}"
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
    }
}
