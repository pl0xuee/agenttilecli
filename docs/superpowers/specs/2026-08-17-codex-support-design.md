# Codex support

AgentTileCLI runs one agent: `claude`. This adds a second, `codex`, as a peer
rather than as a configuration accident — pickable per pane, mixable inside one
project, and reporting its state into the same dots every claude pane already
lights.

## Why this is more than a config change

`config.command` already exists, so setting `command = "codex"` looks like it
should work today. It does not, and fails in the least helpful way available:
`Pane::new` (`src/pane.rs`) unconditionally appends `--settings <path>` to
whatever the command is, codex rejects the unknown flag, and the pane dies at
startup with the reason scrolled off inside a terminal nobody reads.

Beyond that, the status dots are claude-shaped end to end. `hooks::Event` *is*
claude's six hook names, and the settings writer, the `--hook` argv path in
`main.rs`, and the socket protocol in `ipc.rs` all speak that vocabulary.

## Two findings that make this cheap

**Pane identity travels in the environment, not the settings file.**
`Pane::spawn` injects `ATC_PANE_ID`, `ATC_SOCKET` and `ATC_BIN` per pane through
VTE, while `claude-settings.json` is one shared file. Codex hooks run with the
session's ordinary process environment, so those three variables reach a codex
hook exactly as they reach a claude one. Codex therefore needs one shared
`hooks.json`, not a file per pane, and no new IPC of any kind.

**The event vocabularies almost match.** Codex's hooks engine fires
`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse` and `Stop` —
identical strings to claude's — and puts the tool name in a `tool_name` field on
stdin, which is the field `report_hook` already reads. The single divergence is
claude's `Notification` against codex's `PermissionRequest`, and since
`hooks::advance` already reads that event as "blocked on you", the state machine
is untouched.

Net effect: `src/main.rs` and `src/ipc.rs` need no changes at all.

## Decisions

- **A closed set of two agents.** Claude and codex, each with compiled-in launch
  flags and status wiring. A third agent is a code change, deliberately.
- **Mixing within a project is allowed.** A claude and a codex can tile side by
  side in one folder.
- **Hooks install via a private `CODEX_HOME`.** See below.
- **A pane names its agent in the head strip**, as `folder · agent`.

## Components

### `src/agent.rs` (new)

`Kind { Claude, Codex }`, free of GTK so the mapping is unit-testable the way
`hooks.rs` is. It answers four questions: display label, default command, how to
install its hooks, and what it calls each `hooks::Event` on the wire.

An enum rather than a trait with two impls: the set is closed by decision, and a
trait would scatter two implementations across vtables to buy extensibility
that has been explicitly deferred.

### `src/config.rs`

```toml
default_agent = "claude"

[agent.claude]
command = "claude"

[agent.codex]
command = "codex"
```

`deny_unknown_fields` is on, so removing top-level `command` would turn every
existing user's config into a startup error dialog. It keeps parsing, meaning
*the claude agent's command*, and reports itself once as deprecated through the
existing `problem()` channel.

The one lossy case: someone who already wrote `command = "codex"` — broken today
for the reason above — is mapped to claude and has to move the line. The
deprecation text says so explicitly rather than leaving them to find out.

### Installing codex's hooks

Codex hooks cannot be set per-session or on the command line; they load only
from `~/.codex/hooks.json`, `~/.codex/config.toml`, or `<repo>/.codex/`. Claude's
`--settings` is what lets this app keep its standing promise that nothing in the
agent's own home is written to and the user's agent in any other terminal is
unaffected. Codex offers no equivalent flag, but it does honour `CODEX_HOME`.

So: a per-app cache directory that symlinks every entry of the real
`$CODEX_HOME` (default `~/.codex`) — auth, config, sessions, history — and holds
one real file of our own, `hooks.json`. Panes launch with `CODEX_HOME` pointing
at it. The user's `~/.codex` is never written to.

- `hooks::codex_hooks_json()` walks the same `Event::ALL` loop `settings_json()`
  does, emitting codex's `hooks.json` shape with `PermissionRequest` in
  `Notification`'s slot.
- Rebuilt on every launch and pruned of stale links, for the reason the existing
  comment in `pane.rs` gives for rewriting the claude settings each time: a
  stale hook left by an older build must not outlive the version that wrote it.
- **The user already has `~/.codex/hooks.json`**: don't symlink it — read it and
  merge our six event arrays into a copy.
- **`~/.codex` doesn't exist**: create the directory with only our `hooks.json`
  and let codex do its own first-run thing.
- **Anything fails**: launch a plain `codex`. The dot stays hollow and the bell
  fallback carries, which is the identical contract `Pane::new` already promises
  for claude.

`CODEX_HOME` goes in the VTE environment vector beside `ATC_PANE_ID` rather than
as a prefix on the shell command line: it is per-pane state, it belongs where the
other per-pane state is, and it avoids a quoting layer.

A user with inline `[hooks]` in their `config.toml` will see codex warn that it
loaded both representations. That earns a README note, not code.

### Pane and header

- `Pane::new(cwd, kind)`; `Head` carries the agent name and renders
  `folder · agent` through the existing ellipsizing label.
- `spawn_pane_here` takes a kind. Each group remembers a default, seeded from
  `default_agent` and thereafter inherited from the last project worked in — the
  same habit-learning the agent *count* already does.
- The header `+` spawns the group default; a `▾` beside it offers both. The
  command palette gains "Spawn a claude agent" / "Spawn a codex agent".

### `src/session.rs`

A per-pane `agent` field, absent meaning claude, so existing session files
restore exactly as they do now. Only bites when `restore_agents` is on.

## Testing

`agent`, `hooks` and `config` are GTK-free, so the load-bearing parts are
testable without a window:

- event names round-trip through `Event::parse` for each kind, including the
  `Notification` / `PermissionRequest` divergence
- `codex_hooks_json` parses back as JSON with all six events pointing at the
  right binary
- config back-compat (a bare `command` becomes claude's command *and* raises a
  deprecation problem) and a round-trip with both agent tables
- the private-home builder against a temp dir, in its three cases: real
  `~/.codex` absent, present, and present with its own `hooks.json`

### The seam that cannot be tested here

`codex` is not installed on this machine. Everything above is built from the
published hook documentation, not from a running binary, so the point where a
real codex reads our `hooks.json` out of a redirected `CODEX_HOME` is unverified
by construction. It must be labelled as such until someone runs it against a
real codex.

### Running a dev build

The installed AgentTileCLI is the user's live working environment and must never
be closed to test a change. Two things keep a dev instance clear of it, and only
the first is already handled:

1. `app_id()` suffixes the GApplication id on any non-`master` branch, so a dev
   build off a branch opens its own window instead of waking the live one.
2. The XDG directories are *not* isolated by the app.
   `~/.cache/agenttilecli/claude-settings.json` is one shared file, rewritten on
   every pane launch, carrying the path of the binary that wrote it — a dev
   build sharing it repoints the live app's next-spawned panes at
   `target/debug`, and a later rebuild strands them on "starting…" forever.
   `session.json` is the live app's project list and is overwritten on exit.

`scripts/dev-run.sh` does both, and leaves `HOME` alone so the agents' own auth
still resolves. Because the codex home lives under `$XDG_CACHE_HOME`, the same
sandbox isolates the symlink farm.

## Out of scope

Per-agent colours; the codex `notify` path (superseded by its hooks engine);
subagent and compact events; any third agent; per-agent keybinding config.
