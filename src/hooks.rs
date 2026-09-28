//! What an agent tells us about itself, and what it means.
//!
//! The only signal a pane has ever emitted is a bell byte (`pane::BELL_HOOK`),
//! rung when an agent finishes a turn or stops to ask. One byte, one meaning:
//! *something happened*. It is enough to flash a sidebar row and nothing more -
//! an agent that finishes in the group you are already looking at marks nothing
//! at all, and "three agents" on a strip cannot say whether that is three
//! working, three waiting on you, or three finished an hour ago.
//!
//! The supported agents will say considerably more than that if asked. They run
//! a command of our choosing at six points in their life, handing it a JSON
//! object on stdin,
//! and this module is the vocabulary for those six points plus the rule for
//! what each one does to a pane's state.
//!
//! Deliberately free of GTK, sockets and processes: what an event *means* is
//! the part worth testing, and it is testable without any of them. The wiring
//! that carries an event from claude to here lives in `ipc`.

use crate::model::PaneState;

/// The six moments, defined beside the rest of the wire protocol - see `wire`
/// for why that file has to stay free of everything this one is free of, and
/// more besides.
pub use crate::wire::Event;

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
///
/// `reason` is the agent's own word for a notification (see `wire::Message`),
/// and it decides what a `Notification` means - see `notification`.
pub fn advance(
    state: &PaneState,
    event: Event,
    tool: Option<&str>,
    reason: Option<&str>,
) -> PaneState {
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
        // Back to thinking - unless it is blocked on something *else*. The
        // tool it asked about finishing is the answer having been yes; any
        // other tool finishing (a parallel call that needed no permission) is
        // not you answering, and must not quietly take the amber off a pane
        // that is still waiting on you. A question that named no tool can't be
        // told apart from the rest, so it waits for something unambiguous: the
        // next tool starting, the turn ending, or your next prompt.
        Event::PostToolUse => match state {
            PaneState::Waiting { tool: Some(asked) } if tool == Some(asked.as_str()) => {
                PaneState::Working { tool: None }
            }
            PaneState::Waiting { .. } => state.clone(),
            _ => PaneState::Working { tool: None },
        },
        Event::Notification => match notification(reason) {
            // Claude asks twice about one prompt - `PermissionRequest` the
            // moment it appears, naming the tool, and a `Notification` six
            // seconds on, naming nothing. The second keeps what the first said.
            Notice::Question => match state {
                PaneState::Waiting { tool: Some(_) } if tool.is_none() => state.clone(),
                _ => PaneState::Waiting {
                    tool: tool.map(str::to_string),
                },
            },
            Notice::TurnOver => PaneState::Idle,
            Notice::Aside => state.clone(),
        },
        Event::Stop => PaneState::Idle,
    }
}

/// What a notification is saying.
#[derive(PartialEq, Eq, Debug)]
pub enum Notice {
    /// The agent has stopped and will not go on until you answer.
    Question,
    /// The turn is over and the agent is idle - which is a status, not a
    /// question, however it happens to be delivered.
    TurnOver,
    /// Something that changes nothing about what the agent is doing: a login
    /// that went through, a form that closed.
    Aside,
}

/// Sorts a notification by the agent's own type for it.
///
/// This is a bug fix before it is a feature. Every `Notification` used to mean
/// "asking permission", and claude sends one called `idle_prompt` about a minute
/// after *every* turn it finishes - so each claude you left alone for a minute
/// turned amber, said "asking permission" when it was asking nothing, and
/// breathed on screen until you next typed to it. That is the dot crying wolf
/// in exactly the case where a user has walked away and is relying on it.
///
/// No type at all is a question: that is what the keys registered as
/// `Notification` without one are (`PermissionRequest`, in claude and codex).
/// An unknown type is an aside rather than a question, because an agent adding
/// a new kind of ping is far more likely than it adding a new kind of question,
/// and a false amber is the expensive mistake here.
pub fn notification(reason: Option<&str>) -> Notice {
    match reason {
        None
        | Some(
            "permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog"
            | "agent_needs_input",
        ) => Notice::Question,
        Some("idle_prompt" | "agent_completed" | "task_complete") => Notice::TurnOver,
        Some(_) => Notice::Aside,
    }
}

/// What the fast hook binary is called. It is installed beside the window's
/// own (see `install.sh`), and cargo builds it beside it too.
const HOOK_BIN_NAME: &str = "agenttilecli-hook";

