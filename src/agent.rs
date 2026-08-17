//! Which agent a pane is running.
//!
//! Two of them, named and known: adding a third is a code change, deliberately.
//! What varies between them is small and awkward - what the binary is called,
//! how you get your hooks in front of it, what it calls each moment of a turn -
//! and an enum keeps the two answers to each question on adjacent lines, where
//! a difference is visible. A trait would put them in separate files and buy
//! an extensibility nobody has asked for.
//!
//! GTK-free on purpose, like `hooks`: the mapping is the part worth testing,
//! and it is testable without a window.

/// An agent this app knows how to launch and how to listen to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Kind {
    #[default]
    Claude,
    Codex,
}

impl Kind {
    /// Every agent, in the order they are offered in the menu.
    pub const ALL: [Kind; 2] = [Kind::Claude, Kind::Codex];

    /// What it is called - in the config file, in the menu, and on the head
    /// strip. One word, lowercase, because it is all three of those things and
    /// the config file is the one that cannot afford ambiguity.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
        }
    }

    /// The command a pane runs when the config doesn't override it.
    pub fn default_command(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
        }
    }

    /// The inverse of `label`, forgiving about case and surrounding space:
    /// this reads a hand-written config file, where `default_agent = "Codex"`
    /// is somebody being reasonable rather than somebody making a mistake.
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        Kind::ALL.into_iter().find(|k| k.label() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_through_its_label() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.label()), Some(kind));
        }
        assert_eq!(Kind::parse("gemini"), None);
    }

    /// The label is what a config file says and what a head strip shows, so a
    /// capitalised or padded one is a person being reasonable, not a typo.
    #[test]
    fn a_label_is_read_the_way_a_person_would_write_it() {
        assert_eq!(Kind::parse(" Claude "), Some(Kind::Claude));
        assert_eq!(Kind::parse("CODEX"), Some(Kind::Codex));
    }

    #[test]
    fn each_kind_defaults_to_the_binary_it_is_named_for() {
        assert_eq!(Kind::Claude.default_command(), "claude");
        assert_eq!(Kind::Codex.default_command(), "codex");
    }
}
