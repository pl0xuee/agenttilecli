//! The desktop's palette, when the desktop has one.
//!
//! Omarchy keeps the live theme at `~/.local/state/omarchy/current/theme`, a
//! symlink into whichever theme directory `omarchy theme set` last pointed it
//! at, and every theme in that scheme carries a `colors.toml` naming its
//! backgrounds, its foregrounds and its sixteen terminal colours. That file is
//! the whole integration surface: read it and this app wears the same palette
//! as the terminal, the editor and the bar sitting beside it.
//!
//! What this module does *not* do is replace `style.css`. The gunmetal ramp is
//! still where the relationships are written down - which surface is the floor,
//! which is the lit one, how far the ink sits from the paper - and this module
//! only ever restates those relationships in somebody else's colours. A machine
//! with no Omarchy on it loads exactly the stylesheet it always did.
//!
//! GTK-free on purpose, like `hooks` and `agent`: the mapping is the part worth
//! testing and the tests should run on a machine with no display.

use std::cell::RefCell;
use std::path::PathBuf;

use crate::palette::Rgb;

/// Whether the theme is drawn for a dark surface or a light one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Dark,
    Light,
}

/// Every colour `colors.toml` is allowed to carry, all of it optional.
///
/// Optional because the format is a convention rather than a schema: Nebula
/// ships no `orange` and no `brown`, and a hand-written theme can leave out the
/// whole bright row. Deserialising into required fields would turn each of those
/// into "this is not a theme", which is how an integration ends up working only
/// against the themes its author happened to test.
#[derive(serde::Deserialize)]
struct Colors {
    mode: Option<String>,
    accent: Option<String>,
    selection: Option<String>,
    background: Option<String>,
    dark_background: Option<String>,
    darker_background: Option<String>,
    lighter_background: Option<String>,
    foreground: Option<String>,
    dark_foreground: Option<String>,
    light_foreground: Option<String>,
    bright_foreground: Option<String>,
    red: Option<String>,
    yellow: Option<String>,
    green: Option<String>,
    cyan: Option<String>,
    blue: Option<String>,
    magenta: Option<String>,
    bright_red: Option<String>,
    bright_yellow: Option<String>,
    bright_green: Option<String>,
    bright_cyan: Option<String>,
    bright_blue: Option<String>,
    bright_magenta: Option<String>,
}

impl Colors {
    /// A field as a colour, or `fallback` when it is absent or malformed. A
    /// theme with one bad hex in it is still a theme; refusing the whole file
    /// over a typo would drop the user back to gunmetal with nothing said.
    fn get(field: &Option<String>, fallback: Rgb) -> Rgb {
        field
            .as_deref()
            .and_then(Rgb::from_hex)
            .unwrap_or(fallback)
    }
}

/// Perceived brightness, Rec. 601. Coarse, and exactly precise enough for the
/// only question asked of it: which of two of a theme's own greys is deeper.
fn luminance(c: Rgb) -> f32 {
    0.299 * c.r as f32 + 0.587 * c.g as f32 + 0.114 * c.b as f32
}

/// Pure black, the one colour no theme supplies and `shadow` is cast in.
const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };

/// A parsed Omarchy theme, already restated in this app's own colour names.
#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub mode: Mode,
    ramp: Vec<(&'static str, Rgb)>,
    ansi: [Rgb; 16],
    selection: Rgb,
}

