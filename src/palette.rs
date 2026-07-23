//! The one palette, read back out of the stylesheet that documents it.
//!
//! `style.css` declares the graphite ramp and the semantic colours in a
//! `@define-color` block, and the chrome derives everything from those names.
//! The terminal can't: VTE paints its own background, foreground, cursor,
//! selection and 16-colour ANSI palette, none of which GTK CSS reaches. So
//! `pane.rs` used to carry its own hand-copied hexes of the same colours.
//!
//! That is exactly how the selection tint got left on the retired blue-grey
//! ramp in 0.3.1 - the stylesheet moved and the copy in `pane.rs` didn't, and
//! nothing anywhere said so. A colour that exists in two files drifts; a colour
//! that exists in one can't.
//!
//! So the stylesheet stays the single source - it's where the ramp is actually
//! explained - and this module parses the `@define-color` declarations out of
//! the very same `include_str!` the CSS provider is handed, at the same names
//! the CSS selectors use. Colours the terminal derives rather than shares (the
//! selection tint, which is @filament mixed over whichever surface the pane is
//! currently painted in) are computed here too, so they follow the ramp on their
//! own instead of needing to be recomputed by hand whenever it moves.
//!
//! Deliberately free of GTK: parsing and mixing are plain Rust over `u8`s, so
//! the tests that guard the ramp run on a headless machine rather than skipping
//! with everything else that needs a display.

use gtk4::gdk;

/// The stylesheet, byte for byte the one `main::load_css` installs.
const STYLESHEET: &str = include_str!("style.css");

/// An 8-bit RGB colour. Opaque by construction - every colour in this app is,
/// including the ones CSS writes as `alpha(...)`, since those are composited
/// by GTK rather than stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Parses `#rrggbb`. `None` for any other shape - including the `#rgb`
    /// short form and named colours, neither of which the palette uses.
    pub fn from_hex(s: &str) -> Option<Self> {
        let digits = s.strip_prefix('#')?;
        if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
        Some(Rgb {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
        })
    }

    /// `self` and `other` blended, `factor` being the share of `other` - the
    /// same direction as GTK CSS's own `mix(a, b, f)`, so a mix written here
    /// and a mix written in the stylesheet mean the same thing.
    pub fn mix(self, other: Self, factor: f32) -> Self {
        let f = factor.clamp(0.0, 1.0);
        let blend = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f).round() as u8;
        Rgb {
            r: blend(self.r, other.r),
            g: blend(self.g, other.g),
            b: blend(self.b, other.b),
        }
    }

    /// The GDK colour VTE wants. Built component-wise rather than through
    /// `RGBA::parse`, so no hex string has to be reconstructed only to be
    /// re-parsed by GDK - and so nothing here can fail at runtime.
    pub fn to_rgba(self) -> gdk::RGBA {
        let c = |v: u8| v as f32 / 255.0;
        gdk::RGBA::new(c(self.r), c(self.g), c(self.b), 1.0)
    }
}

/// Every `@define-color <name> <#rrggbb>;` in the stylesheet, in source order.
///
/// A deliberately small parser rather than a CSS one: the block it reads is
/// six lines of two-token declarations at the top of a file this crate owns,
/// and `the_stylesheet_parses_without_errors` already holds GTK's opinion of
/// the rest of it. Anything it doesn't recognise it skips, so a declaration
/// written in a form this doesn't handle surfaces as a missing name from
/// `color()` - caught by `every_required_colour_is_defined` - rather than as a
/// silently wrong colour.
fn declarations() -> impl Iterator<Item = (&'static str, Rgb)> {
    STYLESHEET.lines().filter_map(|line| {
        let rest = line.trim().strip_prefix("@define-color")?;
        let mut tokens = rest.split_whitespace();
        let name = tokens.next()?;
        let value = tokens.next()?.trim_end_matches(';');
        Some((name, Rgb::from_hex(value)?))
    })
}

/// The colour the stylesheet defines as `name` (without the `@`).
///
/// Panics if it isn't defined, which is a programming error rather than a
/// runtime condition - the names are fixed strings in this crate, checked by
/// `every_required_colour_is_defined`.
pub fn color(name: &str) -> Rgb {
    declarations()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| c)
        .unwrap_or_else(|| panic!("style.css defines no @define-color named {name:?}"))
}

/// How much of the light goes into a selection. Low enough that selected text
/// keeps its own colour and its syntax highlighting - which matters because
/// selecting is how you copy here (see `clipboard`), so it happens over real
/// output rather than over a blank prompt.
const SELECTION_TINT: f32 = 0.24;