/// The program every agent is told to run as its hook.
///
/// `agenttilecli-hook` beside the running binary when it is there, and the
/// running binary itself when it isn't. Both answer to `--hook <event>`, so the
/// command line registered is the same either way and only the path moves.
///
/// The fallback is what keeps this safe to ship. A `cargo run` of the window
/// alone may never have built the other binary, and a clone updated by an older
/// `install.sh` has only the one file - and in both, pointing agents at a path
/// that doesn't exist would be every pane in the window sitting on "starting…"
/// for the whole session. Slow status is better than none.
pub fn hook_bin() -> Result<String, String> {
    Ok(hook_bin_beside(&crate::update::exe()?))
}

fn hook_bin_beside(exe: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let fast = std::path::Path::new(exe).with_file_name(HOOK_BIN_NAME);
    let runnable = std::fs::metadata(&fast)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
    if runnable {
        fast.to_string_lossy().into_owned()
    } else {
        exe.to_string()
    }
}

/// Writes a file every agent of one kind reads, atomically, and only if what it
/// says has changed.
///
/// Atomically because these files are read by agents starting up *while* the
/// window writes them for the next pane - four agents opened at once is four
/// launches, and a claude that reads its `--settings` file mid-rewrite finds
/// truncated JSON and runs without hooks. A rename is the one filesystem
/// operation a concurrent reader cannot see half of.
///
/// Only if changed because the content only moves when the theme or the binary's
/// path does, and an agent that watches its settings for edits would otherwise
/// be told they had changed every time a neighbouring pane opened.
pub fn write_if_changed(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|current| current == contents) {
        return Ok(());
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".{}.new", std::process::id()));
    let temporary = std::path::PathBuf::from(temporary);
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// Adds our event entries to a hook file of theirs, keeping both.
///
/// Codex and Grok both read hooks from a home directory, and both private homes
/// (`codex_home`, `grok_home`) have the same problem there: the user may already
/// keep a hook file of their own at the very name ours needs. Their hooks are
/// something they set up on purpose, for reasons that have nothing to do with
/// this app, so ours are added to theirs rather than replacing them - somebody
/// who loses their own hooks by opening a window manager has no reason at all to
/// connect the two events.
///
/// Anything unparseable on their side means ours alone: an agent that cannot
/// read their file was not going to run their hooks either, and losing our dots
/// as well would help nobody.
pub fn merge_hook_files(theirs: &str, ours: &str) -> String {
    let (Ok(mut theirs), Ok(ours)) = (
        serde_json::from_str::<serde_json::Value>(theirs),
        serde_json::from_str::<serde_json::Value>(ours),
    ) else {
        return ours.to_string();
    };

    let Some(our_events) = ours["hooks"].as_object().cloned() else {
        return ours.to_string();
    };
    if !theirs["hooks"].is_object() {
        theirs["hooks"] = serde_json::json!({});
    }
    let Some(their_events) = theirs["hooks"].as_object_mut() else {
        return ours.to_string();
    };
    for (event, entries) in our_events {
        let slot = their_events
            .entry(event)
            .or_insert_with(|| serde_json::json!([]));
        match (slot.as_array_mut(), entries.as_array()) {
            (Some(slot), Some(entries)) => slot.extend(entries.iter().cloned()),
            // Their value for this event isn't a list at all. It is not this
            // app's job to have an opinion about that, but it is this app's job
            // to still get its own hook registered.
            _ => *slot = entries,
        }
    }
    theirs.to_string()
}

