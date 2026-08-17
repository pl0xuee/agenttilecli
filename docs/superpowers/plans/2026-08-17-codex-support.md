# Codex Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `codex` as a first-class second agent alongside `claude` — pickable per pane, mixable inside one project, reporting into the same status dots.

**Architecture:** A new GTK-free `agent` module holds a two-variant `Kind` enum carrying each agent's label, default command and hook-install strategy. Claude keeps its `--settings` layer; codex gets a private `CODEX_HOME` built as a symlink farm over the user's real one, with our own `hooks.json` beside the links. Pane identity already travels in the environment, so no IPC changes are needed.

**Tech Stack:** Rust 2024, GTK4 + libadwaita 1.7, VTE, serde/serde_json, toml.

**Spec:** `docs/superpowers/specs/2026-08-17-codex-support-design.md`

## Global Constraints

- **Never close or restart the installed AgentTileCLI.** It is the user's live working environment. Test only via `scripts/dev-run.sh`, which requires a non-`master` branch and sandboxes the XDG dirs.
- **Never write to `~/.claude` or `~/.codex`.** Both agents' homes are read-only to this app. This is a standing promise stated in `src/pane.rs` and the README.
- Work happens on branch `codex-support`.
- Every hook-install failure is **non-fatal**: fall back to launching the bare command, leaving the dot hollow and the bell as the signal.
- Existing `config.toml` and `session.json` files must keep working untouched.
- Comment style: this codebase explains *why*, in prose, at the point of the decision. Match it. Do not add comments that restate the code.
- `codex` is **not installed on this machine.** No task may claim the codex launch path is verified end to end.

---

### Task 1: The agent module

**Files:**
- Create: `src/agent.rs`
- Modify: `src/main.rs` (add `mod agent;` beside the other module declarations)

**Interfaces:**
- Consumes: nothing.
- Produces: `agent::Kind` (`Kind::Claude`, `Kind::Codex`), `Kind::ALL: [Kind; 2]`, `Kind::label(self) -> &'static str`, `Kind::default_command(self) -> &'static str`, `Kind::parse(&str) -> Option<Kind>`.

- [ ] **Step 1: Write the failing test**

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib agent::`
Expected: FAIL — `src/agent.rs` does not exist / `mod agent` unresolved.

- [ ] **Step 3: Write minimal implementation**

Create `src/agent.rs`:

```rust
//! Which agent a pane is running.
//!
//! Two of them, named and known: adding a third is a code change, deliberately.
//! What varies between them is small and awkward - what the binary is called,
//! how you get your hooks in front of it, what it calls each moment of a turn -
//! and an enum keeps the two answers to each question on adjacent lines, where
//! a difference is visible. A trait would put them in separate files and buy
//! extensibility nobody has asked for.
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
```

Add `mod agent;` to `src/main.rs` alongside the existing module declarations.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib agent::`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/agent.rs src/main.rs
git commit -m "Name the two agents this app knows"
```

---

### Task 2: Codex's hook payload

**Files:**
- Modify: `src/hooks.rs` (add `Event::codex_key`, add `codex_hooks_json`; leave `settings_json` and `advance` alone)

**Interfaces:**
- Consumes: `hooks::Event`, `hooks::Event::ALL`, `hooks::Event::name`, `crate::update::sh_quote`.
- Produces: `hooks::Event::codex_key(self) -> &'static str`, `hooks::codex_hooks_json(hook_bin: &str, bell_hook: &str) -> String`.

**Why the argv stays claude's word:** the `--hook <name>` argument is a string this app writes and this app parses (`main.rs::hook_event`). So codex's file keys `PermissionRequest` while still invoking `--hook Notification`, and `Event::parse` needs no new alias.

**Unverified detail to flag in the code:** whether a standalone `hooks.json` wraps its event map in a `"hooks"` key or is the map itself. The documented `config.toml` form is `[[hooks.PreToolUse]]`, which implies a wrapper. This task writes the wrapper and leaves a comment saying it is the seam to check first against a real codex.

- [ ] **Step 1: Write the failing test**

Add to `src/hooks.rs`'s `mod tests`:

```rust
/// The two agents differ by exactly one word, and it is the word that matters
/// most: the moment an agent stops to ask you something. Getting it wrong
/// costs the amber dot and nothing else complains.
#[test]
fn codex_names_the_blocked_moment_its_own_way() {
    assert_eq!(Event::Notification.codex_key(), "PermissionRequest");
    for event in Event::ALL {
        if event != Event::Notification {
            assert_eq!(
                event.codex_key(),
                event.name(),
                "{} is spelt the same by both agents",
                event.name(),
            );
        }
    }
}

/// Codex's file is a different shape to claude's, so it gets its own test
/// rather than sharing one: what is common to both is only that a mangled
/// path fails silently in exactly the same way.
#[test]
fn the_codex_payload_registers_every_event_against_our_binary() {
    let bell = r#"printf '\a' > "$PTY""#;
    let json = codex_hooks_json("/opt/agent tile/agenttilecli", bell);

    let parsed: serde_json::Value =
        serde_json::from_str(&json).expect("codex hooks payload is not valid JSON");
    let hooks = &parsed["hooks"];

    for event in Event::ALL {
        let entry = &hooks[event.codex_key()];
        assert!(!entry.is_null(), "{} is not registered", event.codex_key());
        let commands: Vec<String> = entry[0]["hooks"]
            .as_array()
            .expect("an array of hooks")
            .iter()
            .map(|h| h["command"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(
            commands.iter().any(|c| c.contains("--hook")),
            "{} reports nothing: {commands:?}",
            event.codex_key(),
        );
        assert!(
            commands
                .iter()
                .all(|c| !c.contains("PermissionRequest")),
            "the argv keeps claude's vocabulary, which is what main.rs parses",
        );
    }

    // Claude's key must not leak into codex's file - it would register an
    // event codex never fires, and the amber dot would simply never light.
    assert!(
        hooks.get("Notification").is_none(),
        "Notification is claude's word, not codex's",
    );
}

/// Same silent-failure argument as the claude payload: a path the shell would
/// mangle produces six hooks that fail to exec, stderr nobody sees, and a pane
/// that sits on "starting..." for the whole session.
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib hooks::`
Expected: FAIL — no method `codex_key`, no function `codex_hooks_json`.

