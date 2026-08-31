# tmux-agent-sidebar

tmux-agent-sidebar is a tmux window switcher and AI agent monitor for Claude
Code, Codex, and OpenCode.

Press <kbd>Ctrl</kbd>+<kbd>n</kbd> to open a full-screen popup. It shows every
window across all sessions in a sidebar, with a live preview of the selected
window beside it. Agent rows show **Working**, **Blocked**, or **Idle**, so you
can find an agent that needs input.

> [!NOTE]
> Detection is passive. The plugin does not wrap or start your agents. Run
> `claude`, `codex`, or `opencode` as usual inside tmux.

![tmux-agent-sidebar showing agent statuses and a live Codex pane preview](assets/demo.png)

## Features

- A full-screen popup with a sidebar and live preview of the selected window.
- Cross-session window selection and reordering with Vim, number, and search
  input modes.
- Working, Blocked, and Idle agent states with run timers.
- Optional agent state icons in tmux window tabs.
- Vim-aware <kbd>Ctrl</kbd>+<kbd>h/j/k/l</kbd> navigation.
- Adaptive status polling with process-table snapshot reuse.
- A configurable periodic heartbeat command.

## Requirements

- tmux 3.3 or newer.
- Bash and `ps` on macOS or Linux.
- A Rust toolchain only when no prebuilt binary exists for your platform.

## Install

### TPM

Add the plugin to your tmux configuration:

```tmux
set -g @plugin 'Ymirke/tmux-agent-sidebar'
```

Press <kbd>prefix</kbd>+<kbd>I</kbd> to install it. The first use downloads a
prebuilt binary. If no binary exists, the launcher builds from source.

### Manual install

Clone the repository:

```sh
git clone https://github.com/Ymirke/tmux-agent-sidebar \
  ~/.tmux/plugins/tmux-agent-sidebar
```

Load the plugin from your tmux configuration:

```tmux
run-shell "~/.tmux/plugins/tmux-agent-sidebar/tmux-agent-sidebar.tmux"
```

See [`examples/tmux.conf`](examples/tmux.conf) for a complete example.

### Cargo

```sh
cargo install --git https://github.com/Ymirke/tmux-agent-sidebar
```

## Usage

The sidebar uses one global key. The default is <kbd>Ctrl</kbd>+<kbd>n</kbd>.
This key does not use the tmux prefix.

| Key | Action |
| --- | --- |
| <kbd>Ctrl</kbd>+<kbd>n</kbd> | Open the sidebar |
| <kbd>Ctrl</kbd>+<kbd>j</kbd> / <kbd>Ctrl</kbd>+<kbd>k</kbd> | Select the next or previous session |
| <kbd>Ctrl</kbd>+<kbd>h</kbd> / <kbd>Ctrl</kbd>+<kbd>l</kbd> | Select a pane or adjacent window |

Inside the sidebar, Vim input mode is the default:

- Use <kbd>j</kbd> and <kbd>k</kbd> to move the selection.
- Use a count such as <kbd>10j</kbd> or <kbd>10k</kbd> to open a relative window.
- Use <kbd>Shift</kbd>+<kbd>j/k</kbd> or <kbd>Shift</kbd>+<kbd>↑/↓</kbd> to move
  the selected session.
- Use <kbd>Alt</kbd>+<kbd>j/k</kbd> or <kbd>Alt</kbd>+<kbd>↑/↓</kbd> to move a
  window. A window moves into the next session when it crosses a session edge.
- Press <kbd>Enter</kbd> to open the selected window.
- Press <kbd>Tab</kbd> to select Vim, number, or search input mode.
- Press <kbd>Shift</kbd>+<kbd>Tab</kbd> to place the sidebar on the left or
  right, or use the centered palette view.
- Press <kbd>r</kbd> to rename the selected window.
- Press <kbd>u</kbd> to mark the selected tab unread.
- Press <kbd>e</kbd> to mark the selected tab read without opening it.
- Press <kbd>x</kbd> to close the selected window.
- Press <kbd>Ctrl</kbd>+<kbd>t</kbd> to create a window.
- Press <kbd>Ctrl</kbd>+<kbd>s</kbd> to create a session.
- Press <kbd>?</kbd> to show all sidebar shortcuts.

The preview changes as you move the selection. Press <kbd>Enter</kbd> to switch
to the selected window and close the popup. Rename and close operations update
the open list in place. Press <kbd>Esc</kbd> or <kbd>Ctrl</kbd>+<kbd>c</kbd> to
close the popup without switching.

## Configuration

Set options before the plugin loads:

```tmux
set -g @agent_sidebar_key 'C-n'          # global popup opener; empty disables it
set -g @agent_sidebar_nav 'on'           # Vim-aware C-h/C-j/C-k/C-l navigation
set -g @agent_sidebar_view 'sidebar'     # sidebar, sidebar-right, or palette
set -g @agent_sidebar_input 'keys'       # keys, numbers, or search
set -g @agent_sidebar_tab_status 'on'    # agent state icons in tmux window tabs
set -g @agent_sidebar_daemon_autostart 'off' # start the daemon at plugin load
set -g @agent_sidebar_tick_command ''    # optional daemon heartbeat command
set -g @agent_sidebar_tick_interval '60' # heartbeat interval in seconds
```

For example, use <kbd>Ctrl</kbd>+<kbd>g</kbd> instead of the default key:

```tmux
set -g @agent_sidebar_key 'C-g'
```

Set `@agent_sidebar_nav` to `off` if you use the navigation keys for another
purpose. Set `@agent_sidebar_tab_status` to `off` to keep the current tmux
window formats unchanged.

Set `@agent_sidebar_daemon_autostart` to `on` to publish agent status before
the first sidebar opens. The daemon starts in the background, so a download or
build does not block the tmux configuration.

The daemon starts the heartbeat command with `sh -c`. The heartbeat does not
block status polling. Invalid intervals use 60 seconds.

## How it works

The plugin reads these sources:

- tmux pane metadata, including the command and pane title;
- visible pane text from `tmux capture-pane`;
- a cached `ps` process-tree snapshot.

The daemon polls every two seconds when no pane has an agent. When an agent is
present, the daemon polls every 300 milliseconds while pane state changes.
After 20 unchanged polls, the interval changes to one second. A state change
restores the 300 millisecond interval.

The daemon stores pane and window state in tmux options. The sidebar and tmux
status line read these options without a new process scan.

> [!WARNING]
> Detection depends on visible agent output. Agent UI changes, custom themes,
> and non-English output can reduce detection accuracy.

## Development

Run the checks:

```sh
cargo test
cargo build --release
```

The [`bin/tmux-agent-sidebar`](bin/tmux-agent-sidebar) launcher uses an existing
release binary when possible. It downloads a binary or builds one when needed.

## Release checklist

- [x] Rust crate, launchers, and tmux entry point.
- [x] Configurable sidebar key, view, and input mode.
- [x] Prebuilt release binaries for macOS and Linux.
- [x] CI tests on macOS and Linux.
- [ ] Add a current sidebar demo.
- [ ] Add a headless status daemon smoke test to CI.
- [ ] Publish the crate and a Homebrew tap.

## License

[MIT](LICENSE)