/// Whether the bell rides on a hook key, and on which of its firings.
#[derive(Clone, Copy)]
enum Bell {
    Never,
    Always,
    /// Only when the agent's own matcher accepts it - for a key like claude's
    /// `Notification`, which is a question on some firings and a status ping on
    /// others. The matcher is the agent's: a regular expression over the
    /// notification type.
    When(&'static str),
}

/// One key an agent fires a hook under, and what it means here.
///
/// The window thinks in six moments (see `Event`), and every agent fires more
/// keys than that, under names of its own. This is where the two vocabularies
/// meet - one table per agent, so the difference between them is a list you can
/// read rather than a set of special cases spread across three writers.
struct Registration {
    key: &'static str,
    event: Event,
    bell: Bell,
}

const fn reg(key: &'static str, event: Event, bell: Bell) -> Registration {
    Registration { key, event, bell }
}

/// The notification types that are an agent asking you something, as a matcher
/// both claude and grok accept. Everything else a `Notification` can be -
/// `idle_prompt` above all - is the agent saying it is still there, and ringing
/// the bell for that is a second flash for a turn that already had one.
const QUESTIONS: &str = "permission_prompt|elicitation_dialog|elicitation_url_dialog|agent_needs_input";

/// Claude's questions, less the one it also announces through
/// `PermissionRequest`. Claude says "may I run this?" twice - the moment the
/// prompt appears, and again as a `permission_prompt` notification six seconds
/// on - and a bell on both was a second flash for one question.
const CLAUDE_QUESTIONS: &str = "elicitation_dialog|elicitation_url_dialog|agent_needs_input";

/// Claude's keys, from its hook reference.
///
/// Three beyond the six, each closing a gap the six left open:
///
/// - `PermissionRequest` is the moment claude puts a permission dialog up.
///   `Notification`'s `permission_prompt` says the same thing, but only after
///   the dialog has sat unanswered for about six seconds - so the amber dot,
///   the one signal this whole feature exists for, used to arrive six seconds
///   after the thing it was reporting.
/// - `StopFailure` is how a turn ends when the API fails it. Without it a rate
///   limit left the pane saying "working" for as long as nobody looked.
/// - `PostToolUseFailure` is `PostToolUse` for a tool that failed, and without
///   it the strip kept naming the failed tool until the next one started.
const CLAUDE: &[Registration] = &[
    reg("SessionStart", Event::SessionStart, Bell::Never),
    reg("UserPromptSubmit", Event::UserPromptSubmit, Bell::Never),
    reg("PreToolUse", Event::PreToolUse, Bell::Never),
    reg("PostToolUse", Event::PostToolUse, Bell::Never),
    reg("PostToolUseFailure", Event::PostToolUse, Bell::Never),
    reg("PermissionRequest", Event::Notification, Bell::Always),
    reg("Notification", Event::Notification, Bell::When(CLAUDE_QUESTIONS)),
    reg("Stop", Event::Stop, Bell::Always),
    reg("StopFailure", Event::Stop, Bell::Always),
];

/// Codex's keys. It has no `Notification` at all: `PermissionRequest` is its
/// name for the blocked moment, which is why the state machine needed no
/// changes to gain a second agent. `Interrupt` is how a turn you stopped ends -
/// no bell, because you are the one who stopped it and you are looking at it.
const CODEX: &[Registration] = &[
    reg("SessionStart", Event::SessionStart, Bell::Never),
    reg("UserPromptSubmit", Event::UserPromptSubmit, Bell::Never),
    reg("PreToolUse", Event::PreToolUse, Bell::Never),
    reg("PostToolUse", Event::PostToolUse, Bell::Never),
    reg("PermissionRequest", Event::Notification, Bell::Always),
    reg("Stop", Event::Stop, Bell::Always),
    reg("Interrupt", Event::Stop, Bell::Never),
];

/// Grok's keys. It speaks claude's vocabulary and adds `StopCancelled`, which
/// fires *instead of* `Stop` when a turn is interrupted or a permission refused -
/// so without it, pressing Ctrl+C left a grok pane saying "working" until the
/// next prompt.
const GROK: &[Registration] = &[
    reg("SessionStart", Event::SessionStart, Bell::Never),
    reg("UserPromptSubmit", Event::UserPromptSubmit, Bell::Never),
    reg("PreToolUse", Event::PreToolUse, Bell::Never),
    reg("PostToolUse", Event::PostToolUse, Bell::Never),
    reg("PostToolUseFailure", Event::PostToolUse, Bell::Never),
    reg("Notification", Event::Notification, Bell::When(QUESTIONS)),
    reg("Stop", Event::Stop, Bell::Always),
    reg("StopFailure", Event::Stop, Bell::Always),
    reg("StopCancelled", Event::Stop, Bell::Never),
];

fn registrations(kind: crate::agent::Kind) -> &'static [Registration] {
    match kind {
        crate::agent::Kind::Claude => CLAUDE,
        crate::agent::Kind::Codex => CODEX,
        crate::agent::Kind::Grok => GROK,
    }
}