- [ ] **Step 3: Write minimal implementation**

Add to `impl Event` in `src/hooks.rs`:

```rust
    /// What codex calls this moment in its own hook config.
    ///
    /// Identical to `name` everywhere except the one that matters: claude's
    /// `Notification` is codex's `PermissionRequest`. Both mean the same thing
    /// to `advance` - the agent has stopped and wants an answer - which is why
    /// the state machine needed no changes to gain a second agent.
    ///
    /// Only the *key* differs. The command this registers still passes
    /// `--hook Notification`, because that argument is parsed by `Event::parse`
    /// in this process, not by either agent.
    pub fn codex_key(self) -> &'static str {
        match self {
            Event::Notification => "PermissionRequest",
            other => other.name(),
        }
    }
```

Add beside `settings_json`:

```rust
/// The `hooks.json` written into the private `CODEX_HOME`, registering
/// `hook_bin` against all six events.
///
/// Codex has no `--settings`: hooks load from its home or the repo, and from
/// nowhere else. So this file goes into a home of our own making (see
/// `pane::codex_home`) and the user's real `~/.codex` is never written to -
/// the same promise the claude side keeps by a easier route.
///
/// The `"hooks"` wrapper is the one thing here taken from documentation rather
/// than from a running codex: the documented `config.toml` form nests under a
/// `hooks` table, so a standalone file is written to match. If codex ever
/// reports finding no hooks, this wrapper is the first thing to try removing.
pub fn codex_hooks_json(hook_bin: &str, bell_hook: &str) -> String {
    let command = |c: String| serde_json::json!({ "type": "command", "command": c });

    let mut hooks = serde_json::Map::new();
    for event in Event::ALL {
        // Single-quoted for the reason spelt out in `settings_json`: this is a
        // shell command line carrying an install prefix, and an unquoted `$`
        // costs every dot in the window with nothing said about it.
        let mut commands = vec![command(format!(
            "{} --hook {}",
            crate::update::sh_quote(hook_bin),
            event.name(),
        ))];
        if matches!(event, Event::Stop | Event::Notification) {
            commands.push(command(bell_hook.to_string()));
        }
        hooks.insert(
            event.codex_key().to_string(),
            serde_json::json!([{ "hooks": commands }]),
        );
    }
    serde_json::json!({ "hooks": hooks }).to_string()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib hooks::`
Expected: PASS — the three new tests plus the five existing ones.

- [ ] **Step 5: Commit**

```bash
git add src/hooks.rs
git commit -m "Say the six moments in codex's dialect"
```

---

