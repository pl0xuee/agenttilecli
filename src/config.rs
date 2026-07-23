//! The things a person chooses, in a file they can edit.
//!
//! The counterpart to `session`: that one is machine-managed and nobody is
//! meant to open it, this one exists to be opened. Everything here was a
//! compiled-in constant until now - the command each pane runs, how many agents
//! a new project starts with, how much air there is between tiles - and a
//! constant is a decision made once, for everybody, by whoever happened to
//! write the line.
//!
//! TOML rather than JSON because it takes comments, and a config file that
//! cannot explain itself is one people guess at.
//!
//! A malformed file is *reported*, unlike a malformed session. Someone typed
//! this; silently ignoring what they typed and carrying on with defaults is the
//! behaviour that has people convinced the file does nothing.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// The config this run is using.
///
/// Process-wide because it is a process-wide fact, read from a great many
/// places - the panes, the layout, the project picker - none of which have any
/// business being handed it through four signatures that never vary.
static ACTIVE: OnceLock<Config> = OnceLock::new();

/// The config, or the defaults if it was never installed (which is every unit
/// test in this crate, and is correct: a test should not read the developer's
/// own config file).
pub fn get() -> &'static Config {
    static FALLBACK: OnceLock<Config> = OnceLock::new();
    ACTIVE
        .get()
        .unwrap_or_else(|| FALLBACK.get_or_init(Config::default))
}

/// Anything wrong with the file, kept for the window to report once it exists.
static PROBLEM: OnceLock<Option<String>> = OnceLock::new();

/// Installs the config for the rest of the run. Called once, from `main`.
pub fn install(loaded: Loaded) {
    let _ = ACTIVE.set(loaded.config);
    let _ = PROBLEM.set(loaded.problem);
}

/// What was wrong with the config file, if anything.
///
/// Read after the window is up rather than reported from `main`, because the
/// report is a dialog and a dialog needs something to sit on.
pub fn problem() -> Option<&'static str> {
    PROBLEM.get().and_then(Option::as_deref)
}

/// Everything configurable, with the defaults the app used when none of it was.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// What each pane runs. The hook settings are layered on top of whatever
    /// this is, so a wrapper script still reports its status.
    pub command: String,
    /// How many agents a newly-opened project starts with, before the app has
    /// learned your habit from the project you were last working in.
    pub agents: usize,
    /// Whether reopening a saved session also restarts the agents that were
    /// running in it. Off, deliberately - see `session`'s header.
    pub restore_agents: bool,
    /// Half the space between neighbouring tiles, in pixels. Applied to every
    /// side of every tile, so two tiles that share a seam end up twice this far
    /// apart - see `layout::gap`.
    pub gap: i32,
    /// How many lines of scrollback each pane keeps. Agents produce a great
    /// deal of output and the default is not generous.
    pub scrollback: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            command: "claude".to_string(),
            agents: 1,
            restore_agents: false,
            gap: 4,
            scrollback: 10_000,
        }
    }
}

/// What reading the config produced, including anything worth telling the user.
pub struct Loaded {
    pub config: Config,
    /// Set when a file exists but could not be used. The app runs on defaults
    /// and says so, rather than leaving someone to wonder why their edit did
    /// nothing.
    pub problem: Option<String>,
}

impl Config {
    /// Reads the config, falling back to defaults and explaining why if it has
    /// to.
    pub fn load() -> Loaded {
        let Some(path) = config_path() else {
            return Loaded {
                config: Config::default(),
                problem: None,
            };
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // Not existing is the normal case, not a problem.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Loaded {
                    config: Config::default(),
                    problem: None,
                }
            }
            Err(e) => {
                return Loaded {
                    config: Config::default(),
                    problem: Some(format!("{} couldn't be read: {e}", path.display())),
                }
            }
        };
        Config::parse(&text, &path.display().to_string())
    }

    /// The parsing half, split out so it can be tested without a filesystem.
    pub fn parse(text: &str, whence: &str) -> Loaded {
        match toml::from_str::<Config>(text) {
            Ok(config) => Loaded {
                config,
                problem: None,
            },
            Err(e) => Loaded {
                config: Config::default(),
                // `to_string` on a toml error carries the line and column, which
                // is the entire value of reporting this at all.
                problem: Some(format!("{whence} has a problem, so defaults are in use:\n\n{e}")),
            },
        }
    }
}

/// `$XDG_CONFIG_HOME/agenttilecli/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(dir.join("agenttilecli").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_config_is_not_a_problem() {
        let loaded = Config::parse("", "test");
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.problem.is_none());
    }

    #[test]
    fn a_partial_config_only_overrides_what_it_names() {
        let loaded = Config::parse("agents = 3\n", "test");
        assert!(loaded.problem.is_none());
        assert_eq!(loaded.config.agents, 3);
        assert_eq!(
            loaded.config.command,
            Config::default().command,
            "a key you didn't write keeps its default",
        );
    }

    /// Someone typed this file. A mistake in it has to be said out loud, or the
    /// file acquires a reputation for doing nothing.
    #[test]
    fn a_malformed_config_is_reported_rather_than_swallowed() {
        let loaded = Config::parse("agents = = 3", "config.toml");
        assert_eq!(loaded.config, Config::default(), "and defaults are used");
        let problem = loaded.problem.expect("a malformed file is reported");
        assert!(problem.contains("config.toml"), "says which file: {problem}");
    }

    /// A key that doesn't exist is almost always a typo for one that does, and
    /// silently ignoring it is how someone spends an evening wondering why
    /// `agent = 3` did nothing.
    #[test]
    fn an_unknown_key_is_reported() {
        let loaded = Config::parse("agent = 3\n", "config.toml");
        let problem = loaded.problem.expect("an unknown key is reported");
        assert!(problem.contains("agent"), "names the key: {problem}");
    }

    #[test]
    fn a_config_round_trips() {
        let config = Config {
            command: "claude --model opus".into(),
            agents: 2,
            restore_agents: true,
            gap: 8,
            scrollback: 50_000,
        };
        let text = toml::to_string(&config).expect("serialises");
        let back = Config::parse(&text, "test");
        assert!(back.problem.is_none());
        assert_eq!(back.config, config);
    }
}