impl Theme {
    /// Reads a theme's `colors.toml`. `None` when the file isn't a theme -
    /// unparseable TOML, or missing the two colours everything else is derived
    /// from.
    pub fn parse(name: &str, colors_toml: &str) -> Option<Theme> {
        let colors: Colors = toml::from_str(colors_toml).ok()?;

        // The two that cannot be derived from anything else, and whose absence
        // means this file is somebody's unrelated config rather than a theme.
        let background = colors.background.as_deref().and_then(Rgb::from_hex)?;
        let foreground = colors.foreground.as_deref().and_then(Rgb::from_hex)?;

        let mode = match colors.mode.as_deref().map(str::trim) {
            Some("light") => Mode::Light,
            _ => Mode::Dark,
        };

        // ── The four surfaces ────────────────────────────────────────────
        // Sorted by brightness rather than assigned by name, and that is the
        // whole of what makes a light theme work here.
        //
        // The names describe a *dark* theme's ladder: in Catppuccin,
        // `darker_background` really is the deepest colour in the file. In
        // White it is `#e8e8e8` and `lighter_background` is `#c0c0c0` - the
        // greyest thing in the theme - because in a light theme "lighter"
        // stopped meaning "further from the floor" and started meaning
        // "further from the paper". A mapping that trusted the key names would
        // put pure white on the floor and the deepest grey on the focused
        // pane: a light theme rendered inside out, on every light theme
        // Omarchy ships.
        //
        // What the ramp actually asks for is an ordering - the floor is the
        // deepest surface, the lit pane the shallowest - and an ordering is
        // something you can read off the colours themselves without believing
        // a word the file says about them.
        let mut surfaces = [
            Colors::get(&colors.darker_background, background.mix(BLACK, 0.50)),
            Colors::get(&colors.dark_background, background.mix(BLACK, 0.30)),
            background,
            Colors::get(&colors.lighter_background, background.mix(foreground, 0.10)),
        ];
        surfaces.sort_by(|a, b| luminance(*a).total_cmp(&luminance(*b)));
        let [field, rack, tile, tile_lit] = surfaces;

        // ── The chrome above them ────────────────────────────────────────
        // Derived from the lit pane toward the ink rather than taken from the
        // theme's `selection` and `muted`, because those two are a highlight
        // and a comment colour - they carry no promise of sitting one rung
        // apart, and White sets both to `#c0c0c0`, which would collapse the
        // chip, the rules and the pane border into one flat grey.
        //
        // The three factors are what reproduce this app's own ramp when they
        // are fed this app's own colours: against @tile-lit and @text they
        // land within a couple of points of the @chip, @hairline and @edge
        // that `style.css` states by hand.
        let chip = tile_lit.mix(foreground, 0.10);
        let hairline = tile_lit.mix(foreground, 0.20);
        let edge = tile_lit.mix(foreground, 0.35);

        // ── The ink ──────────────────────────────────────────────────────
        // Faded toward the paper in fixed steps for the same reason, and with
        // the same result: the theme's own `light_foreground` and
        // `dark_foreground` are not a ladder. Nebula's `light_foreground` is
        // *warmer and brighter* than its `foreground`, so a `@dim` taken from
        // it would be louder than the `@text` it is meant to sit beneath.
        let dim = foreground.mix(tile, 0.25);
        let muted = foreground.mix(tile, 0.45);
        let faint = foreground.mix(tile, 0.62);

        let accent = Colors::get(&colors.accent, foreground);
        let red = Colors::get(&colors.red, foreground);
        let green = Colors::get(&colors.green, foreground);
        let yellow = Colors::get(&colors.yellow, foreground);
        let blue = Colors::get(&colors.blue, foreground);
        let magenta = Colors::get(&colors.magenta, foreground);
        let cyan = Colors::get(&colors.cyan, foreground);

        let ramp = vec![
            ("field", field),
            ("rack", rack),
            ("tile", tile),
            ("tile-lit", tile_lit),
            ("chip", chip),
            ("hairline", hairline),
            ("edge", edge),
            ("shadow", field.mix(BLACK, 0.50)),
            ("text", foreground),
            ("dim", dim),
            ("muted", muted),
            ("faint", faint),
            // The focus lamp. `style.css` argues at length that this wants to
            // be warm - a neutral white lights nothing - and an accent chosen
            // for a desktop is under no obligation to be. That is the trade
            // this whole module makes: the theme's accent is what every other
            // window on the desktop calls "focused", and matching it is worth
            // more than the warmth.
            ("filament", accent),
            ("tally", yellow),
            ("fresh", green),
            ("hangup", red),
        ];

        // ANSI 7 and 15 are the theme's two foregrounds rather than literal
        // whites, and 8 is its comment grey: a program printing "white" almost
        // never means #ffffff, it means "the bright one", and on a light theme
        // the bright one is black.
        let bright = |field: &Option<String>, plain: Rgb| Colors::get(field, plain);
        let ansi = [
            background, // replaced with the pane's own surface by `ansi()`
            red,
            green,
            yellow,
            blue,
            magenta,
            cyan,
            Colors::get(&colors.light_foreground, foreground),
            Colors::get(&colors.dark_foreground, foreground.mix(background, 0.45)),
            bright(&colors.bright_red, red),
            bright(&colors.bright_green, green),
            bright(&colors.bright_yellow, yellow),
            bright(&colors.bright_blue, blue),
            bright(&colors.bright_magenta, magenta),
            bright(&colors.bright_cyan, cyan),
            Colors::get(&colors.bright_foreground, foreground),
        ];

        Some(Theme {
            name: name.to_string(),
            mode,
            ramp,
            ansi,
            selection: Colors::get(&colors.selection, background.mix(accent, 0.24)),
        })
    }