/// The selection background for a pane currently painted in `surface`.
///
/// Mixed rather than fixed: the pane has two surfaces (focused and not), and a
/// selection tint that matched only one of them would be the same bug this
/// module exists to prevent, one rung further down.
pub fn selection(surface: Rgb) -> Rgb {
    surface.mix(color("filament"), SELECTION_TINT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parser has to actually find the ramp. Without this, a stylesheet
    /// edit that put the declarations into a shape `declarations()` skips
    /// would leave it returning nothing at all - and since `color()` panics on
    /// a name it can't find, that reads as "the colour is missing" rather than
    /// "the parser stopped working".
    ///
    /// Which names in particular have to resolve is `pane::tests::
    /// every_colour_the_terminal_needs_resolves`'s business, since that's
    /// where they're asked for.
    #[test]
    fn the_ramp_is_parsed_out_of_the_stylesheet() {
        let found: Vec<_> = declarations().collect();
        assert!(
            found.len() >= 10,
            "parsed only {} declarations out of style.css: {found:?}",
            found.len(),
        );
        // A spot check with a known value, so a parser that returned the right
        // *number* of wrong colours doesn't pass either.
        assert_eq!(
            color("tile"),
            Rgb {
                r: 0x1c,
                g: 0x24,
                b: 0x2a
            },
        );
    }

    /// The surfaces, floor to top rung, in the order they're meant to climb.
    const LADDER: [&str; 7] = [
        "field", "rack", "tile", "tile-lit", "chip", "hairline", "edge",
    ];

    /// What makes the ramp gunmetal rather than graphite, stated as arithmetic
    /// so it survives someone "tidying" a hex.
    ///
    /// The cast is the whole of it. Green sitting exactly on the midpoint of red
    /// and blue is a neutral blue-grey - it reads as ink, a colour of absence.
    /// Blued steel keeps a trace of teal, green a point or two *above* that
    /// midpoint, and that difference is the entire reason the surfaces read as a
    /// material. It is also invisible in a diff: every rung here is a plausible
    /// dark grey, and nothing but this test says which ones are the right dark
    /// greys.
    ///
    /// The ladder also has to keep climbing, and the cast has to keep widening as
    /// it does. A fixed red-to-blue offset is a smaller and smaller fraction of
    /// the channel values as they lighten, so holding it flat would let the cast
    /// drain out of the top of the ramp and leave `@edge` a neutral grey rule
    /// against six tinted surfaces below it.
    #[test]
    fn the_ramp_is_gunmetal_all_the_way_up() {
        let mut previous_spread = 0i16;
        let mut previous_light = -1i16;

        for name in LADDER {
            let c = color(name);
            let (r, g, b) = (i16::from(c.r), i16::from(c.g), i16::from(c.b));

            // The teal cast: 2g - (r + b) is twice the distance above the
            // midpoint, so >= 1 is "at least half a point of green".
            assert!(
                2 * g - (r + b) >= 1,
                "@{name} ({c:?}) has no teal in it - green sits at or below the \
                 midpoint of red and blue, which is graphite, not gunmetal",
            );

            let spread = b - r;
            assert!(
                spread >= previous_spread,
                "@{name} ({c:?}) narrows the cool cast to {spread} from {previous_spread} \
                 on the rung below - the cast has to widen as the ladder climbs, or it \
                 drains out of the light end",
            );
            previous_spread = spread;

            assert!(
                r > previous_light,
                "@{name} ({c:?}) is not lighter than the rung below it - the ladder \
                 has to climb, or depth stops reading as depth",
            );
            previous_light = r;
        }
    }

    /// The focused pane is meant to read as lit, which it can't if the two
    /// surfaces are the same colour - and it reads as a different *material*
    /// rather than the same one lit if they're far apart. Half a rung is the
    /// intent; this holds it to somewhere between "visible" and "a new rung".
    #[test]
    fn the_focused_surface_is_a_visible_lift_but_not_a_new_rung() {
        let base = color("tile");
        let lit = color("tile-lit");
        for (a, b) in [(base.r, lit.r), (base.g, lit.g), (base.b, lit.b)] {
            let lift = b as i16 - a as i16;
            assert!(
                (2..=6).contains(&lift),
                "focus lift of {lift} is outside the visible-but-subtle range \
                 (base {base:?}, lit {lit:?})",
            );
        }
    }

    /// The bug this module was written for, as a test: the selection tint has
    /// to sit on whichever surface it's actually drawn over. Before, it was a
    /// fixed hex mixed against a surface two releases old.
    #[test]
    fn the_selection_follows_the_surface_it_sits_on() {
        let surface = color("tile");
        let unfocused = selection(surface);
        let focused = selection(color("tile-lit"));
        assert_ne!(
            unfocused, focused,
            "the selection tint ignored the surface it was mixed over",
        );
        // And it's a tint, not a fill: every channel still sits nearer the
        // surface it's drawn on than the light it's mixed with, or selected
        // text stops being readable as text. Stated per channel rather than on
        // one of them, since @filament is a near-white - it has no channel that
        // stands in for "how much light got mixed in" the way a saturated
        // accent's would.
        let filament = color("filament");
        for (channel, from_surface, from_light) in [
            ("r", unfocused.r.abs_diff(surface.r), unfocused.r.abs_diff(filament.r)),
            ("g", unfocused.g.abs_diff(surface.g), unfocused.g.abs_diff(filament.g)),
            ("b", unfocused.b.abs_diff(surface.b), unfocused.b.abs_diff(filament.b)),
        ] {
            assert!(
                from_surface < from_light,
                "the selection tint has drifted toward a solid fill of light \
                 on {channel}: {unfocused:?} between {surface:?} and {filament:?}",
            );
        }
    }

    #[test]
    fn hex_parsing_takes_the_forms_the_palette_uses_and_rejects_the_rest() {
        assert_eq!(
            Rgb::from_hex("#324654"),
            Some(Rgb {
                r: 0x32,
                g: 0x46,
                b: 0x54
            }),
        );
        for bad in ["#fff", "324654", "#gggggg", "#3246545", ""] {
            assert_eq!(Rgb::from_hex(bad), None, "{bad:?} parsed as a colour");
        }
    }

    /// `mix` has to agree with GTK's, since the same intent is written both
    /// ways in this codebase - `alpha(@filament, ...)` in the stylesheet and
    /// `Rgb::mix` here.
    #[test]
    fn mixing_runs_from_the_receiver_toward_the_argument() {
        let black = Rgb { r: 0, g: 0, b: 0 };
        let white = Rgb {
            r: 255,
            g: 255,
            b: 255,
        };
        assert_eq!(black.mix(white, 0.0), black);
        assert_eq!(black.mix(white, 1.0), white);
        assert_eq!(
            black.mix(white, 0.5),
            Rgb {
                r: 128,
                g: 128,
                b: 128
            },
        );
    }
}