### Task 3: Config learns two agents

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Consumes: `agent::Kind`.
- Produces: `Config::default_agent: String`, `Config::agent: AgentTable`, `Config::command_for(&self, Kind) -> String`, `Config::default_kind(&self) -> Kind`. `Config::command` becomes `Option<String>` (deprecated alias for claude's).

**The back-compat rule:** `deny_unknown_fields` is on, so deleting top-level `command` would turn every existing user's config into a startup error dialog. It keeps parsing, means *claude's* command, and reports itself deprecated through the existing `problem()` channel.

- [ ] **Step 1: Write the failing test**

Add to `src/config.rs`'s `mod tests`:

```rust
/// Every config file in the wild has `command` in it, and `deny_unknown_fields`
/// turns a key we stopped honouring into a dialog on startup. So it keeps
/// working, and says what to write instead.
#[test]
fn the_old_command_key_still_names_claudes_command() {
    let loaded = Config::parse("command = \"claude --model opus\"\n", "config.toml");
    assert_eq!(
        loaded.config.command_for(Kind::Claude),
        "claude --model opus",
    );
    let problem = loaded.problem.expect("a deprecated key is mentioned");
    assert!(problem.contains("command"), "names the key: {problem}");
    assert!(problem.contains("agent.claude"), "says what to write instead: {problem}");
}

/// The lossy case, and the reason the deprecation text has to be specific: this
/// person was trying to run codex, it has never worked, and mapping them to
/// claude silently would be the third confusing thing to happen to them.
#[test]
fn an_old_command_naming_codex_is_called_out() {
    let loaded = Config::parse("command = \"codex\"\n", "config.toml");
    let problem = loaded.problem.expect("this case is explained");
    assert!(
        problem.contains("default_agent"),
        "points at the key that actually switches agent: {problem}",
    );
}

#[test]
fn each_agent_can_be_given_its_own_command() {
    let loaded = Config::parse(
        "default_agent = \"codex\"\n\
         [agent.claude]\ncommand = \"claude --model opus\"\n\
         [agent.codex]\ncommand = \"codex --full-auto\"\n",
        "config.toml",
    );
    assert!(loaded.problem.is_none(), "{:?}", loaded.problem);
    assert_eq!(loaded.config.default_kind(), Kind::Codex);
    assert_eq!(loaded.config.command_for(Kind::Claude), "claude --model opus");
    assert_eq!(loaded.config.command_for(Kind::Codex), "codex --full-auto");
}

#[test]
fn an_unnamed_agent_falls_back_to_its_own_binary() {
    let loaded = Config::parse("", "config.toml");
    assert_eq!(loaded.config.command_for(Kind::Claude), "claude");
    assert_eq!(loaded.config.command_for(Kind::Codex), "codex");
    assert_eq!(loaded.config.default_kind(), Kind::Claude);
}

/// A default_agent naming something we don't have is a typo, and typing
/// `default_agent = "opus"` and getting claude silently is how a config file
/// earns its reputation for doing nothing.
#[test]
fn an_unknown_default_agent_is_reported() {
    let loaded = Config::parse("default_agent = \"gemini\"\n", "config.toml");
    let problem = loaded.problem.expect("an unknown agent is reported");
    assert!(problem.contains("gemini"), "names it: {problem}");
    assert_eq!(loaded.config.default_kind(), Kind::Claude, "and falls back");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib config::`
Expected: FAIL — no `command_for`, no `default_kind`, `agent` key rejected by `deny_unknown_fields`.

- [ ] **Step 3: Write minimal implementation**

In `src/config.rs`, replace the `command` field and add the agent table:

```rust
    /// What each pane runs, before there were two kinds of pane.
    ///
    /// Superseded by `[agent.claude] command`, and kept because
    /// `deny_unknown_fields` would otherwise greet everyone who has ever
    /// written a config file with an error dialog on their next update. It
    /// means claude's command, which is what it always meant - there was no
    /// other agent for it to mean.
    pub command: Option<String>,
    /// Which agent the `+` starts, and what a new project begins with.
    pub default_agent: String,
    /// Per-agent overrides, each defaulting to the binary the agent is named
    /// for.
    pub agent: AgentTable,
```

```rust
/// The `[agent.claude]` / `[agent.codex]` tables.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentTable {
    pub claude: AgentConfig,
    pub codex: AgentConfig,
}

/// What one agent's table can say. One key today; a struct rather than a bare
/// string so that gaining a second is an added field rather than a format
/// change in everybody's config file.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// Empty means "the binary this agent is named for".
    pub command: String,
}
```

In `Default for Config`: `command: None`, `default_agent: "claude".to_string()`, `agent: AgentTable::default()`.

Add to `impl Config`:

```rust
    /// The command line a pane of `kind` runs, before hooks are layered on.
    pub fn command_for(&self, kind: Kind) -> String {
        let configured = match kind {
            Kind::Claude => &self.agent.claude.command,
            Kind::Codex => &self.agent.codex.command,
        };
        if !configured.trim().is_empty() {
            return configured.clone();
        }
        // The deprecated top-level key, which only ever meant claude's.
        if kind == Kind::Claude
            && let Some(legacy) = self.command.as_deref()
            && !legacy.trim().is_empty()
        {
            return legacy.to_string();
        }
        kind.default_command().to_string()
    }

    /// Which agent the `+` starts. An unrecognised name falls back to claude,
    /// having been complained about at load time.
    pub fn default_kind(&self) -> Kind {
        Kind::parse(&self.default_agent).unwrap_or_default()
    }
}
```

In `Config::parse`, after a successful `toml::from_str`, collect complaints instead of returning `problem: None` immediately:

```rust
        match toml::from_str::<Config>(text) {
            Ok(config) => {
                let mut notes = Vec::new();
                if config.command.is_some() {
                    notes.push(format!(
                        "`command` is deprecated: write it as\n\n    [agent.claude]\n    \
                         command = \"…\"\n\nIt still works, and still means claude's \
                         command. To run codex instead, set `default_agent = \"codex\"` \
                         - `command = \"codex\"` never worked, because claude's own \
                         options were being appended to it."
                    ));
                }
                if Kind::parse(&config.default_agent).is_none() {
                    notes.push(format!(
                        "`default_agent = \"{}\"` isn't an agent this app knows \
                         ({}), so claude is in use.",
                        config.default_agent,
                        Kind::ALL
                            .map(|k| k.label())
                            .join(" or "),
                    ));
                }
                let problem = (!notes.is_empty())
                    .then(|| format!("{whence}:\n\n{}", notes.join("\n\n")));
                Loaded { config, problem }
            }
            Err(e) => { /* unchanged */ }
        }
