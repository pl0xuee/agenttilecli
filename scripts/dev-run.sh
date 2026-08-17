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
# HOME is deliberately left alone: ~/.claude and ~/.codex hold the agents' own
# credentials, and a dev instance that cannot log its agents in cannot be used
# to test agents.

set -euo pipefail

cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.."

branch="$(git rev-parse --abbrev-ref HEAD)"
if [ "$branch" = "master" ]; then
    cat >&2 <<'EOF'
On master, a dev build shares the installed app's application id: `cargo run`
would wake the running window rather than open its own. Switch to a branch.
EOF
    exit 1
fi

sandbox="${TMPDIR:-/tmp}/agenttilecli-dev/$branch"
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

printf 'branch    %s\n' "$branch"
printf 'app id    dev.agenttilecli.AgentTileCli.%s\n' "${branch//[^a-zA-Z0-9]/-}"
printf 'sandbox   %s\n' "$sandbox"
printf 'live app  %s (untouched)\n\n' "$(pgrep -x agenttilecli >/dev/null && echo "PID $(pgrep -x agenttilecli | tr '\n' ' ')" || echo 'not running')"

exec cargo run "$@"
