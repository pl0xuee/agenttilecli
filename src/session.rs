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
        serde_json::from_str(&text).unwrap_or_default()
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
    #[test]
    fn rubbish_reads_back_as_defaults() {
        for text in ["", "{", "null", "[]", r#"{"projects":"not a list"}"#] {
            let parsed: Session = serde_json::from_str(text).unwrap_or_default();
            assert_eq!(parsed, Session::default(), "accepted {text:?}");
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
}