```

Update the existing `a_config_round_trips` test's struct literal for the new fields (`command: None`, `default_agent: "claude".into()`, `agent: AgentTable::default()`).

Add `use crate::agent::Kind;` at the top of `config.rs`, and `use super::*;` already covers the tests.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib config::`
Expected: PASS — five new tests plus the five existing ones.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "Let the config name two agents without breaking the one it named"
```

---

### Task 4: The private codex home

**Files:**
- Create: `src/codex_home.rs`
- Modify: `src/main.rs` (add `mod codex_home;`)

**Interfaces:**
- Consumes: `hooks::codex_hooks_json`.
- Produces: `codex_home::build(real: &Path, into: &Path, hooks_json: &str) -> std::io::Result<()>` and `codex_home::prepare(hook_bin: &str, bell_hook: &str) -> Option<PathBuf>`.

Its own file rather than more of `pane.rs`: `pane.rs` is already 1156 lines, this is filesystem work with no GTK in it, and keeping it separate is what lets it be tested against a temp dir instead of a window.

**Behaviour:** mirror every entry of `real` into `into` as a symlink, prune links that no longer resolve, then write our `hooks.json` as a real file. If `real` has its own `hooks.json`, merge our event arrays into a copy of theirs rather than symlinking or shadowing it.

- [ ] **Step 1: Write the failing test**

Create the test module in `src/codex_home.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A scratch pair of directories, removed when the test ends.
    fn scratch(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("atc-codex-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let real = base.join("real");
        let into = base.join("into");
        fs::create_dir_all(&real).expect("scratch real");
        (real, into)
    }

    #[test]
    fn the_users_own_home_is_mirrored_and_never_written_to() {
        let (real, into) = scratch("mirror");
        fs::write(real.join("auth.json"), "{\"token\":\"secret\"}").unwrap();
        fs::create_dir(real.join("sessions")).unwrap();

        build(&real, &into, "{\"hooks\":{}}").expect("builds");

        assert_eq!(
            fs::read_to_string(into.join("auth.json")).unwrap(),
            "{\"token\":\"secret\"}",
            "the pane's codex must still be logged in",
        );
        assert!(
            fs::symlink_metadata(into.join("auth.json")).unwrap().is_symlink(),
            "mirrored as a link, so their credentials are never copied about",
        );
        assert!(into.join("sessions").exists(), "directories mirror too");
        assert!(
            !real.join("hooks.json").exists(),
            "the user's own home gained nothing",
        );
    }

    #[test]
    fn our_hooks_land_as_a_real_file() {
        let (real, into) = scratch("hooks");
        build(&real, &into, "{\"hooks\":{\"Stop\":[]}}").expect("builds");

        let written = into.join("hooks.json");
        assert!(!fs::symlink_metadata(&written).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&written).unwrap(), "{\"hooks\":{\"Stop\":[]}}");
    }

    /// Somebody who already uses codex hooks must not lose them by opening this
    /// app - they would have no reason to connect the two.
    #[test]
    fn an_existing_hooks_file_is_merged_rather_than_shadowed() {
        let (real, into) = scratch("merge");
        fs::write(
            real.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"theirs"}]}]}}"#,
        )
        .unwrap();

        build(
            &real,
            &into,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"ours"}]}],"Stop":[{"hooks":[{"type":"command","command":"ours"}]}]}}"#,
        )
        .expect("builds");

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(into.join("hooks.json")).unwrap()).unwrap();
        let pre = merged["hooks"]["PreToolUse"].as_array().expect("an array");
        let commands: Vec<&str> = pre
            .iter()
            .flat_map(|e| e["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|h| h["command"].as_str().map(str::to_string))
            .map(|s| Box::leak(s.into_boxed_str()) as &str)
            .collect();
        assert!(commands.contains(&"theirs"), "their hook survived: {commands:?}");
        assert!(commands.contains(&"ours"), "ours was added: {commands:?}");
        assert!(!merged["hooks"]["Stop"].is_null(), "and events they had none for");
    }

    /// Codex may never have been run. The app is not the right place to find
    /// that out, so it builds the home anyway and lets codex do its first run.
    #[test]
    fn a_home_that_does_not_exist_yet_is_not_an_error() {
        let (real, into) = scratch("absent");
        fs::remove_dir_all(&real).unwrap();

        build(&real, &into, "{\"hooks\":{}}").expect("an absent home is fine");
        assert!(into.join("hooks.json").exists());
    }

    /// A link to something the user has since deleted would be a broken entry
    /// in a directory codex is about to read, and it costs nothing to drop.
    #[test]
    fn links_to_things_that_have_gone_are_pruned() {
        let (real, into) = scratch("prune");
        fs::write(real.join("stale.json"), "x").unwrap();
        build(&real, &into, "{}").expect("builds");
        assert!(into.join("stale.json").exists());

        fs::remove_file(real.join("stale.json")).unwrap();
        build(&real, &into, "{}").expect("rebuilds");
        assert!(
            fs::symlink_metadata(into.join("stale.json")).is_err(),
            "a link to a file that has gone was left behind",
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib codex_home::`
Expected: FAIL — module/function does not exist.

- [ ] **Step 3: Write minimal implementation**

Create `src/codex_home.rs`:

```rust
//! A `CODEX_HOME` of our own, so codex's hooks can be installed without
//! touching the user's.
//!
//! Claude takes `--settings`, which layers a file over whatever the user has
//! and applies only to the process being launched. Codex has no equivalent:
//! hooks are read from its home directory or from the repo, and from nowhere
//! else. Writing to `~/.codex` would mean every codex the user runs, in every
//! terminal, calling this app for the rest of time - which is exactly the thing
//! the claude side goes to some trouble not to do.
//!
//! What codex does honour is `CODEX_HOME`. So this builds one: a cache
//! directory holding a symlink to every entry of the real home - auth, config,
//! sessions, history - and one real file of our own beside them. Codex reads
//! the user's actual settings through the links and our hooks from the file,
//! and `~/.codex` is never opened for writing.
//!
//! Rebuilt on every launch rather than when absent, for the reason the claude
//! settings file is: a hook left behind by an older build must not outlive the
//! version that wrote it.

use std::path::{Path, PathBuf};

/// Builds the private home at `into`, mirroring `real` and installing
/// `hooks_json`.
pub fn build(real: &Path, into: &Path, hooks_json: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(into)?;

    // Prune first, so a link whose target the user has deleted is gone before
    // codex reads the directory - and so a home that used to have a file and
    // now doesn't stops advertising it.
    for entry in std::fs::read_dir(into)? {
        let entry = entry?;
        let path = entry.path();
        let is_link = std::fs::symlink_metadata(&path).map(|m| m.is_symlink()).unwrap_or(false);
        if is_link && !path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }

    // `hooks.json` is ours, and is handled below - mirroring theirs would
    // shadow ours or ours theirs, and both are the wrong answer.
    if let Ok(entries) = std::fs::read_dir(real) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == std::ffi::OsStr::new(HOOKS_FILE) {
                continue;
            }
            let link = into.join(&name);
            if std::fs::symlink_metadata(&link).is_ok() {
                continue;
            }
            let _ = std::os::unix::fs::symlink(entry.path(), link);
        }
    }

    // Their hooks are a thing they set up on purpose, for reasons that have
    // nothing to do with this app. Ours are added to theirs.
    let merged = match std::fs::read_to_string(real.join(HOOKS_FILE)) {
        Ok(theirs) => merge(&theirs, hooks_json),
        Err(_) => hooks_json.to_string(),
    };
    std::fs::write(into.join(HOOKS_FILE), merged)
}

const HOOKS_FILE: &str = "hooks.json";

/// Adds our event entries to theirs, keeping both.
///
/// Anything unparseable on their side means ours alone: a codex that cannot
/// read their file was not going to run their hooks either, and losing our dots
/// as well helps nobody.
fn merge(theirs: &str, ours: &str) -> String {
    let (Ok(mut theirs), Ok(ours)) = (
        serde_json::from_str::<serde_json::Value>(theirs),
        serde_json::from_str::<serde_json::Value>(ours),
    ) else {
        return ours.to_string();
    };

    let Some(our_events) = ours["hooks"].as_object() else {
        return ours.to_string();
    };
    if !theirs["hooks"].is_object() {
        theirs["hooks"] = serde_json::json!({});
    }
    let their_events = theirs["hooks"].as_object_mut().expect("just ensured");
    for (event, entries) in our_events {
        let slot = their_events
            .entry(event.clone())
            .or_insert_with(|| serde_json::json!([]));
        if let (Some(slot), Some(entries)) = (slot.as_array_mut(), entries.as_array()) {
            slot.extend(entries.iter().cloned());
        } else {
            *slot = entries.clone();
        }
    }
    theirs.to_string()
}

/// The private home for this run, or `None` if it could not be built - in
/// which case the pane launches a plain `codex`, exactly as a claude pane
/// launches a plain `claude` when its settings file cannot be written.
pub fn prepare(hook_bin: &str, bell_hook: &str) -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?
        .join("agenttilecli")
        .join("codex-home");

    let real = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))?;

    build(&real, &cache, &crate::hooks::codex_hooks_json(hook_bin, bell_hook)).ok()?;
    Some(cache)
}
```

Add `mod codex_home;` to `src/main.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib codex_home::`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/codex_home.rs src/main.rs
git commit -m "Build codex a home of our own, so its own stays untouched"
```

