//! What an agent tells us about itself, and what it means.
//!
//! The only signal a pane has ever emitted is a bell byte (`pane::BELL_HOOK`),
//! rung when claude finishes a turn or stops to ask. One byte, one meaning:
//! *something happened*. It is enough to flash a sidebar row and nothing more -
//! an agent that finishes in the group you are already looking at marks nothing
//! at all, and "three agents" on a strip cannot say whether that is three
//! working, three waiting on you, or three finished an hour ago.
//!
//! claude will say considerably more than that if asked. It runs a command of
//! our choosing at six points in its life, handing it a JSON object on stdin,
//! and this module is the vocabulary for those six points plus the rule for
//! what each one does to a pane's state.
//!
//! Deliberately free of GTK, sockets and processes: what an event *means* is
//! the part worth testing, and it is testable without any of them. The wiring
//! that carries an event from claude to here lives in `ipc`.

use crate::model::PaneState;

/// The moments in an agent's turn that this app asks claude to report.
///
/// Six rather than the two the bell covered, because the two only ever meant
/// "look at me": what is missing is everything between, which is the difference
/// between an agent working and an agent finished.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// The agent started up.
    SessionStart,
    /// You pressed return on a prompt.
    UserPromptSubmit,
    /// It is about to run a tool. The event carries which one.
    PreToolUse,
    /// The tool finished; it is thinking again.
    PostToolUse,
    /// It has stopped to ask you something - permission, a choice.
    Notification,
    /// It finished its turn and the floor is yours.
    Stop,
}

impl Event {
    /// Every event, in the order a turn produces them. Used to write the
    /// settings file, so adding one here is all it takes to register it.
    pub const ALL: [Event; 6] = [
        Event::SessionStart,
        Event::UserPromptSubmit,
        Event::PreToolUse,
        Event::PostToolUse,
        Event::Notification,
        Event::Stop,
    ];

    /// claude's own name for this event, which is both the settings key and the
    /// argument the hook is invoked with.
    pub fn name(self) -> &'static str {
        match self {
            Event::SessionStart => "SessionStart",
            Event::UserPromptSubmit => "UserPromptSubmit",
            Event::PreToolUse => "PreToolUse",
            Event::PostToolUse => "PostToolUse",
            Event::Notification => "Notification",
            Event::Stop => "Stop",
        }
    }

    /// The inverse, for the `--hook` side reading its own argument back.
    pub fn parse(name: &str) -> Option<Self> {
        Event::ALL.into_iter().find(|e| e.name() == name)
    }
}

/// What `event` does to a pane currently in `state`.
///
/// The interesting cases are the ones that *don't* transition. A `Notification`
/// means the agent is blocked on an answer, and nothing except your answer
/// clears that - so a `PostToolUse` arriving afterwards must not quietly demote
/// it back to "working", or a pane waiting on permission would stop saying so
/// the moment anything else happened in it.
///
/// `Exited` is terminal for the same reason in reverse: the process is gone, and
/// a late event from a hook that was already in flight cannot bring it back.
pub fn advance(state: &PaneState, event: Event, tool: Option<&str>) -> PaneState {
    if *state == PaneState::Exited {
        return PaneState::Exited;
    }
    match event {
        // Up and waiting for you, which is what a fresh agent is.
        Event::SessionStart => PaneState::Idle,
        Event::UserPromptSubmit => PaneState::Working { tool: None },
        Event::PreToolUse => PaneState::Working {
            tool: tool.map(str::to_string),
        },
        // Back to thinking - unless it is blocked, in which case the tool that
        // just finished was not what it is blocked on.
        Event::PostToolUse => match state {
            PaneState::Waiting => PaneState::Waiting,
            _ => PaneState::Working { tool: None },
        },
        Event::Notification => PaneState::Waiting,
        Event::Stop => PaneState::Idle,
    }
}

