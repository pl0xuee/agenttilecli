//! What the window remembers between runs.
//!
//! Everything here is machine-managed: which projects were open and in what
//! order, how each one's panes were arranged, how big the window was. It is
//! written by the app and read by the app, and nobody is expected to edit it -
//! which is what separates it from `config`, where the things a person chooses
//! live.
//!
//! Split that way because the two have different failure modes. A config file
//! someone hand-edited into invalid TOML should say so; a state file that has
//! gone bad should be quietly ignored and rewritten, because it is not the
//! user's mistake and there is nothing they could do about it. Both cases end
//! up back at defaults here - the difference is only whether anyone is told.
//!
//! What is deliberately *not* restored is the agents. A project reopens with
//! its layout and no panes running, because starting an agent nobody asked for
//! is the one thing this app must not do on its own: an agent is a process with
//! a token budget attached, and "I quit with four of those running" is not the
//! same statement as "start four of those now". `restore_agents` in `config` is
//! there for anyone who disagrees.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::layout::Mode;

/// The whole of what a run hands to the next one.
#[derive(Clone, Default, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub window: Window,
    /// The text scale the keybindings last left the whole window at.
    pub font_scale: f64,
    pub projects: Vec<Project>,
    /// What the preferences dialog was last left showing.
    pub appearance: Appearance,
}

/// The appearance settings, as the preferences dialog last left them.
///
/// Here rather than only in `config.toml`, and the reason is what that file is
/// for: it exists to be opened and commented (see `config`'s header), and
/// serialising a struct back over it would delete every comment the user had
/// written. So the file states the defaults and this remembers the adjustments,
/// which is exactly how `font_scale` above already behaves.
///
/// `Option` on every field, so "never touched the dialog" is distinguishable
/// from "chose the value that happens to be the default". Without it, editing
/// `window_opacity` in the config file would do nothing on any machine whose
/// session had ever been written, which is the failure mode a config file can
/// least afford.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub window_opacity: Option<f64>,
    pub pane_opacity: Option<f64>,
    pub gap: Option<i32>,
}

/// The window's own shape, which is the part people notice missing first.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Window {
    pub width: i32,
    pub height: i32,
    /// The rack's share of the window, as the split view measures it.
    pub sidebar_fraction: f64,
    pub sidebar_shown: bool,
}

/// One project, as it was when the window closed.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    pub path: String,
    pub name: String,
    pub icon: String,
    pub mode: Mode,
    pub master_ratio: f64,
    pub master_count: usize,
    /// How many agents it had running. Not restored unless
    /// `config::Config::restore_agents` says so - see this module's header.
    pub agents: usize,
    /// Whether this was the project on screen.
    pub active: bool,
}

impl Default for Window {
    fn default() -> Self {
        Window {
            // The same 16:10 the window has always opened at when it has no
            // memory to go on.
            width: 1488,
            height: 930,
            sidebar_fraction: 0.17,
            sidebar_shown: false,
        }
    }
}

impl Default for Project {
    fn default() -> Self {
        Project {
            path: String::new(),
            name: String::new(),
            icon: "folder-symbolic".to_string(),
            mode: Mode::default(),
            master_ratio: 0.55,
            master_count: 1,
            agents: 0,
            active: false,
        }
    }
}

/// What the font-scale keybindings will let a person reach: `FONT_SCALE_MIN` and
/// `FONT_SCALE_MAX` in `app` (0.5 and 3.0), the two values
/// `App::inc_font_scale`/`dec_font_scale` clamp every keystroke to.
///
/// A saved scale outside them is a scale no keystroke could have produced, and
/// nothing downstream re-checks it: `font_scale: 50` goes straight into the
/// dynamic stylesheet as `.scaled-content { font-size: 50em }`, which is one
/// glyph per pane and a header bar too large to find the control that would put
/// it back.
use crate::app::{FONT_SCALE_MAX, FONT_SCALE_MIN};

