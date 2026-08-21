#!/usr/bin/env bash
# TPM entry point for tmux-agent-sidebar.
#
# Binds one global key that opens the sidebar and live preview in a full-screen
# popup. The launcher builds or downloads the Rust binary lazily on first use.
#
# Options (set before this plugin is loaded):
#   set -g @agent_sidebar_key 'C-n'       # global popup opener (default C-n)
#   set -g @agent_sidebar_nav 'on'        # vim-aware C-h/C-j/C-k/C-l navigation
#   set -g @agent_sidebar_tab_status 'on' # agent indicator in window tabs
#   set -g @agent_sidebar_daemon_autostart 'off' # start daemon at plugin load
set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
POPUP="$CURRENT_DIR/bin/tmux-agent-sidebar-popup"

tmux_option() {
  local value
  value="$(tmux show-option -gqv "$1")"
  if [[ -z "$value" ]]; then echo "$2"; else echo "$value"; fi
}

tmux_option_allow_empty() {
  if tmux show-option -g "$1" >/dev/null 2>&1; then
    tmux show-option -gqv "$1"
  else
    echo "$2"
  fi
}

# display-popup -B and -e require tmux 3.3 or newer.
version="$(tmux -V | grep -oE '[0-9]+\.[0-9]+' | head -n1 || true)"
if [[ -n "$version" ]]; then
  major="${version%%.*}"
  minor="${version##*.}"
  if (( major < 3 || (major == 3 && minor < 3) )); then
    tmux display-message "tmux-agent-sidebar: requires tmux >= 3.3 (found $(tmux -V))"
    exit 0
  fi
fi

open_key="$(tmux_option_allow_empty @agent_sidebar_key C-n)"
nav="$(tmux_option @agent_sidebar_nav on)"
tab_status="$(tmux_option @agent_sidebar_tab_status on)"
daemon_autostart="$(tmux_option @agent_sidebar_daemon_autostart off)"

configure_tab_status() {
  local option="$1"
  local marker='#{@tmux_agent_sidebar_window_icon}'
  local current
  current="$(tmux show-option -gqv "$option")"

  current="${current//$marker/}"
  if [[ "$tab_status" == "on" ]]; then
    current="${current}${marker}"
  fi
  tmux set-option -gq "$option" "$current"
}

configure_tab_status window-status-format
configure_tab_status window-status-current-format

# Start the daemon before the first sidebar opens. The background launcher
# prevents a first download or build from blocking the tmux configuration.
if [[ "$daemon_autostart" == "on" ]]; then
  tmux run-shell -b "exec '$CURRENT_DIR/bin/tmux-agent-sidebar' status-daemon"
fi

remove_persistent_sidebar() {
  local pane window_id saved_layout
  pane="$(tmux show-option -gqv @tmux_agent_sidebar_pane)"

  if [[ -n "$pane" ]] && tmux display-message -p -t "$pane" '#{pane_id}' >/dev/null 2>&1; then
    window_id="$(tmux display-message -p -t "$pane" '#{window_id}')"
    saved_layout="$(tmux show-option -wqv -t "$pane" @tmux_agent_sidebar_layout)"
    tmux kill-pane -t "$pane"
    if [[ -n "$saved_layout" ]]; then
      tmux select-layout -t "$window_id" "$saved_layout" >/dev/null 2>&1 || true
    fi
  fi

  while IFS= read -r window_id; do
    tmux set-option -wuq -t "$window_id" @tmux_agent_sidebar_layout
    tmux set-option -wuq -t "$window_id" @tmux_agent_sidebar_panes
  done < <(tmux list-windows -a -F '#{window_id}')

  tmux set-option -guq @tmux_agent_sidebar_pane
  tmux set-option -guq @tmux_agent_sidebar_moving
  tmux set-option -guq @tmux_agent_sidebar_client
}

remove_persistent_sidebar

previous_open_key="$(tmux show-option -gqv @tmux_agent_sidebar_bound_key)"
if [[ -n "$previous_open_key" && "$previous_open_key" != "$open_key" ]]; then
  tmux unbind-key -n "$previous_open_key" 2>/dev/null || true
fi

if [[ -n "$open_key" ]]; then
  tmux bind-key -n "$open_key" run-shell -b "$POPUP '#{window_id}' '#{session_name}'"
  tmux set-option -gq @tmux_agent_sidebar_bound_key "$open_key"
else
  tmux set-option -guq @tmux_agent_sidebar_bound_key
fi

# Remove hooks installed by the short-lived persistent-pane implementation.
for hook in session-window-changed client-session-changed client-attached client-detached; do
  tmux set-hook -gu "${hook}[50]" 2>/dev/null || true
done

if [[ "$nav" == "on" ]]; then
  is_vim="ps -o state= -o comm= -t '#{pane_tty}' | grep -iqE '^[^TXZ ]+ +(\\S+/)?g?(view|n?vim?x?)(diff)?\$'"

  tmux bind-key -n C-h if-shell "$is_vim" "send-keys C-h" "if -F '#{pane_at_left}' 'previous-window' 'select-pane -L'"
  tmux bind-key -n C-l if-shell "$is_vim" "send-keys C-l" "if -F '#{pane_at_right}' 'next-window' 'select-pane -R'"
  tmux bind-key -n C-j if-shell "$is_vim" "send-keys C-j" "switch-client -n"
  tmux bind-key -n C-k if-shell "$is_vim" "send-keys C-k" "switch-client -p"

  tmux unbind-key -q -T tree-mode C-j 2>/dev/null || true
  tmux unbind-key -q -T tree-mode C-k 2>/dev/null || true
fi