    /// This app's colour `name` in the theme's palette, or `None` for a name
    /// the mapping doesn't cover - which `every_literal_colour_in_the_
    /// stylesheet_is_mapped` exists to make impossible.
    pub fn color(&self, name: &str) -> Option<Rgb> {
        self.ramp.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
    }

    /// The sixteen terminal colours, ANSI 0 painted in `surface`.
    ///
    /// Slot 0 tracks the pane rather than the theme's background for the reason
    /// `pane::ansi_palette` gives: a program painting a "black" background means
    /// "the surface", and a pane that answered with a foreign near-black would
    /// have a rectangle of somebody else's grey in the middle of it. Under a
    /// theme the two are nearly the same colour anyway - the difference is the
    /// focused pane, which is a rung up.
    pub fn ansi(&self, surface: Rgb) -> [Rgb; 16] {
        let mut ansi = self.ansi;
        ansi[0] = surface;
        ansi
    }

    /// The theme's own selection highlight.
    pub fn selection(&self) -> Rgb {
        self.selection
    }

    /// The `@define-color` block that repoints the ramp at this theme.
    ///
    /// The derived tints are appended verbatim from the stylesheet rather than
    /// restated - see `palette::derived_declarations` for why a provider that
    /// redefines `@text` and stops there gets the washes wrong, silently.
    pub fn css(&self) -> String {
        let mut css = format!("/* Omarchy: {} */\n", self.name);
        for (name, color) in &self.ramp {
            css.push_str(&format!("@define-color {name} {};\n", color.to_hex()));
        }
        for line in crate::palette::derived_declarations() {
            css.push_str(line);
            css.push('\n');
        }
        css
    }

    /// The claude theme that renders from the terminal's ANSI palette rather
    /// than from its own hexes - which is what makes an Omarchy theme reach
    /// inside a pane at all.
    pub fn claude_theme(&self) -> &'static str {
        match self.mode {
            Mode::Dark => "dark-ansi",
            Mode::Light => "light-ansi",
        }
    }
}

/// `~/.local/state/omarchy/current` under `home`.
///
/// Split from `current_dir` only so the rule can be stated in a test without
/// an environment variable to set - see
/// `the_theme_is_looked_for_where_omarchy_actually_puts_it`.
fn current_dir_under(home: &std::path::Path) -> PathBuf {
    home.join(".local/state/omarchy/current")
}