/// The largest window dimension worth asking for.
///
/// The floor is the interesting end and it isn't here: `GtkWindow:default-width`
/// and `:default-height` are `gint` properties whose param spec runs
/// `-1 ..= G_MAXINT` (checked against the installed GTK, not remembered), so a
/// saved `-2` is not a small window - it is a `g_object_set` critical and a
/// property left at whatever it was. Anything that isn't a positive number of
/// pixels is handled by falling back to `Window::default()` instead, because -1
/// and 0 both mean "let GTK choose" and this app never writes either.
///
/// The ceiling is here because the param spec's own is `G_MAXINT`, which no
/// display can honour: a window's width in the X11 protocol is a 16-bit field,
/// so 65535 is the largest size that can even be expressed to a server (Wayland's
/// field is wider; no compositor will hand over more either). Past that a saved
/// size is not a size, and clamping beats asking for two billion pixels.
const WINDOW_MAX_PX: i32 = 65535;

/// The rack's share of the window: as narrow and as wide as the grip itself will
/// go, which is the `0.1 ..= 0.5` in `app::sidebar::sidebar_fraction`.
///
/// libadwaita's `OverlaySplitView:sidebar-width-fraction` accepts `0.0 ..= 1.0`
/// (again checked against the installed library), so a saved `5.0` is out of
/// range for the property as well - but the grip's range is the one that matters,
/// because a restored fraction the grip cannot reach is a rack the user cannot
/// drag back to where they left it. That is exactly the "two clamps disagreeing"
/// that `App::new`'s comment on this property is about.
use crate::app::sidebar::{SIDEBAR_FRACTION_MAX, SIDEBAR_FRACTION_MIN};

impl Session {
    /// Reads the saved session, or a default one.
    ///
    /// Every failure lands on the default: no file, an unreadable file, JSON
    /// that no longer parses because an older version wrote it. None of those
    /// are the user's doing and none of them are worth a dialog - the cost is
    /// one window that opens at its default size, which is exactly what the app
    /// did before it remembered anything.
    pub fn load() -> Session {
        let Some(path) = state_path() else {
            return Session::default();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Session::default();
        };
        Session::parse(&text)
    }

    /// The text of a session file, as the rest of the app is allowed to see it.
    ///
    /// Split out of `load` so the clamping is testable without a file on disk -
    /// `load` is this plus finding the path.
    fn parse(text: &str) -> Session {
        serde_json::from_str::<Session>(text)
            .unwrap_or_default()
            .clamped()
    }