---

### Task 5: Panes know which agent they run

**Files:**
- Modify: `src/pane.rs` (`Pane::new`, `Head`, `Head::refresh`, `Pane::spawn`)

**Interfaces:**
- Consumes: `agent::Kind`, `config::Config::command_for`, `codex_home::prepare`, existing `claude_settings_file`.
- Produces: `Pane::new(cwd: &str, kind: Kind) -> Pane`, `Pane::kind(&self) -> Option<Kind>`.

`Pane::command` (the update pane) is unchanged and keeps producing a pane with no kind.

- [ ] **Step 1: Write the failing test**

Add to `src/pane.rs`'s test module (create one if absent, `#[cfg(test)] mod tests`):

```rust
/// The head strip is the only place a mixed project says which tile is which,
/// and it has to say it without losing what it already said.
#[test]
fn a_head_strip_names_its_agent_alongside_its_state() {
    assert_eq!(
        head_text(Some("working"), Kind::Codex),
        "working \u{b7} codex",
    );
    assert_eq!(
        head_text(Some("agenttilecli"), Kind::Claude),
        "agenttilecli \u{b7} claude",
    );
}

/// A pane with no agent - the update script's - has nothing to name, and a
/// trailing separator would be a promise of a word that never comes.
#[test]
fn a_pane_with_no_agent_says_only_what_it_did_before() {
    assert_eq!(head_text_for(Some("building"), None), "building");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib pane::`
Expected: FAIL — `head_text` / `head_text_for` undefined.

- [ ] **Step 3: Write minimal implementation**

Extract the text decision out of `Head::refresh` so it can be tested without a `gtk4::Label`:

