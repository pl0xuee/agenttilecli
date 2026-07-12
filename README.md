# AgentTileCLI

A native Linux dynamic tiling window manager for AI CLI sessions. Panes are
real terminals (VTE) that auto re-tile as you spawn, close, or promote them —
no manual resizing, though you can also drag any divider with the mouse if
you want to nudge it.

## Features

- **Grid mode by default** — every pane gets an equal-size cell; the grid
  shape (rows/columns) recomputes automatically as you open/close panes.
- **dwm-style master-stack mode** — one larger master pane + a stack column,
  with a persistent adjustable ratio.
- **Monocle mode** — fullscreen the focused pane.
- **Mouse support** — click any pane to focus it, drag any seam between
  panes to resize, click the ✕ in a pane's corner to close it, or click the
  floating **+** button to spawn a new one.
- **Built-in help pane** — a static cheatsheet of every keybinding, toggle it
  any time with `Super+Alt+/`.

## Keybindings

All bindings are held with **Super+Alt** together, so they never collide with
your desktop environment's own `Super+key` shortcuts.

| Keys | Action |
|---|---|
| `Return` | spawn a new pane |
| `Shift+Return` | promote focused pane to master (zoom) |
| `j` / `k` | focus next / previous pane |
| `w` | close the focused pane |
| `h` / `l` | shrink / grow the master column (MasterStack mode) |
| `i` / `d` | more / fewer master panes (MasterStack mode) |
| `m` | toggle monocle (focused pane fullscreen) |
| `Tab` | cycle layout mode: grid → master-stack → monocle |
| `/` | toggle the help pane |

## Requirements

- Linux with GTK4 and VTE4 installed (the GTK4-flavored VTE terminal widget,
  package name is usually `vte4` or `vte3-gtk4` depending on distro).
- [Rust](https://www.rust-lang.org/tools/install) (via `rustup` or your
  distro's package manager) to build from source.
- By default, each pane runs the `claude` CLI in your login shell. If you
  don't have it installed, panes will just show your shell's "command not
  found" and exit — AgentTileCLI still works fine as a general terminal tiler.

## Install

```sh
git clone https://github.com/pl0xuee/agenttilecli.git
cd agenttilecli
./install.sh
```

This builds a release binary and installs it to `~/.local/bin/agenttilecli`
(make sure that's on your `PATH`), plus adds an icon and a desktop entry so
it shows up in your application launcher.

To update later, just `git pull && ./install.sh` again.

## Uninstall

```sh
rm ~/.local/bin/agenttilecli \
   ~/.local/share/applications/agenttilecli.desktop \
   ~/.local/share/icons/hicolor/scalable/apps/agenttilecli.svg
```