    /// Drags every remembered number back into the range its consumer can use.
    ///
    /// Here, at the one place a session enters the process, rather than at each
    /// place it is read - because those places are a window builder, a split
    /// view and a stylesheet in three different modules, and a guard in each is
    /// three opportunities to add a fourth reader without one. `appearance` has
    /// clamped the three fields it owns since it was written (see
    /// `Appearance::resolved`); these are the ones nothing was clamping.
    ///
    /// Not paranoia about the user: a state file is machine-written and nobody
    /// is expected to edit it (see this module's header). It is about the file
    /// that *isn't* what this version writes - one from a future build, one
    /// truncated by a full disk, one somebody opened out of curiosity - and
    /// about what those cost. Bad JSON is already survivable, because it lands
    /// on the defaults; a file that parses and holds `font_scale: 50` is worse,
    /// because it opens a window the user cannot read well enough to fix it, run
    /// after run, with nothing on screen saying why.
    fn clamped(mut self) -> Session {
        // 0.0 is not a scale, it is the absence of one: `#[serde(default)]`
        // leaves it there for any file written before this field existed, and
        // `App::new` reads exactly that ("> 0.0") as "no saved scale, open at
        // 1.0". So the sentinel is passed through untouched - clamping it up to
        // the floor would open every upgraded session at half size - and
        // anything else that isn't a finite positive number is turned *into* the
        // sentinel, because a negative or NaN scale is not something a keystroke
        // saved and "open at 1.0" is a kinder answer to it than 0.5.
        self.font_scale = if self.font_scale.is_finite() && self.font_scale > 0.0 {
            self.font_scale.clamp(FONT_SCALE_MIN, FONT_SCALE_MAX)
        } else {
            0.0
        };

        let default = Window::default();
        self.window.width = window_px(self.window.width, default.width);
        self.window.height = window_px(self.window.height, default.height);
        // A fraction of nothing is not a fraction. `App::new`'s own fallback for
        // a non-positive one is `SIDEBAR_DEFAULT_FRACTION`, which is this same
        // 0.17 - so agreeing with it here costs nothing and means the two places
        // cannot drift apart.
        self.window.sidebar_fraction =
            if self.window.sidebar_fraction.is_finite() && self.window.sidebar_fraction > 0.0 {
                self.window
                    .sidebar_fraction
                    .clamp(SIDEBAR_FRACTION_MIN, SIDEBAR_FRACTION_MAX)
            } else {
                default.sidebar_fraction
            };

        // And the two numbers each project hands the tiler as geometry.
        //
        // These are not idle. `Tiler::restore_layout` clamps the ratio and floors
        // the count, so a bad file cannot make the *layout* misbehave - but it
        // clamps with `f64::clamp`, which returns NaN unchanged, and a NaN ratio
        // multiplied by a width is a master column allocated zero pixels. And it
        // applies no ceiling to the count, so `usize::MAX` survives the restore,
        // is ignored by every geometry calculation that re-clamps it against the
        // live pane count, and is then written back out on quit: a value nothing
        // honours and nothing corrects, sitting in the file for ever.
        //
        // The ceiling is the project's own agent count, because that is what the
        // runtime bound actually is - `layout::compute` clamps to the number of
        // panes there are, and `agents` is exactly the number `snapshot_session`
        // recorded. `max(1)` on it so a project saved with no agents still leaves
        // a usable 1 rather than an empty range for `clamp` to panic on.
        for project in &mut self.projects {
            project.master_ratio = if project.master_ratio.is_finite() {
                project
                    .master_ratio
                    .clamp(crate::layout::MASTER_RATIO_MIN, crate::layout::MASTER_RATIO_MAX)
            } else {
                Project::default().master_ratio
            };
            project.master_count = project.master_count.clamp(1, project.agents.max(1));
        }

        self
    }

    /// Writes the session, atomically.
    ///
    /// Via a temporary file and a rename, because the alternative is a window
    /// that was killed mid-write leaving a half-written file - and the next
    /// launch reading it, failing to parse it, and silently forgetting every
    /// project the user had open. A rename is the one filesystem operation that
    /// cannot leave a reader looking at half of anything.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = state_path() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let temporary = path.with_extension("json.new");
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, &path)
    }
}

/// One window dimension, as GTK will take it: the saved pixels if they are
/// pixels at all, `default` if they aren't, and never more than any display can
/// be asked for. See `WINDOW_MAX_PX` for both ends.
fn window_px(saved: i32, default: i32) -> i32 {
    if saved < 1 {
        default
    } else {
        saved.min(WINDOW_MAX_PX)
    }
}