/// The `hooks` object for `kind`: every key it fires, pointed at `hook_bin`
/// with the moment that key means here, and the bell where it belongs.
///
/// Every agent reads the same shape - an event key holding a list of matcher
/// groups, each holding a list of commands - which is why one writer serves
/// all three and only the table differs.
fn hooks_table(kind: crate::agent::Kind, hook_bin: &str, bell_hook: &str) -> serde_json::Value {
    // Built as a value and serialised, rather than formatted as text. A hook is
    // a shell command containing quotes and backslashes, going into a JSON
    // string, inside a JSON document - and the first draft of this escaped it
    // once on the way into the command and again on the way into the document,
    // which turned the bell's `printf '\a'` into a literal backslash-a. The
    // encoder knows how many layers there are; a `replace` chain only knows how
    // many its author remembered.
    let command = |c: String| serde_json::json!({ "type": "command", "command": c });

    let mut hooks = serde_json::Map::new();
    for registration in registrations(kind) {
        // Single-quoted through `update::sh_quote`, because this string is a
        // *shell command line* - the agent hands it to `sh -c` - and the path in
        // it is whatever prefix somebody installed the binary under.
        //
        // Double quotes were enough for the space in `/home/a b/` and for
        // nothing else. Inside them `sh` still expands `$`, still runs a
        // backtick, and still eats a backslash. So an install under
        // `/home/dev/src/agent$tile/target/release/agenttilecli` loses `$tile`
        // to an unset variable, every hook fails to exec a path that doesn't
        // exist, the agent discards their stderr, and every pane in the window
        // sits on "starting…" for the entire session with nothing anywhere
        // saying why. A directory whose name contains a backtick or `$(…)` would
        // be worse than broken - it would *execute*, several times per turn.
        //
        // Single quotes suspend all of it, and `sh_quote` handles the one
        // character they cannot contain.
        //
        // Synchronous, deliberately, where claude and codex would both accept
        // `"async": true`. Asynchronous hooks carry no ordering promise, and
        // these feed a state machine: a `PostToolUse` overtaking its own
        // `PreToolUse` leaves a pane naming a tool that finished long ago. The
        // hook binary costs a quarter of a millisecond (see `wire`), which is
        // less than the agent spends starting the shell that runs it.
        let ours = command(format!(
            "{} --hook {}",
            crate::update::sh_quote(hook_bin),
            registration.event.name(),
        ));
        let bell = command(bell_hook.to_string());
        let groups = match registration.bell {
            Bell::Never => serde_json::json!([{ "hooks": [ours] }]),
            Bell::Always => serde_json::json!([{ "hooks": [ours, bell] }]),
            Bell::When(matcher) => serde_json::json!([
                { "hooks": [ours] },
                { "matcher": matcher, "hooks": [bell] },
            ]),
        };
        hooks.insert(registration.key.to_string(), groups);
    }
    serde_json::Value::Object(hooks)
}

/// The `--settings` payload that registers `hook_bin` against every moment
/// claude reports, and names the theme claude should draw itself in.
///
/// Layered over the user's own settings by claude rather than replacing them,
/// and written per-pane, so nothing in `~/.claude` is touched and their claude
/// in any other terminal is unaffected.
///
/// The bell hook rides along on the moments worth interrupting someone for. It
/// is the fallback: if the socket could not be created, or a hook cannot reach
/// it, those moments still light up a sidebar row exactly as they did before
/// any of this existed - which is the behaviour this feature is an improvement
/// on, not a replacement for.
pub fn settings_json(hook_bin: &str, bell_hook: &str, theme: Option<&str>) -> String {
    let mut settings = serde_json::Map::new();
    settings.insert(
        "hooks".to_string(),
        hooks_table(crate::agent::Kind::Claude, hook_bin, bell_hook),
    );

    // Present only when the desktop has a theme, and absent rather than set to
    // some default when it doesn't. `--settings` outranks the user's own
    // `~/.claude/settings.json`, so a key written here is a key they cannot
    // override - and "no opinion" has to be expressible, or a machine with no
    // Omarchy on it would have this app quietly overriding a theme its owner
    // chose by hand.
    if let Some(theme) = theme {
        settings.insert("theme".to_string(), serde_json::Value::from(theme));
    }

    serde_json::Value::Object(settings).to_string()
}

/// The `hooks.json` written into the private `CODEX_HOME`.
///
/// Codex has no `--settings`. Its hooks load from its home directory or from
/// the repo, and from nowhere else - so this file goes into a home of our own
/// making (see `codex_home`) and the user's real `~/.codex` is never written
/// to. Same promise the claude side keeps, by a harder route.
pub fn codex_hooks_json(hook_bin: &str, bell_hook: &str) -> String {
    serde_json::json!({
        "hooks": hooks_table(crate::agent::Kind::Codex, hook_bin, bell_hook)
    })
    .to_string()
}