```rust
/// The strip's text, given what it would have said and which agent is running.
///
/// Split out of `refresh` because it is the part with a decision in it, and a
/// `gtk4::Label` is not something a unit test should need to own.
///
/// The agent's name rides on the end rather than replacing anything: what the
/// strip already said - the state, or the folder when the agent has wandered
/// out of it - is the more urgent fact, and stays where the eye already looks
/// for it. It is only worth saying at all because a project can now hold two
/// kinds of tile, and two identical strips over two different agents is the
/// one thing this feature must not produce.
fn head_text_for(base: Option<&str>, kind: Option<Kind>) -> String {
    match (base, kind) {
        (Some(base), Some(kind)) => format!("{base} \u{b7} {}", kind.label()),
        (Some(base), None) => base.to_string(),
        (None, Some(kind)) => kind.label().to_string(),
        (None, None) => String::new(),
    }
}

#[cfg(test)]
fn head_text(base: Option<&str>, kind: Kind) -> String {
    head_text_for(base, Some(kind))
}
```

Add `kind: Option<Kind>` to `struct Head`, set it in `Pane::spawn`, and route `refresh` through the helper:

```rust
    fn refresh(&self) {
        let base = match self.cwd.borrow().as_deref() {
            Some(cwd) if cwd != self.root.borrow().as_str() => cwd.to_string(),
            _ if self.reports => status_words(&self.state.borrow()),
            _ => self.root.borrow().clone(),
        };
        self.label.set_label(&head_text_for(Some(&base), self.kind));
    }
```

Rewrite `Pane::new` to take a kind and build the right launch:

```rust
    /// The usual pane: an agent of `kind`, running in `cwd`, with this app's
    /// hooks installed so its dot reports.
    ///
    /// How the hooks get there differs by agent and the difference is the whole
    /// of `agent`'s reason to exist: claude takes a `--settings` file, codex
    /// takes a `CODEX_HOME` we build for it. Both are best-effort in the same
    /// way - if the hooks cannot be installed the pane still gets a perfectly
    /// good agent, just a silent one, which is what every pane was before any
    /// of this existed.
    pub fn new(cwd: &str, kind: Kind) -> Self {
        let configured = crate::config::get().command_for(kind);
        let (command, env) = match kind {
            Kind::Claude => match claude_settings_file() {
                Some(path) => (
                    format!("{configured} --settings {}", crate::update::sh_quote(&path)),
                    Vec::new(),
                ),
                None => (configured, Vec::new()),
            },
            Kind::Codex => {
                // In the environment rather than prefixed onto the command
                // line: it is per-pane state, it belongs beside the per-pane
                // state already going through VTE, and it dodges a quoting
                // layer that has bitten this file before.
                let env = crate::update::exe()
                    .ok()
                    .and_then(|bin| crate::codex_home::prepare(&bin, BELL_HOOK))
                    .map(|home| vec![format!("CODEX_HOME={}", home.display())])
                    .unwrap_or_default();
                (configured, env)
            }
        };
        Self::spawn_with(cwd, &command, true, Some(kind), env)
    }
```

Rename the existing private `spawn` to `spawn_with`, giving it `kind: Option<Kind>` and `extra_env: Vec<String>` parameters; have `Pane::command` call `Self::spawn_with(cwd, command, false, None, Vec::new())`. Inside, extend the existing env vector:

```rust
        let mut env = Vec::new();
        if let (Some(socket), Ok(bin)) = (crate::ipc::socket_path(), crate::update::exe()) {
            env.push(format!("{}={id}", crate::ipc::ENV_PANE));
            env.push(format!("{}={socket}", crate::ipc::ENV_SOCKET));
            env.push(format!("{}={bin}", crate::ipc::ENV_BIN));
        }
        env.extend(extra_env);
```

Store the kind on `Pane` and expose it:

```rust
    /// Which agent this pane runs, or `None` for a pane that runs a command
    /// rather than an agent.
    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib pane::` then `cargo build`
Expected: tests PASS; build FAILS at `Pane::new` call sites, which Task 6 fixes.

- [ ] **Step 5: Commit**

```bash
git add src/pane.rs
git commit -m "Give a pane an agent, and a strip that names it"
```

---

### Task 6: Spawning, the split button, and the palette

**Files:**
- Modify: `src/tiler/panes.rs` (`spawn_pane_here`, `spawn_pane_in`)
- Modify: `src/app/header.rs:307-320` (the `new_agent` button)
- Modify: `src/keybindings.rs:229` (the spawn action)
- Modify: `src/app/mod.rs:684-686` (restore loop), `src/app/projects.rs` (new-project spawning)