/// `$XDG_STATE_HOME/agenttilecli/session.json`.
///
/// State rather than config or cache: it is neither something a person edits
/// nor something that can be regenerated from scratch, which is the exact gap
/// the state directory exists to fill.
pub fn state_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(dir.join("agenttilecli").join("session.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_session() -> Session {
        Session {
            window: Window {
                width: 1600,
                height: 900,
                sidebar_fraction: 0.22,
                sidebar_shown: true,
            },
            font_scale: 1.1,
            appearance: Appearance {
                window_opacity: Some(0.85),
                pane_opacity: None,
                gap: Some(10),
            },
            projects: vec![
                Project {
                    path: "/home/a/work".into(),
                    name: "work".into(),
                    icon: "folder-symbolic".into(),
                    mode: Mode::MasterStack,
                    master_ratio: 0.6,
                    master_count: 2,
                    agents: 3,
                    active: true,
                },
                Project {
                    path: "/home/a/other".into(),
                    name: "other".into(),
                    ..Project::default()
                },
            ],
        }
    }

    #[test]
    fn a_session_round_trips_through_json() {
        let session = a_session();
        let text = serde_json::to_string(&session).expect("serialises");
        let back: Session = serde_json::from_str(&text).expect("parses");
        assert_eq!(back, session);
    }

    /// Order is the whole point of saving the list. A user who dragged their
    /// projects into an order they like should find it again.
    #[test]
    fn project_order_survives() {
        let session = a_session();
        let text = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&text).unwrap();
        let names: Vec<_> = back.projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["work", "other"]);
        let active: Vec<_> = back
            .projects
            .iter()
            .filter(|p| p.active)
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(active, ["work"], "exactly one project comes back active");
    }

    /// A state file from an older version - or a corrupted one - must not stop
    /// the window opening. It is not the user's mistake and there is nothing
    /// they could do about it.
    ///
    /// Through `parse` rather than `serde_json` directly, because `parse` is what
    /// `load` calls: it also says the defaults survive their own clamping, which
    /// they have to, or every window with no session would open somewhere else.
    #[test]
    fn rubbish_reads_back_as_defaults() {
        for text in ["", "{", "null", "[]", r#"{"projects":"not a list"}"#] {
            assert_eq!(
                Session::parse(text),
                Session::default(),
                "accepted {text:?}"
            );
        }
    }

    /// Anything the app itself wrote must come back untouched. A clamp that moved
    /// a legitimate value would be a window that quietly forgot its own size.
    #[test]
    fn a_session_the_app_wrote_is_not_touched_by_the_clamps() {
        let session = a_session();
        let text = serde_json::to_string(&session).expect("serialises");
        assert_eq!(Session::parse(&text), session);
    }

    /// A file that *parses* and holds nonsense is the case bad JSON never
    /// reaches. Bad JSON lands on the defaults; this lands on the widgets - on
    /// `default_width`, on a libadwaita property with a documented range, and on
    /// a stylesheet - and every one of them takes what it is given.
    #[test]
    fn a_session_full_of_absurd_values_loads_as_something_usable() {
        let absurd = r#"{
            "font_scale": 50,
            "window": {
                "width": -7,
                "height": -2147483648,
                "sidebar_fraction": 5.0,
                "sidebar_shown": true
            }
        }"#;
        let session = Session::parse(absurd);
        let default = Window::default();

        assert_eq!(
            session.font_scale, FONT_SCALE_MAX,
            "50em of text is a window nobody can read the menus of",
        );
        assert_eq!(
            session.window.width, default.width,
            "a negative width is not a small window, it is a GTK critical",
        );
        assert_eq!(session.window.height, default.height);
        assert_eq!(
            session.window.sidebar_fraction, SIDEBAR_FRACTION_MAX,
            "a fraction of 5 is a rack five windows wide",
        );
        assert!(
            session.window.sidebar_shown,
            "the fields that were fine are left exactly as they were",
        );
    }

    /// The other end of both ranges, and the sizes that are legal for the
    /// property but not for any display.
    #[test]
    fn the_far_ends_of_every_range_are_brought_back_in() {
        let squashed =
            Session::parse(r#"{"font_scale":0.01,"window":{"sidebar_fraction":0.0001}}"#);
        assert_eq!(squashed.font_scale, FONT_SCALE_MIN);
        assert_eq!(squashed.window.sidebar_fraction, SIDEBAR_FRACTION_MIN);

        let enormous = Session::parse(r#"{"window":{"width":2147483647,"height":100000}}"#);
        assert_eq!(enormous.window.width, WINDOW_MAX_PX);
        assert_eq!(enormous.window.height, WINDOW_MAX_PX);
    }

    /// The one value that must *not* be dragged into its range: 0.0 is the
    /// absence of a saved scale, not a scale of zero, and every session file
    /// written before the field existed says exactly that. Clamping it up to the
    /// floor would open all of them at half size.
    ///
    /// The values that can't be spelled in JSON are checked through `clamped`
    /// directly - `serde_json` won't write a NaN, but a hand-made file, a future
    /// field or an arithmetic accident upstream can still produce one, and a NaN
    /// is the value `clamp` famously returns unchanged.
    #[test]
    fn a_font_scale_nobody_saved_stays_unsaved() {
        assert_eq!(
            Session::parse(r#"{"projects":[]}"#).font_scale,
            0.0,
            "a file with no font_scale must not gain one",
        );

        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let session = Session {
                font_scale: scale,
                ..Session::default()
            }
            .clamped();
            assert_eq!(
                session.font_scale, 0.0,
                "{scale} is not a scale a keystroke saved; it should read as none at all",
            );
        }

        for fraction in [f64::NAN, f64::INFINITY, -2.0, 0.0] {
            let session = Session {
                window: Window {
                    sidebar_fraction: fraction,
                    ..Window::default()
                },
                ..Session::default()
            }
            .clamped();
            assert_eq!(
                session.window.sidebar_fraction,
                Window::default().sidebar_fraction,
                "{fraction} of a window is not a rack width",
            );
        }
    }

    /// A file written by a version that knew fewer fields still loads, and the
    /// fields it never heard of come back as defaults rather than as an error.
    /// This is what `#[serde(default)]` is buying, and it is the difference
    /// between an upgrade that keeps your projects and one that forgets them.
    #[test]
    fn a_session_from_an_older_version_still_loads() {
        let old = r#"{"projects":[{"path":"/home/a/work","name":"work","active":true}]}"#;
        let session: Session = serde_json::from_str(old).expect("older shape still parses");
        assert_eq!(session.projects.len(), 1);
        assert_eq!(session.projects[0].name, "work");
        assert_eq!(
            session.projects[0].mode,
            Mode::default(),
            "a field it never wrote comes back as the default",
        );
        assert_eq!(session.window, Window::default());
    }
    /// The two numbers a project hands the tiler, from a file that means harm.
    ///
    /// NaN is the one worth spelling out: `Tiler::restore_layout` already clamps
    /// the ratio, but `f64::clamp` returns NaN unchanged and `layout::master_stack`
    /// then multiplies a width by it and casts - which saturates to zero, i.e. a
    /// master column allocated no pixels at all. And `usize::MAX` for the count is
    /// the value that would otherwise round-trip through the file for ever, since
    /// every geometry path re-clamps it against the live pane count instead of
    /// correcting it.
    #[test]
    fn a_projects_geometry_is_clamped_out_of_the_file() {
        let mut session = a_session();
        session.projects[0].master_ratio = f64::NAN;
        session.projects[0].master_count = usize::MAX;
        session.projects[0].agents = 2;
        let fixed = session.clamped();

        assert_eq!(
            fixed.projects[0].master_ratio,
            Project::default().master_ratio,
            "a NaN ratio has to become a real one, not stay NaN through `clamp`",
        );
        assert_eq!(
            fixed.projects[0].master_count, 2,
            "the count is held to the agents the project actually has",
        );

        let mut extreme = a_session();
        extreme.projects[0].master_ratio = 40.0;
        extreme.projects[0].master_count = 0;
        extreme.projects[0].agents = 0;
        let fixed = extreme.clamped();
        assert_eq!(fixed.projects[0].master_ratio, crate::layout::MASTER_RATIO_MAX);
        assert_eq!(
            fixed.projects[0].master_count, 1,
            "a project saved with no agents still leaves a usable count",
        );
    }

}