/// The global hook file written into the private `GROK_HOME`, in the claude
/// format grok deliberately accepts.
pub fn grok_hooks_json(hook_bin: &str, bell_hook: &str) -> String {
    serde_json::json!({
        "hooks": hooks_table(crate::agent::Kind::Grok, hook_bin, bell_hook)
    })
    .to_string()
}

#[cfg(test)]
mod tests {

    /// The half of "the desktop's theme reaches claude" that lives outside the
    /// terminal.
    ///
    /// Handing VTE the theme's sixteen colours does nothing on its own: claude
    /// renders from its own hexes unless it is told to render from the
    /// terminal's palette, and `dark-ansi` is that instruction. The two changes
    /// only mean anything together, which is why this test names the value
    /// rather than merely checking that some theme was set.
    #[test]
    fn a_themed_desktop_tells_claude_to_draw_from_the_terminals_palette() {
        let json = settings_json("/usr/bin/agenttilecli", "true", Some("dark-ansi"));
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(parsed["theme"], "dark-ansi");
        // The hooks are still there beside it - this file's original job.
        assert!(parsed["hooks"].is_object());
    }

    /// No theme means no key, not a default one. `--settings` outranks the
    /// user's own settings file, so a `"theme"` written here on a machine with
    /// no Omarchy would silently overrule a choice they made by hand.
    #[test]
    fn an_unthemed_desktop_leaves_claudes_own_theme_alone() {
        let json = settings_json("/usr/bin/agenttilecli", "true", None);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert!(
            parsed.get("theme").is_none(),
            "wrote a theme key with no theme to write: {json}"
        );
    }
    use super::*;

