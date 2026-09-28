#!/usr/bin/env bash
# Run a dev AgentTileCLI alongside an installed one, touching none of its state.
#
# The installed app is somebody's live working environment - closing it hangs up
# every agent in every group - so a dev build has to coexist with it rather than
# replace it. Two things keep them apart, and only the first is handled by the
# app itself:
#
#   1. The GTK application id. `app_id()` in src/main.rs suffixes it on any
#      branch that isn't master, so a dev build off a branch opens its own
#      window. On master the ids are identical and GApplication's
#      single-instance handshake means `cargo run` just wakes the running
#      window over D-Bus - which looks, confusingly, like a build that did
#      nothing. Hence the refusal below.
#
#   2. The XDG directories, which the app does not isolate, and which is the
#      reason this script exists at all:
#
#      - $XDG_CACHE_HOME/agenttilecli/claude-settings.json is a single shared
#        file, rewritten on every pane launch, carrying the path of the binary
#        that wrote it. A dev build sharing it repoints the live app's
#        next-spawned panes at target/debug, and the next `cargo build` then
#        strands those panes on "starting..." forever with nothing anywhere
#        saying why.
#      - $XDG_CACHE_HOME/agenttilecli/codex-home/ is the private CODEX_HOME the
#        codex panes launch against, and has the same problem.
#      - $XDG_STATE_HOME/agenttilecli/session.json is the live app's project
#        list, overwritten when an instance exits.
#
#   3. The working directory, which is this app's *first project* - and
#      therefore the directory its agents get spawned into.
#
#      Left alone, that is the checkout: the script has to cd to the repo root
#      to build, `cargo run` hands the binary the cwd it inherited, and
#      `build_window` opens its first project there. So a dev build launched to
#      test a change came up holding a claude with write access to the working
#      tree that change was being written in - a second agent editing the branch
#      under the person testing it. It happened on 2026-08-23, and it is the
#      kind of thing that is obvious only afterwards.
#
#      So the build and the run are separated: --manifest-path points cargo at
#      the repo (keeping one shared target/ and incremental builds), while the
#      process cwd is a scratch project inside the sandbox. Agents spawned in a
#      dev window work there and cannot reach the checkout.
#
# HOME is deliberately left alone: ~/.claude and ~/.codex hold the agents' own
# credentials, and a dev instance that cannot log its agents in cannot be used
# to test agents. It is also where Omarchy keeps the current theme
# ($HOME/.local/state/omarchy/current, hardcoded there rather than under
# XDG_STATE_HOME - see src/omarchy.rs), so a dev build follows the real
# desktop's theme despite every XDG_* variable above being redirected. That is
# wanted: testing theming against a state directory with no theme in it tests
# nothing.

set -euo pipefail

repo="$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.."
repo="$(cd "$repo" && pwd)"
cd "$repo"

branch="$(git rev-parse --abbrev-ref HEAD)"
if [ "$branch" = "master" ]; then
    cat >&2 <<'EOF'
On master, a dev build shares the installed app's application id: `cargo run`
would wake the running window rather than open its own. Switch to a branch.
EOF
    exit 1
fi

# Deliberately not under /tmp. Codex refuses to create its PATH helper binaries
# when CODEX_HOME sits in a temporary directory ("Refusing to create helper
# binaries under temporary dir"), and the private CODEX_HOME this app builds
# lives under XDG_CACHE_HOME - so a sandbox in /tmp would make codex panes
# behave differently under test than they ever do in production, which is the
# one thing a test environment must not do.
sandbox="$HOME/.cache/agenttilecli-dev/$branch"
export XDG_CONFIG_HOME="$sandbox/config"
export XDG_CACHE_HOME="$sandbox/cache"
export XDG_STATE_HOME="$sandbox/state"
export XDG_DATA_HOME="$sandbox/data"
mkdir -p "$XDG_CONFIG_HOME/agenttilecli" "$XDG_CACHE_HOME" "$XDG_STATE_HOME" "$XDG_DATA_HOME"

# Seeded once, so the dev instance behaves like the real one rather than like a
# fresh install. Copied rather than symlinked, because the preferences dialog
# writes this file and a symlink would carry those edits back into the live
# app's config.
real_config="${XDG_CONFIG_HOME_REAL:-$HOME/.config}/agenttilecli/config.toml"
if [ -f "$real_config" ] && [ ! -f "$XDG_CONFIG_HOME/agenttilecli/config.toml" ]; then
    cp "$real_config" "$XDG_CONFIG_HOME/agenttilecli/config.toml"
fi

# The first project a dev window opens, and so the directory its agents run in.
# Seeded with a note rather than left bare, because an empty folder in a cache
# directory is a thing someone finds later and cannot explain.
project="$sandbox/project"
mkdir -p "$project"
if [ ! -f "$project/README.md" ]; then
    cat > "$project/README.md" <<'NOTE'
Scratch project for the AgentTileCLI dev build.

A dev window opens this folder as its first project, so any agent started in one
works here. That is deliberate: the dev build shares $HOME with the live one so
its agents can log in, and a dev project pointed at the checkout would put a
second agent inside the working tree the branch is being written in.

Safe to delete. scripts/dev-run.sh recreates it.
NOTE
fi

printf 'branch    %s\n' "$branch"
printf 'app id    dev.agenttilecli.AgentTileCli.%s\n' "${branch//[^a-zA-Z0-9]/-}"
printf 'sandbox   %s\n' "$sandbox"
printf 'project   %s\n' "$project"
printf 'live app  %s (untouched)\n\n' "$(pgrep -x agenttilecli >/dev/null && echo "PID $(pgrep -x agenttilecli | tr '\n' ' ')" || echo 'not running')"

# Run from the scratch project, build from the repo. `cargo run` hands the
# binary whatever cwd it inherits, so this is what keeps agents out of the
# checkout; --manifest-path is what still lets cargo find the crate.
#
# Every binary first, not just the window: `cargo run` builds only the one it
# runs, and the window looks for `agenttilecli-hook` beside itself. Without this
# a dev window's agents would report through a stale hook binary, or through the
# slow fallback, and neither is what a change to the hook is being tested with.
cd "$project"
cargo build --manifest-path "$repo/Cargo.toml" --bins
exec cargo run --manifest-path "$repo/Cargo.toml" "$@"