/// The `--settings` payload that registers `hook_bin` against all six events.
///
/// Layered over the user's own settings by claude rather than replacing them,
/// and written per-pane, so nothing in `~/.claude` is touched and their claude
/// in any other terminal is unaffected.
///
/// The bell hook rides along on `Stop` and `Notification`. It is the fallback:
/// if the socket could not be created, or a hook cannot reach it, those two
/// moments still light up a sidebar row exactly as they did before any of this
/// existed - which is the behaviour this feature is an improvement on, not a
/// replacement for.
pub fn settings_json(hook_bin: &str, bell_hook: &str) -> String {
    // Built as a value and serialised, rather than formatted as text. A hook is
    // a shell command containing quotes and backslashes, going into a JSON
    // string, inside a JSON document - and the first draft of this escaped it
    // once on the way into the command and again on the way into the document,
    // which turned the bell's `printf '\a'` into a literal backslash-a. The
    // encoder knows how many layers there are; a `replace` chain only knows how
    // many its author remembered.
    let command = |c: String| serde_json::json!({ "type": "command", "command": c });

    let mut hooks = serde_json::Map::new();
    for event in Event::ALL {
        // Single-quoted through `update::sh_quote`, because this string is a
        // *shell command line* - claude hands it to `sh -c` - and the path in it
        // is whatever prefix somebody installed the binary under.
        //
        // Double quotes were enough for the space in `/home/a b/` and for
        // nothing else. Inside them `sh` still expands `$`, still runs a
        // backtick, and still eats a backslash. So an install under
        // `/home/dev/src/agent$tile/target/release/agenttilecli` loses `$tile`
        // to an unset variable, all six hooks fail to exec a path that doesn't
        // exist, claude discards their stderr, and every pane in the window sits
        // on "starting…" for the entire session with nothing anywhere saying
        // why. That is the same silent failure the escaping test below is about,
        // arriving by a different route: claude runs a mangled command happily,
        // and the pane simply never reports anything. A directory whose name
        // contains a backtick or `$(…)` would be worse than broken - it would
        // *execute*, six times per agent turn.
        //
        // Single quotes suspend all of it, and `sh_quote` handles the one
        // character they cannot contain. `pane` already quotes the `--settings`
        // path beside this one the same way; this is the same launch and the
        // same problem.
        let mut commands = vec![command(format!(
            "{} --hook {}",
            crate::update::sh_quote(hook_bin),
            event.name(),
        ))];
        if matches!(event, Event::Stop | Event::Notification) {
            commands.push(command(bell_hook.to_string()));
        }
        hooks.insert(
            event.name().to_string(),
            serde_json::json!([{ "hooks": commands }]),
        );
    }
    serde_json::json!({ "hooks": hooks }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_round_trips_through_its_name() {
        for event in Event::ALL {
            assert_eq!(Event::parse(event.name()), Some(event));
        }
        assert_eq!(Event::parse("NoSuchEvent"), None);
    }

    /// A turn, start to finish, is the sequence this state machine exists for.
    #[test]
    fn a_whole_turn_reads_the_way_it_looks() {
        let mut s = PaneState::Starting;
        s = advance(&s, Event::SessionStart, None);
        assert_eq!(s, PaneState::Idle, "a fresh agent is waiting on you");

        s = advance(&s, Event::UserPromptSubmit, None);
        assert_eq!(s, PaneState::Working { tool: None });

        s = advance(&s, Event::PreToolUse, Some("Bash"));
        assert_eq!(
            s,
            PaneState::Working {
                tool: Some("Bash".into())
            },
            "and it says what it is doing",
        );

        s = advance(&s, Event::PostToolUse, None);
        assert_eq!(s, PaneState::Working { tool: None });

        s = advance(&s, Event::Stop, None);
        assert_eq!(s, PaneState::Idle, "the floor is yours again");
    }

    /// An agent blocked on a question stays blocked until you answer it. This is
    /// the transition worth getting right: it is the one the whole feature is
    /// for, and the one a naive "last event wins" would lose.
    #[test]
    fn a_pane_waiting_on_you_is_not_demoted_by_its_own_activity() {
        let waiting = advance(
            &PaneState::Working { tool: None },
            Event::Notification,
            None,
        );
        assert_eq!(waiting, PaneState::Waiting);

        let still = advance(&waiting, Event::PostToolUse, None);
        assert_eq!(
            still,
            PaneState::Waiting,
            "a tool finishing is not you answering",
        );
    }

    /// A dead pane stays dead, however late a hook arrives.
    #[test]
    fn nothing_resurrects_an_exited_pane() {
        for event in Event::ALL {
            assert_eq!(
                advance(&PaneState::Exited, event, Some("Bash")),
                PaneState::Exited
            );
        }
    }

    /// The settings file is a shell command inside a JSON string inside another
    /// JSON string, and a single lost backslash is silent: claude runs the
    /// mangled command happily and the pane simply never reports anything.
    #[test]
    fn the_settings_payload_survives_its_escaping() {
        let bell = r#"printf '\a' > "$PTY""#;
        let json = hooks_json(bell);

        // Valid JSON at all - the thing that is easiest to get wrong and
        // hardest to notice.
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("settings payload is not valid JSON");

        let hooks = &parsed["hooks"];
        for event in Event::ALL {
            assert!(
                !hooks[event.name()].is_null(),
                "{} is not registered",
                event.name(),
            );
        }

        // The bell survives, on exactly the two events it is the fallback for.
        let commands = |event: Event| {
            hooks[event.name()][0]["hooks"]
                .as_array()
                .expect("an array of hooks")
                .iter()
                .map(|h| h["command"].as_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
        };
        assert!(
            commands(Event::Stop)
                .iter()
                .any(|c| c.contains(r"printf '\a'")),
            "the bell fallback lost its escape: {:?}",
            commands(Event::Stop),
        );
        assert!(
            commands(Event::PreToolUse)
                .iter()
                .all(|c| !c.contains("printf")),
            "the bell should only ride on Stop and Notification",
        );
    }

    /// A path with a space in it is a path a great many people have - and a path
    /// with a `$`, a backtick or a quote in it is a path somebody has, because
    /// an install prefix is a directory name and a directory name can be
    /// anything.
    ///
    /// The command this ends up in is a *shell* command line, so the whole
    /// question is what `sh` does with the path once it is inside one. Hence a
    /// real `sh` and a real executable rather than assertions about quoting
    /// rules: a `$` that expands to nothing produces a path that doesn't exist,
    /// all six hooks fail, claude swallows their stderr, and the only visible
    /// symptom is every pane in the window sitting on "starting…" forever. That
    /// is not a failure anyone traces back to a pair of quotes by reading.
    ///
    /// The second half asserts the *old* form is still broken, so that anyone
    /// who "simplifies" this back to `"{hook_bin}"` gets a red test rather than
    /// a silent window.
    #[test]
    fn a_binary_path_the_shell_would_mangle_still_runs_one_binary() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        // Every character that has ever broken this: a space, a `$` that would
        // expand to nothing, a backtick that would *run* what it encloses, and
        // the single quote `sh_quote` has to escape by hand.
        let dir = std::env::temp_dir().join(format!(
            "agenttilecli hooks $tile `x` it's {}",
            std::process::id(),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        let binary = dir.join("agenttilecli");
        let arguments = dir.join("arguments");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
                crate::update::sh_quote(&arguments.to_string_lossy()),
            ),
        )
        .expect("write");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let path = binary.to_string_lossy().into_owned();
        let json = settings_json(&path, "true");

        // Valid JSON first - the payload is a shell command inside a JSON string
        // inside a JSON document, and the quoting added above is one more layer
        // for the encoder to get right.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .expect("a command");

        let status = Command::new("sh")
            .arg("-c")
            .arg(command)
            .status()
            .expect("sh runs");
        assert!(status.success(), "sh could not run the hook: {command}");
        let ran_with = std::fs::read_to_string(&arguments)
            .expect("the hook binary never ran - the path was mangled");
        assert_eq!(
            ran_with.lines().collect::<Vec<_>>(),
            ["--hook", "Stop"],
            "the hook ran, but not as one binary with two arguments",
        );

        // And the form this replaced, on the same path: double quotes leave `$`
        // and the backtick live, so `sh` builds a different path (and runs
        // whatever the backtick encloses on the way) and execs nothing.
        let _ = std::fs::remove_file(&arguments);
        let double_quoted = format!("\"{path}\" --hook Stop");
        let status = Command::new("sh")
            .arg("-c")
            .arg(&double_quoted)
            .status()
            .expect("sh runs");
        assert!(
            !status.success() && !arguments.exists(),
            "double quotes turned out to be enough for {double_quoted} - if that \
             is really true, this test and the quoting it guards can go",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn hooks_json(bell: &str) -> String {
        settings_json("/usr/bin/agenttilecli", bell)
    }
}
