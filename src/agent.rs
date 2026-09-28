//! Which agent a pane is running.
//!
//! A small closed set, named and known: adding one is a code change,
//! deliberately.
//! What varies between them is small and awkward - what the binary is called,
//! how you get your hooks in front of it, what it calls each moment of a turn -
//! and an enum keeps the answers to each question on adjacent lines, where
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
    Grok,
}

impl Kind {
    /// Every agent, in the order they are offered in the menu.
    pub const ALL: [Kind; 3] = [Kind::Claude, Kind::Codex, Kind::Grok];

    /// What it is called - in the config file, in the menu, and on the head
    /// strip. One word, lowercase, because it is all three of those things and
    /// the config file is the one that cannot afford ambiguity.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
            Kind::Grok => "grok",
        }
    }

    /// The command a pane runs when the config doesn't override it.
    pub fn default_command(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
            Kind::Grok => "grok",
        }
    }

    /// The inverse of `label`, forgiving about case and surrounding space:
    /// this reads a hand-written config file, where `default_agent = "Codex"`
    /// is somebody being reasonable rather than somebody making a mistake.
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        Kind::ALL.into_iter().find(|k| k.label() == name)
    }

    /// Whether this agent lets a new conversation be given its id up front.
    ///
    /// Claude and grok do (`--session-id`), and it is worth doing whenever it
    /// is possible: a pane that knows its conversation's id from the moment it
    /// starts can be resumed even if no hook ever reports - a hook that could
    /// not find the window, an agent killed in its first second. Codex doesn't,
    /// and learns its id from its first hook instead (see `wire::Payload`).
    pub fn names_its_session(self) -> bool {
        match self {
            Kind::Claude | Kind::Grok => true,
            Kind::Codex => false,
        }
    }

    /// The shell command line that starts this agent as `launch` says, from the
    /// command the config names for it.
    ///
    /// `session` is the id to give a *new* conversation, for the agents that
    /// take one (see `names_its_session`); it is ignored when resuming, since a
    /// resumed conversation already has one.
    ///
    /// Flags are appended rather than placed, because `configured` is whatever
    /// somebody wrote in a config file - `claude --model opus`, a wrapper
    /// script - and the end is the one position every such line leaves free.
    /// Codex is the exception, because its resume is a *subcommand* and has to
    /// follow the program's name directly: `codex --model o3 resume <id>` is a
    /// usage error, `codex resume <id> --model o3` is not (the resume
    /// subcommand takes the same options).
    pub fn command_line(self, configured: &str, launch: &Launch, session: Option<&str>) -> String {
        let quote = crate::update::sh_quote;
        match launch {
            Launch::Resume(id) => match self {
                Kind::Claude | Kind::Grok => format!("{configured} --resume {}", quote(id)),
                Kind::Codex => {
                    let configured = configured.trim_start();
                    let (program, rest) = configured
                        .split_once(char::is_whitespace)
                        .unwrap_or((configured, ""));
                    let named_codex = std::path::Path::new(program)
                        .file_name()
                        .is_some_and(|name| name == "codex");
                    if named_codex {
                        format!("{program} resume {} {rest}", quote(id))
                            .trim_end()
                            .to_string()
                    } else {
                        // A wrapper script: it is somebody else's job to pass
                        // its arguments along, and the end is where they are.
                        format!("{configured} resume {}", quote(id))
                    }
                }
            },
            Launch::Fresh | Launch::Worktree => {
                let mut line = configured.to_string();
                // Not onto a command line that already picks its conversation:
                // `claude --continue --session-id X` is refused outright (a new
                // id with a resume needs `--fork-session`), and a pane that
                // exits the instant it starts is a worse answer than a pane
                // whose id the hooks report a moment later.
                let picks_its_own = configured.split_whitespace().any(|word| {
                    matches!(word, "-c" | "--continue" | "-r" | "--resume" | "--session-id" | "-s")
                        || word.starts_with("--resume=")
                        || word.starts_with("--session-id=")
                });
                if let Some(session) = session.filter(|_| self.names_its_session() && !picks_its_own) {
                    line.push_str(&format!(" --session-id {}", quote(session)));
                }
                if matches!(launch, Launch::Worktree) {
                    // All three spell it the same way, and all three name the
                    // worktree themselves when not told a name - which is the
                    // right answer here, since the app has no better one.
                    line.push_str(" --worktree");
                }
                line
            }
        }
    }
}

/// How an agent pane should start.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Launch {
    /// A new conversation, in the project's own folder.
    #[default]
    Fresh,
    /// The conversation with this id, picked up where it was left - what a
    /// reopened session offers for every agent it had running.
    Resume(String),
    /// A new conversation in a git worktree of its own, so that two agents
    /// working on one project are not editing the same files underneath each
    /// other. Every supported agent creates and names the worktree itself.
    Worktree,
}

/// Whether `dir` is inside a git checkout - the one thing a worktree launch
/// needs, and the one thing every agent refuses it without.
///
/// A walk up the tree for `.git` rather than running git: this is asked on a
/// click, on the main thread, and a directory or a `.git` file (which is what a
/// worktree or a submodule has instead) is all the answer needs.
pub fn is_git_checkout(dir: &std::path::Path) -> bool {
    dir.ancestors().any(|dir| dir.join(".git").exists())
}