**Interfaces:**
- Consumes: `Pane::new(cwd, kind)`, `Config::default_kind`.
- Produces: `Tiler::spawn_pane_here(&self)` (unchanged signature — spawns the group's default), `Tiler::spawn_pane_of(&self, kind: Kind)`, `Tiler::default_kind(&self) -> Kind`, `Tiler::set_default_kind(&self, kind: Kind)`.

Keeping `spawn_pane_here`'s signature is what lets `keybindings.rs:229`'s `Action::Tiler(Tiler::spawn_pane_here)` function pointer keep compiling untouched.

- [ ] **Step 1: Write the failing test**

Add to `src/tiler/panes.rs` or the tiler's test module:

```rust
/// A group remembers the kind you last chose in it, so a project you run codex
/// in keeps starting codexes without being told twice - the same habit the
/// agent *count* is already learned from.
#[test]
fn a_group_remembers_the_kind_it_was_last_given() {
    let group = Tiler::new_for_test("/tmp");
    assert_eq!(group.default_kind(), crate::config::get().default_kind());

    group.set_default_kind(Kind::Codex);
    assert_eq!(group.default_kind(), Kind::Codex);
}
```

If the tiler has no test constructor, assert on the stored `Cell<Kind>` through a smaller unit instead: add the field, and test `Config::default_kind()` seeding it in `config`'s tests rather than building a GTK widget. Do not construct GTK types in a unit test.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib tiler::`
Expected: FAIL — no `default_kind` / `set_default_kind`.

- [ ] **Step 3: Write minimal implementation**

In the tiler's `imp`, add `default_kind: Cell<Kind>`, initialised from `crate::config::get().default_kind()`. Then:

```rust
    /// The kind the bare `+` starts in this group.
    pub fn default_kind(&self) -> Kind {
        self.imp().default_kind.get()
    }

    pub fn set_default_kind(&self, kind: Kind) {
        self.imp().default_kind.set(kind);
    }

    /// Spawns this group's default agent - the `+`, and the keybinding.
    pub fn spawn_pane_here(&self) {
        self.spawn_pane_of(self.default_kind());
    }

    /// Spawns an agent of `kind`, and remembers it: choosing codex from the
    /// menu once is a statement about this project, not about this click.
    pub fn spawn_pane_of(&self, kind: Kind) {
        self.set_default_kind(kind);
        let cwd = self.imp().cwd.borrow().clone();
        self.attach_process_pane(Pane::new(&cwd, kind));
    }
```

Update `src/app/mod.rs:684-686`'s restore loop to spawn per remembered kind (Task 7 supplies the list; until then use `tiler.default_kind()`).

In `src/app/header.rs`, replace the lone `new_agent` button with a split:

```rust
        // A split rather than a menu: the `+` is the action this window exists
        // for and it keeps its one click. The arrow is for the other answer,
        // which is the rarer one by construction - a project tends to be a
        // project you run one agent in.
        let new_agent = gtk4::Button::builder()
            .icon_name("tab-new-symbolic")
            .can_focus(false)
            .valign(gtk4::Align::Center)
            .css_classes(["header-action", "header-primary"])
            .tooltip_text("Spawn a new agent in this project")
            .build();
        let this = self.clone();
        new_agent.connect_clicked(move |_| {
            if let Some(tiler) = this.active_tiler() {
                tiler.spawn_pane_here();
            }
        });

        let menu = gio::Menu::new();
        for kind in Kind::ALL {
            menu.append(
                Some(&format!("Spawn {}", kind.label())),
                Some(&format!("win.spawn-{}", kind.label())),
            );
        }
        let choose_agent = gtk4::MenuButton::builder()
            .css_classes(["header-action", "header-split-arrow"])
            .valign(gtk4::Align::Center)
            .can_focus(false)
            .menu_model(&menu)
            .tooltip_text("Choose which agent to spawn")
            .build();

        let split = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .css_classes(["header-split"])
            .build();
        split.append(&new_agent);
        split.append(&choose_agent);
```

Pack `split` where `new_agent` was packed. Register one `win.spawn-<label>` action per kind on the window, each calling `tiler.spawn_pane_of(kind)`. Add a `.header-split` / `.header-split-arrow` rule to `src/style.css` matching the existing `.header-action` treatment — a shared border radius across the pair, the arrow narrower and without the primary filament outline.

Add the two palette entries alongside `keybindings.rs:229`'s existing spawn entry, each running the matching kind.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test && cargo build`
Expected: PASS and a clean build.

Then, visually: `scripts/dev-run.sh`, click `▾`, spawn one of each in a project, confirm two strips reading `… · claude` and `… · codex`.

- [ ] **Step 5: Commit**

```bash
git add src/tiler/panes.rs src/app/header.rs src/app/mod.rs src/app/projects.rs src/keybindings.rs src/style.css
git commit -m "Offer the second agent where the first is offered"
```

---

### Task 7: Sessions remember which agent

**Files:**
- Modify: `src/session.rs` (`Project`), `src/app/mod.rs:684-686` and `:721-726` (save and restore)

**Interfaces:**
- Consumes: `agent::Kind`, `Pane::kind`.
- Produces: `session::Project::agent_kinds: Vec<String>`.

`agents: usize` stays as it is — it feeds `last_agent_count`, which is about *how many*, and a count is still the right shape for that. The kinds ride alongside.

- [ ] **Step 1: Write the failing test**

Add to `src/session.rs`'s tests:

```rust
/// Session files written before this existed must restore exactly as they did.
#[test]
fn a_session_from_before_agents_had_kinds_still_reads() {
    let old = r#"{"projects":[{"path":"/tmp","name":"tmp","agents":2}]}"#;
    let session: Session = serde_json::from_str(old).expect("an old session still parses");
    let project = &session.projects[0];
    assert_eq!(project.agents, 2);
    assert!(project.agent_kinds.is_empty(), "and claims no kinds");
    assert_eq!(
        project.kinds(),
        vec![Kind::Claude, Kind::Claude],
        "which means claude, because there was nothing else it could have been",
    );
}

#[test]
fn a_mixed_project_remembers_which_was_which() {
    let project = Project {
        agents: 2,
        agent_kinds: vec!["codex".into(), "claude".into()],
        ..Project::default()
    };
    assert_eq!(project.kinds(), vec![Kind::Codex, Kind::Claude]);
}

/// A hand-mangled or half-written state file is not the user's mistake, so it
/// is quietly ignored rather than reported - the rule this module's header
/// sets out.
#[test]
fn kinds_that_disagree_with_the_count_fall_back_to_the_count() {
    let project = Project {
        agents: 3,
        agent_kinds: vec!["codex".into()],
        ..Project::default()
    };
    assert_eq!(project.kinds().len(), 3, "the count is the fact that matters");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib session::`
Expected: FAIL — no `agent_kinds`, no `kinds`.

- [ ] **Step 3: Write minimal implementation**

Add to `session::Project`:

```rust
    /// Which agent each of those panes was running, in pane order.
    ///
    /// Absent in any file written before there were two agents, which is why
    /// this is read through `kinds` and never directly: an empty list means
    /// claude, because claude is what it could have been.
    pub agent_kinds: Vec<String>,
```

with `agent_kinds: Vec::new()` in `Default for Project`, and:

```rust
impl Project {
    /// The agents to restore, one per pane.
    ///
    /// `agents` is the fact that decides how many; the kinds only decide which.
    /// A list that disagrees with the count is a state file that has been
    /// edited or half-written, and this module quietly tolerates that rather
    /// than reporting it - see the header.
    pub fn kinds(&self) -> Vec<Kind> {
        (0..self.agents)
            .map(|i| {
                self.agent_kinds
                    .get(i)
                    .and_then(|k| Kind::parse(k))
                    .unwrap_or_default()
            })
            .collect()
    }
}
```

In `src/app/mod.rs`, replace the restore loop's `for _ in 0..project.agents` with `for kind in project.kinds()` calling `tiler.spawn_pane_of(kind)`, and populate `agent_kinds` at save time from each pane's `kind()`, skipping panes with none (the editor, the update pane) so the list stays parallel to the agent count.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib session::`
Expected: PASS (3 new tests).

- [ ] **Step 5: Commit**

```bash
git add src/session.rs src/app/mod.rs
git commit -m "Remember which agent each pane was, for the people who restore them"
```

---

### Task 8: Documentation

**Files:**
- Modify: `README.md` (the features list and the `config.toml` block at :185)

**Interfaces:** none.

- [ ] **Step 1: Update the config block**

Replace the `command = "claude"` line in the README's config example with the new shape, keeping the existing comment style:

```toml
default_agent = "claude"  # which agent the + starts

[agent.claude]
command = "claude"        # what a claude pane runs

[agent.codex]
command = "codex"         # what a codex pane runs
```

- [ ] **Step 2: Add the feature paragraph**

Add to the features list, in the voice the rest of the list uses — concrete about what the user sees, honest about the seam:

- both agents in one project, the `▾` beside the `+`
- the head strip naming its agent
- `~/.codex` never written to, and *why* (a private `CODEX_HOME` of symlinks)
- the one note owed: a user with inline `[hooks]` in their `config.toml` will see codex warn that it loaded both that and our `hooks.json`

- [ ] **Step 3: Note the untested seam**

Add a line to the spec's "seam that cannot be tested here" section recording whether it has since been verified against a real codex, and by whom.

- [ ] **Step 4: Verify**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all clean.

- [ ] **Step 5: Commit**

```bash
git add README.md docs/superpowers/specs/2026-08-17-codex-support-design.md
git commit -m "Say that there are two agents now, and what that costs"
```

---

## Self-Review

**Spec coverage:** `agent.rs` → Task 1. Event mapping and codex payload → Task 2. Config schema and back-compat → Task 3. Private `CODEX_HOME`, merge, three failure cases → Task 4. `Pane::new(cwd, kind)`, `CODEX_HOME` in the VTE env, head strip → Task 5. Split button, palette, group default → Task 6. Session persistence → Task 7. README and the untested-seam note → Task 8. No spec section is unclaimed.

**Deviation from the spec, recorded deliberately:** the spec says the head strip renders `folder · agent`. `Head::refresh` actually shows *status words* and only falls back to the folder when the agent has left the project directory, so Task 5 appends the agent to whichever of the three texts `refresh` chooses. The intent (a pane names its agent) is met; the literal rendering in the spec is not, and the spec's mockup was drawn before that code was read.

**Deviation two:** the spec implies `Event::name` gains a per-kind form. It does not — only the JSON *key* differs (`Event::codex_key`), because the `--hook` argv is written and parsed by this app. Smaller change, same effect.

**Type consistency:** `Kind` is `crate::agent::Kind` throughout. `command_for(Kind) -> String` and `default_kind() -> Kind` are used as defined in Tasks 5–7. `codex_home::build(&Path, &Path, &str)` and `prepare(&str, &str) -> Option<PathBuf>` match their Task 5 call site. `Pane::new(&str, Kind)`, `Pane::kind(&self) -> Option<Kind>`, `Tiler::spawn_pane_of(Kind)` and `Project::kinds() -> Vec<Kind>` are consistent across Tasks 5, 6 and 7.

**Known risk, carried by design:** Task 2's `"hooks"` wrapper in the standalone `hooks.json` and Task 4's whole launch path are documentation-derived. Nothing in this plan verifies them against a running codex, and Task 8 Step 3 exists so that stays visible rather than becoming folklore.
