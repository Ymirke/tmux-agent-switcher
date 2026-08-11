# tmux-agent-switcher

A tmux sidebar that lets you switch between windows **and** keep an eye on your
running AI coding agents (Claude Code, Codex, and OpenCode) from one full-screen
popup.

Press <kbd>Ctrl</kbd>+<kbd>n</kbd> and you get a list of every window across all
your sessions on the left, a live preview of the selected window on the right,
and a status badge on any pane running an agent: **Working**, **Blocked**
(waiting on you), or **Idle** (done). Jump straight to the agent that needs you.

> [!NOTE]
> Detection is **fully passive** (see [How it works](#how-it-works)). The plugin
> never wraps, shims, or launches your agents. You run `claude`, `codex`, or
> `opencode` as usual; the sidebar reads tmux and the process table.

![tmux-agent-switcher showing agent statuses and a live Codex pane preview](assets/demo.png)

## Features

- **Cross-session window switcher** in a full-screen popup with a live, scaled
  preview of the highlighted window.
- **Agent monitoring**: each pane running Claude Code, Codex, or OpenCode is
  tagged Working / Blocked / Idle, with a run timer, so you can see at a glance
  which agent is waiting on input.
- **Tab status indicators**: the same rolled-up agent state is appended to each
  tmux window tab without replacing your existing tab format.
- **Vim-aware navigation**: <kbd>Ctrl</kbd>+<kbd>h/j/k/l</kbd> move between panes,
  windows, and sessions but pass through to Vim/Neovim when it's focused.
- **No daemon to babysit**: a lightweight background poller starts itself the
  first time you open the sidebar and keeps agent state fresh.

## Requirements

- **tmux ≥ 3.3** (for `display-popup -B -e`)
- **bash** and **ps** (present on macOS and Linux)
- One or more agents (`claude`, `codex`, `opencode`) running inside tmux panes.
  That's all detection needs; launch them however you like.
- To build from source: a **Rust toolchain** (only needed if no prebuilt binary
  exists for your platform; see below).

## Install

### With [TPM](https://github.com/tmux-plugins/tpm) (recommended)

Add to `~/.tmux.conf` (or `~/.config/tmux/tmux.conf`):

```tmux
set -g @plugin 'Ymirke/tmux-agent-switcher'
```

Then press <kbd>prefix</kbd> + <kbd>I</kbd> to install. On first use the plugin
downloads a prebuilt binary for your platform (falling back to a source build if
you have Rust and no prebuilt exists).

### Manual

```sh
git clone https://github.com/Ymirke/tmux-agent-switcher \
  ~/.tmux/plugins/tmux-agent-switcher
```

Add to your tmux config and reload:

```tmux
run-shell "~/.tmux/plugins/tmux-agent-switcher/tmux-agent-switcher.tmux"
```

See [`examples/tmux.conf`](examples/tmux.conf) for a manual keybinding snippet.

### From source (cargo)

```sh
cargo install --git https://github.com/Ymirke/tmux-agent-switcher
```

## Usage

| Key | Action |
| --- | --- |
| <kbd>Ctrl</kbd>+<kbd>n</kbd> | Open the sidebar |
| <kbd>Ctrl</kbd>+<kbd>j</kbd> / <kbd>Ctrl</kbd>+<kbd>k</kbd> | Switch to the next / previous session (pass through to Vim if focused) |
| <kbd>Ctrl</kbd>+<kbd>h</kbd> / <kbd>Ctrl</kbd>+<kbd>l</kbd> | Move panes / wrap to prev/next window (pass through to Vim if focused) |

Inside the switcher, Vim mode is the default. Bare <kbd>j/k</kbd> moves the
highlight, and a count such as <kbd>10j</kbd>/<kbd>10k</kbd> immediately opens
the relative window. Window rows show Vim-style relative numbers: the selected
window is <kbd>0</kbd>, and every other number is its distance above or below
it. After you type a count, matching relative-number prefixes are highlighted
and a dim <kbd>j</kbd> or <kbd>k</kbd> shows which targets remain available. A
prefix that matches no window cancels itself, so the next digit starts fresh.

- <kbd>Tab</kbd> cycles **Vim**, **numbers**, and **search** modes. Keeping the
  numeric shortcuts separate means the first digit of <kbd>10j</kbd> is never
  mistaken for session 1.
- Numbers mode labels sessions and their windows from 1. Press <kbd>2</kbd>,
  then <kbd>5</kbd> to open the fifth window in session 2. A comma remains
  optional, and <kbd>Enter</kbd> commits an ambiguous multi-digit window prefix.
- Search mode filters windows fuzzily (telescope.nvim style), matching session
  name, window name, process, and directory. <kbd>Esc</kbd> clears the filter,
  then closes.
- <kbd>↑</kbd>/<kbd>↓</kbd> move the selection for previewing,
  <kbd>Ctrl</kbd>+<kbd>j</kbd>/<kbd>k</kbd> opens the next/previous window
  directly, and <kbd>Enter</kbd> jumps to the selected window.
- In Vim or Numbers mode, <kbd>r</kbd> opens an editable prompt prefilled with
  the selected window's current name; <kbd>Enter</kbd> applies the rename and
  <kbd>Esc</kbd> cancels. Arrow keys, Home/End, Backspace, and Delete edit the
  field.
- <kbd>H</kbd>/<kbd>L</kbd> move between session edges: first and last in the
  current session, then first and last in the previous or next session.
- <kbd>Shift</kbd>+<kbd>J</kbd>/<kbd>K</kbd> swaps the selected session down or
  up in the list. The custom order lasts for the tmux server's lifetime.
- <kbd>Alt</kbd>+<kbd>j</kbd>/<kbd>k</kbd> (or <kbd>Alt</kbd>+<kbd>↓</kbd>/<kbd>↑</kbd>)
  moves the selected window down or up within its session, like moving a line
  in an editor. The move runs through `swap-window`, so tmux keeps the new
  order itself.
- <kbd>Shift</kbd>+<kbd>Tab</kbd> cycles the view: the left-docked **sidebar**,
  a right-docked **sidebar-right**, or a **palette** floating just above the
  middle of the screen with the selected window previewed full-screen behind
  it.
- Both toggles stick for the rest of the tmux server's lifetime.
- <kbd>Ctrl</kbd>+<kbd>t</kbd> / <kbd>Ctrl</kbd>+<kbd>s</kbd> create a new
  window / session in any mode. Press <kbd>?</kbd> for the full shortcut list.

## Configuration

Set these **before** the plugin loads:

```tmux
set -g @agent_switcher_key 'C-n'       # key that opens the sidebar (default: C-n; 'none' = don't bind)
set -g @agent_switcher_nav 'on'        # vim-aware C-h/C-j/C-k/C-l nav (default: on)
set -g @agent_switcher_view 'sidebar'  # 'sidebar' (left), 'sidebar-right' (right) or 'palette' (floating)
set -g @agent_switcher_input 'keys'    # 'keys' (default), 'numbers', or 'search'
set -g @agent_switcher_tab_status 'on' # show agent state in tmux window tabs
```

Set `@agent_switcher_key 'none'` to skip the root-table binding entirely and
open the sidebar from your own binding instead, e.g.
`bind a run-shell -b "~/.tmux/plugins/tmux-agent-switcher/bin/tmux-agent-switcher-popup '#{window_id}' '#{session_name}'"`.
Set `@agent_switcher_nav 'off'` if you already bind
<kbd>Ctrl</kbd>+<kbd>h/j/k/l</kbd> yourself or want to keep those keys.
Set `@agent_switcher_tab_status 'off'` to leave tmux's window status formats
untouched. Tab indicators begin updating after the sidebar starts its status
daemon for the first time.

## How it works

The plugin observes; it never intercepts. Agent state comes from:

- `tmux` pane metadata: `pane_current_command` and the OSC pane title,
- `tmux capture-pane`: the visible screen text, and
- a `ps` process-tree snapshot that attributes agents to panes.

**Working** is inferred from the agent's on-screen activity indicators,
**Blocked** from a prompt/selection awaiting input, and **Idle** once activity
settles (with debouncing so a single stray sample can't flash a false "done").
There are no wrappers, shims, PID files, FIFOs, `LD_PRELOAD`, or log scraping
around your agents.

> [!WARNING]
> Because detection reads the agents' on-screen output, it is **heuristic and
> version-sensitive**: a Claude/Codex/OpenCode UI change, a custom theme, or a
> non-English locale can throw off state classification. It's best-effort and
> expected to need occasional upkeep as the agent CLIs evolve.

## Development

```sh
cargo test          # unit tests
cargo build --release
```

The `bin/tmux-agent-switcher` launcher prefers an existing release binary,
otherwise downloads a prebuilt one, otherwise builds from source. Editing
`src/` and reopening the sidebar picks up your changes.

## Roadmap / release checklist

Tracking the path to a trustworthy first release. Checked items are done in this
repo; unchecked ones need a published release or external accounts.

- [x] Scope to the shippable product (Rust crate + launchers + tmux entry point); leave personal dotfiles and superseded scripts out
- [x] Neutralize hardcoded personal paths in test fixtures
- [x] TPM entry point (`tmux-agent-switcher.tmux`) with a tmux-version guard and configurable keys
- [x] Launcher prefers a prebuilt binary, downloads on first run, falls back to `cargo build`
- [x] Release workflow builds per-platform binaries (macOS arm64/x86_64, Linux musl x86_64/aarch64) and attaches them to GitHub Releases
- [x] `LICENSE` (MIT)
- [x] README with install, usage, requirements, and the detection caveat
- [x] Keybindings overridable via `@agent_switcher_*` options
- [x] CI runs `cargo test` on macOS + Linux
- [ ] Add a demo GIF to the README
- [ ] Headless smoke test of the status daemon in CI
- [ ] Tag `v0.1.0` and verify `prefix + I` on a clean machine with **no Rust toolchain**, on both macOS arm64 and Linux
- [ ] Publish to crates.io and add a Homebrew tap as alternate install channels
- [ ] Expose detection patterns (agent names, Working/Blocked phrases) as config for other locales / agent-UI versions

## License

[MIT](LICENSE).