    /// Every command registered under `key` in a hooks object, across all of
    /// its matcher groups.
    fn commands_under(hooks: &serde_json::Value, key: &str) -> Vec<String> {
        hooks[key]
            .as_array()
            .unwrap_or_else(|| panic!("{key} is not registered"))
            .iter()
            .flat_map(|group| group["hooks"].as_array().cloned().unwrap_or_default())
            .map(|h| h["command"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// The moment the command under `key` reports, read back out of its argv.
    fn reported_under(hooks: &serde_json::Value, key: &str) -> Option<Event> {
        commands_under(hooks, key)
            .iter()
            .find_map(|c| c.split("--hook ").nth(1).and_then(Event::parse))
    }

    /// Every agent reports every one of the six moments under *some* key. A
    /// moment no key reports is a dot that can never reach that state, and the
    /// failure is silent - nothing complains, the pane just never says it.
    #[test]
    fn every_agent_reports_all_six_moments() {
        for kind in crate::agent::Kind::ALL {
            let table = hooks_table(kind, "/usr/bin/agenttilecli-hook", "true");
            let keys: Vec<String> = table.as_object().expect("an object").keys().cloned().collect();
            for event in Event::ALL {
                assert!(
                    keys.iter().any(|key| reported_under(&table, key) == Some(event)),
                    "{} never reports {}",
                    kind.label(),
                    event.name(),
                );
            }
        }
    }

    /// The argv keeps the window's vocabulary whatever the key is called, since
    /// that argument is what `Event::parse` reads back. `PermissionRequest` is
    /// reported *as* `Notification`, not under its own name.
    #[test]
    fn a_key_in_the_agents_vocabulary_reports_a_moment_in_ours() {
        let codex: serde_json::Value =
            serde_json::from_str(&codex_hooks_json("/bin/h", "true")).expect("valid JSON");
        assert_eq!(
            reported_under(&codex["hooks"], "PermissionRequest"),
            Some(Event::Notification),
        );
        assert_eq!(reported_under(&codex["hooks"], "Interrupt"), Some(Event::Stop));

        let claude: serde_json::Value =
            serde_json::from_str(&settings_json("/bin/h", "true", None)).expect("valid JSON");
        assert_eq!(
            reported_under(&claude["hooks"], "PermissionRequest"),
            Some(Event::Notification),
            "claude's immediate permission signal, rather than the one six seconds late",
        );
        assert_eq!(reported_under(&claude["hooks"], "StopFailure"), Some(Event::Stop));

        let grok: serde_json::Value =
            serde_json::from_str(&grok_hooks_json("/bin/h", "true")).expect("valid JSON");
        assert_eq!(reported_under(&grok["hooks"], "StopCancelled"), Some(Event::Stop));
    }

    /// Each agent's file names only keys that agent fires. A key it doesn't know
    /// is at best a hook that never runs, and codex has no `Notification` at
    /// all, so registering the blocked moment under that name there would leave
    /// the amber dot unlit, on the one transition the whole feature exists for.
    #[test]
    fn no_agent_is_handed_another_agents_words() {
        let codex: serde_json::Value =
            serde_json::from_str(&codex_hooks_json("/bin/h", "true")).expect("valid JSON");
        assert!(codex["hooks"].get("Notification").is_none());
        assert!(codex["hooks"].get("StopCancelled").is_none());

        let grok: serde_json::Value =
            serde_json::from_str(&grok_hooks_json("/bin/h", "true")).expect("valid JSON");
        assert!(grok["hooks"].get("PermissionRequest").is_none());
        assert!(grok["hooks"].get("Interrupt").is_none());

        let claude: serde_json::Value =
            serde_json::from_str(&settings_json("/bin/h", "true", None)).expect("valid JSON");
        assert!(claude["hooks"].get("Interrupt").is_none());
        assert!(claude["hooks"].get("StopCancelled").is_none());
    }

    /// The bell rings for the moments worth turning your head for and no others -
    /// and on a `Notification` only when it is a question, since claude's
    /// minute-later idle ping is not one.
    #[test]
    fn the_bell_rings_for_questions_and_finished_turns_only() {
        let bell = r#"printf '\a' > "$PTY""#;
        for (kind, json) in [
            ("claude", settings_json("/bin/h", bell, None)),
            ("codex", codex_hooks_json("/bin/h", bell)),
            ("grok", grok_hooks_json("/bin/h", bell)),
        ] {
            let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
            let hooks = &parsed["hooks"];
            let rings = |key: &str| commands_under(hooks, key).iter().any(|c| c.contains("printf"));
            assert!(rings("Stop"), "{kind}: the bell rides on Stop");
            assert!(!rings("PreToolUse"), "{kind}: and not on every tool");
            assert!(!rings("UserPromptSubmit"), "{kind}: nor on your own typing");
        }

        let claude: serde_json::Value =
            serde_json::from_str(&settings_json("/bin/h", bell, None)).expect("valid JSON");
        let groups = claude["hooks"]["Notification"].as_array().expect("groups");
        let bell_group = groups
            .iter()
            .find(|g| g["hooks"].to_string().contains("printf"))
            .expect("a bell on Notification");
        let matcher = bell_group["matcher"].as_str().expect("a matcher on it");
        assert!(matcher.contains("elicitation_dialog"), "{matcher}");
        assert!(
            !matcher.contains("permission_prompt"),
            "PermissionRequest already rang for that prompt: {matcher}",
        );
        assert!(
            !matcher.contains("idle_prompt"),
            "the minute-later idle ping must not ring: {matcher}",
        );
        assert!(
            groups
                .iter()
                .any(|g| g.get("matcher").is_none() && g["hooks"].to_string().contains("--hook")),
            "while our own hook hears every notification, idle ones included",
        );
    }

    /// Same silent-failure argument as the claude payload: a path the shell
    /// would mangle produces six hooks that fail to exec, stderr nobody sees,
    /// and a pane that sits on "starting…" for the whole session.
    #[test]
    fn the_codex_payload_survives_a_path_the_shell_would_mangle() {
        let json = codex_hooks_json("/home/dev/agent$tile/agenttilecli", "true");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .expect("a command");
        assert!(
            command.contains("'/home/dev/agent$tile/agenttilecli'"),
            "the path lost its quoting: {command}",
        );
    }

    /// Grok reads claude's JSON shape from `$GROK_HOME/hooks/*.json`, under
    /// its own - mostly claude's - event names.
    #[test]
    fn the_grok_payload_is_in_grok_vocabulary() {
        let json = grok_hooks_json("/opt/agent tile/agenttilecli", "printf '\\a'");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let hooks = &parsed["hooks"];
        for key in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Notification", "Stop"] {
            assert!(hooks[key].is_array(), "{key} is not registered");
        }
        assert!(
            commands_under(hooks, "Notification").iter().any(|c| c.contains("printf")),
            "the bell rides on grok's questions too",
        );
    }

    /// A turn, start to finish, is the sequence this state machine exists for.
    #[test]
    fn a_whole_turn_reads_the_way_it_looks() {
        let mut s = PaneState::Starting;
        s = advance(&s, Event::SessionStart, None, None);
        assert_eq!(s, PaneState::Idle, "a fresh agent is waiting on you");

        s = advance(&s, Event::UserPromptSubmit, None, None);
        assert_eq!(s, PaneState::Working { tool: None });

        s = advance(&s, Event::PreToolUse, Some("Bash"), None);
        assert_eq!(
            s,
            PaneState::Working {
                tool: Some("Bash".into())
            },
            "and it says what it is doing",
        );

        s = advance(&s, Event::PostToolUse, None, None);
        assert_eq!(s, PaneState::Working { tool: None });

        s = advance(&s, Event::Stop, None, None);
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
            Some("Bash"),
            None,
        );
        assert_eq!(waiting, PaneState::Waiting { tool: Some("Bash".into()) });

        let still = advance(&waiting, Event::PostToolUse, Some("Read"), None);
        assert_eq!(still, waiting, "another tool finishing is not you answering");

        // Claude's second, later word about the same prompt names no tool, and
        // must not forget the one the first named.
        let later = advance(&still, Event::Notification, None, Some("permission_prompt"));
        assert_eq!(later, waiting);

        let answered = advance(&later, Event::PostToolUse, Some("Bash"), None);
        assert_eq!(
            answered,
            PaneState::Working { tool: None },
            "the tool it asked about running is the answer having been yes",
        );

        // A question that named nothing waits for something unambiguous.
        let vague = advance(&PaneState::Idle, Event::Notification, None, Some("permission_prompt"));
        assert_eq!(advance(&vague, Event::PostToolUse, Some("Bash"), None), vague);
        assert_eq!(
            advance(&vague, Event::PreToolUse, Some("Bash"), None),
            PaneState::Working { tool: Some("Bash".into()) },
        );
    }

    /// A dead pane stays dead, however late a hook arrives.
    #[test]
    fn nothing_resurrects_an_exited_pane() {
        for event in Event::ALL {
            assert_eq!(
                advance(&PaneState::Exited, event, Some("Bash"), None),
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

        // The bell survives its escaping.
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
            "the bell should not ride on every tool",
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
        let json = settings_json(&path, "true", None);

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

    /// The bug this sorting exists to fix: claude's idle ping, a minute after a
    /// finished turn, used to paint the pane amber and label it "asking
    /// permission" - so every claude you walked away from cried wolf.
    #[test]
    fn claudes_idle_ping_is_not_a_question() {
        let finished = advance(&PaneState::Working { tool: None }, Event::Stop, None, None);
        assert_eq!(finished, PaneState::Idle);
        assert_eq!(
            advance(&finished, Event::Notification, None, Some("idle_prompt")),
            PaneState::Idle,
            "a minute of quiet is not a permission prompt",
        );
    }

    /// An idle ping also closes a turn that ended without a `Stop` - an
    /// interrupt, in claude, which fires none.
    #[test]
    fn an_idle_ping_settles_a_turn_that_never_said_it_stopped() {
        assert_eq!(
            advance(
                &PaneState::Working { tool: Some("Bash".into()) },
                Event::Notification,
                None,
                Some("idle_prompt"),
            ),
            PaneState::Idle,
        );
    }

    #[test]
    fn a_notification_is_sorted_by_its_type() {
        assert_eq!(notification(None), Notice::Question, "PermissionRequest carries none");
        assert_eq!(notification(Some("permission_prompt")), Notice::Question);
        assert_eq!(notification(Some("elicitation_dialog")), Notice::Question);
        assert_eq!(notification(Some("idle_prompt")), Notice::TurnOver);
        assert_eq!(notification(Some("auth_success")), Notice::Aside);
        assert_eq!(
            notification(Some("some_future_ping")),
            Notice::Aside,
            "an unknown type must not light the amber dot",
        );
        assert_eq!(
            advance(&PaneState::Working { tool: None }, Event::Notification, None, Some("auth_success")),
            PaneState::Working { tool: None },
            "an aside changes nothing",
        );
    }

    fn hooks_json(bell: &str) -> String {
        settings_json("/usr/bin/agenttilecli", bell, None)
    }
}