/// `~/.local/state/omarchy/current`, where Omarchy links the live theme from.
///
/// Built from `$HOME` and *not* from `$XDG_STATE_HOME`, which is the opposite
/// of what the variable's name suggests and is nonetheless correct: Omarchy
/// writes this path as `"$HOME/.local/state/omarchy/current"` in
/// `omarchy-theme-set`, hardcoded, and never consults `XDG_STATE_HOME` at all.
/// So on a machine where the two disagree, the state directory that has a theme
/// in it is the first one.
///
/// Which is not a hypothetical: this app's own `scripts/dev-run.sh` relocates
/// every `XDG_*` variable to sandbox a dev build away from the live one, and
/// the first version of this module read `XDG_STATE_HOME` first - so a dev
/// build launched to test theming was the one build on the machine that could
/// never find a theme, and came up in plain gunmetal looking like the feature
/// simply hadn't worked.
pub fn current_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(current_dir_under(std::path::Path::new(&home)))
}

/// The file whose contents change on every `omarchy theme set` - what the live
/// reload watches.
///
/// Deliberately `theme.name` and not the `theme` symlink beside it. A file
/// monitor on a symlink reports the link's own metadata, and `omarchy-theme-set`
/// replaces where it points rather than touching the link's target - so watching
/// it gives an event for some theme changes and not others. `theme.name` is a
/// regular file rewritten every single time, which is the one thing a monitor
/// can be relied on to notice.
pub fn name_path() -> Option<PathBuf> {
    Some(current_dir()?.join("theme.name"))
}

/// The live theme, or `None` on a machine with no Omarchy - which is not an
/// error and is not reported anywhere. The app has its own palette; this one is
/// an improvement on it when the desktop offers one.
pub fn load() -> Option<Theme> {
    let dir = current_dir()?;
    let name = std::fs::read_to_string(dir.join("theme.name"))
        .map(|n| n.trim().to_string())
        .unwrap_or_else(|_| "omarchy".to_string());
    let colors = std::fs::read_to_string(dir.join("theme/colors.toml")).ok()?;
    Theme::parse(&name, &colors)
}

thread_local! {
    /// The theme in force, or `None` on a machine with no Omarchy.
    ///
    /// Process-wide for the same reason `appearance` is: it is read from the
    /// stylesheet loader, from `palette` on behalf of every widget, and from
    /// every pane's terminal, none of which have any business being handed a
    /// palette through a signature that never varies. A thread-local because
    /// GTK is single-threaded and every one of those readers is a GTK callback.
    static ACTIVE: RefCell<Option<Theme>> = const { RefCell::new(None) };
}

/// Reads the live theme and installs it. Returns what it found, so a caller can
/// tell "the desktop is themed" from "the desktop is not" without asking twice.
///
/// Called at startup and again on every `omarchy theme set`; a failure to read
/// is a return to the built-in ramp rather than an error, because that ramp is
/// a complete palette and always was.
pub fn reload() -> bool {
    let theme = load();
    let themed = theme.is_some();
    ACTIVE.with(|active| *active.borrow_mut() = theme);
    themed
}

/// Whether the desktop's palette is light, which is the one fact about a theme
/// that reaches past colour into how libadwaita paints its own widgets.
pub fn mode() -> Option<Mode> {
    ACTIVE.with(|active| active.borrow().as_ref().map(|t| t.mode))
}

/// This app's colour `name` in the live theme, or `None` when there isn't one -
/// in which case `palette::color` answers from the stylesheet exactly as it
/// always has.
pub fn color(name: &str) -> Option<Rgb> {
    ACTIVE.with(|active| active.borrow().as_ref().and_then(|t| t.color(name)))
}

/// The live theme's sixteen terminal colours for a pane painted in `surface`.
pub fn ansi(surface: Rgb) -> Option<[Rgb; 16]> {
    ACTIVE.with(|active| active.borrow().as_ref().map(|t| t.ansi(surface)))
}

/// The live theme's selection highlight.
pub fn selection() -> Option<Rgb> {
    ACTIVE.with(|active| active.borrow().as_ref().map(|t| t.selection()))
}