/// A fresh id for a new conversation, from the kernel's own generator.
///
/// `/proc/sys/kernel/random/uuid` rather than a crate: it is a version-4 UUID,
/// which is exactly the shape `--session-id` insists on, and it has been in
/// every Linux kernel this app could possibly run on. `None` off Linux, or with
/// `/proc` unmounted, and the agent then names its own conversation as it
/// always did.
pub fn new_session_id() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/uuid").ok()?;
    let id = id.trim();
    (id.len() == 36).then(|| id.to_string())
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
        assert_eq!(Kind::parse(" Grok "), Some(Kind::Grok));
    }

    const ID: &str = "0b5c2c1e-6f4e-4f52-9d8b-2f0c9c4c8e11";

    /// A fresh claude or grok is told the id of the conversation it is about
    /// to have, so the pane can resume it even if no hook ever gets through.
    #[test]
    fn a_fresh_agent_is_given_its_session_where_it_takes_one() {
        assert_eq!(
            Kind::Claude.command_line("claude", &Launch::Fresh, Some(ID)),
            format!("claude --session-id '{ID}'"),
        );
        assert_eq!(
            Kind::Grok.command_line("grok", &Launch::Fresh, Some(ID)),
            format!("grok --session-id '{ID}'"),
        );
        assert_eq!(
            Kind::Codex.command_line("codex", &Launch::Fresh, Some(ID)),
            "codex",
            "codex has no such flag, and learns its id from its first hook",
        );
        assert_eq!(Kind::Claude.command_line("claude", &Launch::Fresh, None), "claude");
    }

    /// A configured command that already chooses its conversation keeps its
    /// choice: claude refuses a fresh id beside `--continue`.
    #[test]
    fn a_command_that_picks_its_own_conversation_is_not_given_another() {
        for configured in ["claude --continue", "claude -c", "claude --resume abc", "grok -s x"] {
            let line = Kind::Claude.command_line(configured, &Launch::Fresh, Some(ID));
            assert_eq!(line, configured, "{configured}");
        }
    }

    #[test]
    fn a_resume_asks_each_agent_in_its_own_words() {
        assert_eq!(
            Kind::Claude.command_line("claude --model opus", &Launch::Resume(ID.into()), Some("x")),
            format!("claude --model opus --resume '{ID}'"),
            "resuming ignores the fresh id - the conversation has one",
        );
        assert_eq!(
            Kind::Grok.command_line("grok", &Launch::Resume(ID.into()), None),
            format!("grok --resume '{ID}'"),
        );
    }

    /// Codex resumes with a subcommand, which has to follow the program's name:
    /// `codex -m o3 resume <id>` is a usage error, and the configured options
    /// are carried after it instead.
    #[test]
    fn codex_resumes_with_a_subcommand_placed_after_its_name() {
        assert_eq!(
            Kind::Codex.command_line("codex", &Launch::Resume(ID.into()), None),
            format!("codex resume '{ID}'"),
        );
        assert_eq!(
            Kind::Codex.command_line("  /usr/bin/codex -m o3", &Launch::Resume(ID.into()), None),
            format!("/usr/bin/codex resume '{ID}' -m o3"),
        );
        assert_eq!(
            Kind::Codex.command_line("codex-wrapper --fast", &Launch::Resume(ID.into()), None),
            format!("codex-wrapper --fast resume '{ID}'"),
            "a wrapper passes its arguments on, so they go at the end",
        );
    }

    /// An id comes back from an agent's hook, so it is quoted like anything
    /// else that reaches a shell from outside.
    #[test]
    fn a_session_id_cannot_escape_its_quotes() {
        let line = Kind::Claude.command_line("claude", &Launch::Resume("x'; rm -rf ~; '".into()), None);
        assert_eq!(line, r"claude --resume 'x'\''; rm -rf ~; '\'''");
    }

    #[test]
    fn a_worktree_launch_asks_for_one() {
        for kind in Kind::ALL {
            let line = kind.command_line(kind.default_command(), &Launch::Worktree, Some(ID));
            assert!(line.ends_with(" --worktree"), "{line}");
        }
    }

    #[test]
    fn a_checkout_is_found_from_anywhere_inside_it() {
        let root = std::env::temp_dir().join(format!("atc-git-{}", std::process::id()));
        let deep = root.join("src").join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(!is_git_checkout(&deep) || is_git_checkout(&std::env::temp_dir()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert!(is_git_checkout(&deep));
        assert!(is_git_checkout(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_kernel_hands_out_session_ids_of_the_shape_agents_want() {
        let Some(id) = new_session_id() else {
            return; // not Linux, or no /proc
        };
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
        assert_ne!(new_session_id(), Some(id), "and a different one every time");
    }

    #[test]
    fn each_kind_defaults_to_the_binary_it_is_named_for() {
        assert_eq!(Kind::Claude.default_command(), "claude");
        assert_eq!(Kind::Codex.default_command(), "codex");
        assert_eq!(Kind::Grok.default_command(), "grok");
    }
}