/// The `@define-color` block for the live theme, for the stylesheet provider
/// that repoints the ramp at it.
pub fn css() -> Option<String> {
    ACTIVE.with(|active| active.borrow().as_ref().map(|t| t.css()))
}

/// The claude theme every pane should be launched with, or `None` to leave
/// claude on whatever the user's own settings say.
pub fn claude_theme() -> Option<&'static str> {
    ACTIVE.with(|active| active.borrow().as_ref().map(|t| t.claude_theme()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nebula, as shipped - a dark theme whose four backgrounds already climb
    /// in the order this app's ramp wants them.
    const DARK: &str = r##"
mode = "dark"
accent = "#4fc3c7"
selection = "#1b3d4d"
muted = "#3c5866"
background = "#0a1621"
dark_background = "#07111a"
darker_background = "#050c13"
lighter_background = "#122433"
foreground = "#cbd8da"
dark_foreground = "#5a7480"
light_foreground = "#e6d6c3"
bright_foreground = "#eaf4f4"
red = "#c0563d"
yellow = "#d3a061"
green = "#7fb894"
cyan = "#3f9ea6"
blue = "#3d84a6"
magenta = "#a97383"
bright_red = "#f0895a"
bright_yellow = "#f2c97e"
bright_green = "#a6d6ad"
bright_cyan = "#7fdcd8"
bright_blue = "#5cb0d1"
bright_magenta = "#d99a9c"
"##;

    /// White, as shipped. The interesting one: its backgrounds climb the *other
    /// way*, because in a light theme "darker_background" means a deeper grey
    /// rather than a deeper black.
    const LIGHT: &str = r##"
mode = "light"
accent = "#6e6e6e"
selection = "#c0c0c0"
muted = "#808080"
background = "#ffffff"
dark_background = "#f5f5f5"
darker_background = "#e8e8e8"
lighter_background = "#c0c0c0"
foreground = "#000000"
dark_foreground = "#c0c0c0"
light_foreground = "#000000"
bright_foreground = "#000000"
red = "#2a2a2a"
yellow = "#4a4a4a"
green = "#3a3a3a"
cyan = "#3e3e3e"
blue = "#1a1a1a"
magenta = "#2e2e2e"
"##;

    fn dark() -> Theme {
        Theme::parse("nebula", DARK).expect("nebula parses")
    }

    fn light() -> Theme {
        Theme::parse("white", LIGHT).expect("white parses")
    }

    /// Perceived brightness, for the ladder assertions below. Rec. 601, which
    /// is coarse and entirely good enough to say which of two greys is darker.
    fn luminance(c: Rgb) -> f32 {
        0.299 * c.r as f32 + 0.587 * c.g as f32 + 0.114 * c.b as f32
    }

    fn surface(theme: &Theme, name: &str) -> Rgb {
        theme
            .color(name)
            .unwrap_or_else(|| panic!("no mapping for {name:?}"))
    }

    /// The four backgrounds land on the four surfaces in brightness order, so
    /// the floor is the deepest of them and the lit pane the shallowest. Stated
    /// as an ordering rather than as four fixed assignments because that ordering is
    /// the only thing the ramp actually means.
    #[test]
    fn the_four_backgrounds_land_on_the_ramp_in_brightness_order() {
        let theme = dark();
        let field = luminance(surface(&theme, "field"));
        let rack = luminance(surface(&theme, "rack"));
        let tile = luminance(surface(&theme, "tile"));
        let lit = luminance(surface(&theme, "tile-lit"));
        assert!(
            field <= rack && rack <= tile && tile <= lit,
            "ramp is not monotonic: field {field}, rack {rack}, tile {tile}, lit {lit}"
        );
        assert_eq!(surface(&theme, "field"), Rgb::from_hex("#050c13").unwrap());
        assert_eq!(surface(&theme, "tile-lit"), Rgb::from_hex("#122433").unwrap());
    }

    /// The same rule, on a theme where it means the opposite thing. White's
    /// `lighter_background` is its *greyest* colour, so a mapping that trusted
    /// the key names would put the darkest grey on the focused pane and pure
    /// white on the floor - a light theme rendered inside out.
    #[test]
    fn a_light_theme_puts_its_greyest_background_on_the_floor() {
        let theme = light();
        assert_eq!(theme.mode, Mode::Light);
        assert_eq!(surface(&theme, "field"), Rgb::from_hex("#c0c0c0").unwrap());
        assert_eq!(surface(&theme, "tile-lit"), Rgb::from_hex("#ffffff").unwrap());
    }

    /// The chrome above the surfaces - the chip, the rules, the pane border -
    /// pulls away from the lit pane toward the ink, which is a climb in a dark
    /// theme and a descent in a light one. Contrast is the invariant; luminance
    /// is not.
    #[test]
    fn the_chrome_pulls_away_from_the_lit_surface_in_both_modes() {
        for theme in [dark(), light()] {
            let lit = luminance(surface(&theme, "tile-lit"));
            let gap = |name: &str| (luminance(surface(&theme, name)) - lit).abs();
            assert!(
                gap("chip") < gap("hairline") && gap("hairline") < gap("edge"),
                "{}: chip {}, hairline {}, edge {}",
                theme.name,
                gap("chip"),
                gap("hairline"),
                gap("edge"),
            );
        }
    }

    /// The ink ladder, same shape: every step is further from the paper than
    /// the last. A `faint` that had drifted past `muted` would be a footnote
    /// louder than the label above it.
    #[test]
    fn the_ink_ladder_fades_toward_the_paper_in_both_modes() {
        for theme in [dark(), light()] {
            let paper = luminance(surface(&theme, "tile"));
            let gap = |name: &str| (luminance(surface(&theme, name)) - paper).abs();
            assert!(
                gap("text") > gap("dim") && gap("dim") > gap("muted") && gap("muted") > gap("faint"),
                "{}: text {}, dim {}, muted {}, faint {}",
                theme.name,
                gap("text"),
                gap("dim"),
                gap("muted"),
                gap("faint"),
            );
        }
    }

    /// The point of the whole exercise. `pane::ansi_palette` substitutes this
    /// app's three signal colours into ANSI 1, 2 and 3; under a theme those
    /// slots have to be the theme's own, or claude renders in a palette the
    /// desktop never chose.
    #[test]
    fn the_ansi_palette_carries_the_themes_own_colours() {
        let theme = dark();
        let pane = Rgb::from_hex("#0a1621").unwrap();
        let ansi = theme.ansi(pane);
        assert_eq!(ansi[0], pane, "ANSI 0 is the pane's own surface");
        assert_eq!(ansi[1], Rgb::from_hex("#c0563d").unwrap());
        assert_eq!(ansi[2], Rgb::from_hex("#7fb894").unwrap());
        assert_eq!(ansi[3], Rgb::from_hex("#d3a061").unwrap());
        assert_eq!(ansi[4], Rgb::from_hex("#3d84a6").unwrap());
        assert_eq!(ansi[5], Rgb::from_hex("#a97383").unwrap());
        assert_eq!(ansi[6], Rgb::from_hex("#3f9ea6").unwrap());
        assert_eq!(ansi[9], Rgb::from_hex("#f0895a").unwrap());
        assert_eq!(ansi[15], Rgb::from_hex("#eaf4f4").unwrap());
    }

    /// `colors.toml` has no required schema beyond what each theme happens to
    /// need: Nebula ships no `orange` or `brown`, and a hand-written theme can
    /// leave out the whole bright row. A missing bright colour is its plain
    /// one, which is what every terminal does anyway.
    #[test]
    fn a_theme_with_no_bright_row_reuses_its_plain_colours() {
        let plain = DARK
            .lines()
            .filter(|l| !l.starts_with("bright_"))
            .collect::<Vec<_>>()
            .join("\n");
        let theme = Theme::parse("plain", &plain).expect("a theme without a bright row still parses");
        let ansi = theme.ansi(Rgb::from_hex("#0a1621").unwrap());
        for slot in 1..=6 {
            assert_eq!(
                ansi[slot + 8],
                ansi[slot],
                "bright slot {} should fall back to plain {slot}",
                slot + 8
            );
        }
    }

    /// Every colour `style.css` states as a literal has to have somewhere to
    /// come from, or a theme leaves half the window painted in gunmetal and the
    /// other half in the desktop's palette - which looks like a bug in the
    /// theme rather than a gap in this mapping.
    #[test]
    fn every_literal_colour_in_the_stylesheet_is_mapped() {
        let theme = dark();
        for (name, _) in crate::palette::literal_declarations() {
            assert!(
                theme.color(name).is_some(),
                "style.css defines @{name} but no theme colour maps onto it"
            );
        }
    }

    /// The derived tints (`@wash`, `@lit-wash`, and whatever joins them) are
    /// re-emitted from the stylesheet verbatim rather than restated here. A
    /// second copy of `alpha(@text, 0.05)` in this file is a second place for
    /// the interaction ramp to drift, and this module exists because of what
    /// drift costs.
    #[test]
    fn the_derived_tints_are_re_emitted_from_the_stylesheet() {
        let css = dark().css();
        assert!(css.contains("@define-color wash      alpha(@text, 0.05);"));
        assert!(css.contains("@define-color lit-hi    alpha(@filament, 0.20);"));
    }

    /// Mode decides what claude is told to be, and it is the only thing that
    /// does. `dark-ansi` on a light desktop is white-on-white.
    #[test]
    fn mode_chooses_the_ansi_theme_claude_is_launched_with() {
        assert_eq!(dark().claude_theme(), "dark-ansi");
        assert_eq!(light().claude_theme(), "light-ansi");
    }

    /// A file that isn't a theme is not a theme. Both failures matter: garbage
    /// TOML is a corrupt install, and valid TOML with no `background` is
    /// somebody's unrelated config file that happens to live at that path.
    #[test]
    fn a_file_that_is_not_a_theme_is_refused() {
        assert!(Theme::parse("junk", "this is not toml {{{").is_none());
        assert!(Theme::parse("empty", "mode = \"dark\"").is_none());
    }

    /// Omarchy states this path in terms of `$HOME` and never reads
    /// `XDG_STATE_HOME`, so this app must not either. A machine where the two
    /// disagree is a machine where reading the variable finds no theme and says
    /// nothing about it - and `scripts/dev-run.sh` makes exactly such a machine
    /// out of this one every time it runs.
    #[test]
    fn the_theme_is_looked_for_where_omarchy_actually_puts_it() {
        assert_eq!(
            current_dir_under(std::path::Path::new("/home/someone")),
            std::path::PathBuf::from("/home/someone/.local/state/omarchy/current"),
        );
    }

    /// The real ones, when they are here. Every stock theme on this machine has
    /// to survive the mapping and come out with a monotonic ramp - which is the
    /// only test in this module that would notice Omarchy changing the shape of
    /// `colors.toml` under us.
    #[test]
    fn every_stock_theme_on_this_machine_parses() {
        let stock = std::path::Path::new("/usr/share/omarchy/themes");
        let Ok(entries) = std::fs::read_dir(stock) else {
            return; // not an Omarchy machine; nothing to check
        };
        let mut checked = 0;
        for entry in entries.flatten() {
            let colors = entry.path().join("colors.toml");
            let Ok(source) = std::fs::read_to_string(&colors) else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let theme = Theme::parse(&name, &source)
                .unwrap_or_else(|| panic!("stock theme {name} failed to parse"));
            let field = luminance(surface(&theme, "field"));
            let lit = luminance(surface(&theme, "tile-lit"));
            assert!(field <= lit, "{name}: floor is brighter than the lit pane");
            checked += 1;
        }
        assert!(checked > 0, "found /usr/share/omarchy/themes but no themes in it");
    }
}
