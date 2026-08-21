//! tmux-agent-sidebar: a tmux sidebar that switches windows and monitors
//! running AI coding agents by passive observation.
//!
//! Module map:
//! - [`model`] — core types (agents, statuses, windows, panes, cards)
//! - [`tmux`] — running tmux commands, parsing their output, executing actions
//! - [`detect`] — recognizing agent processes and inferring their state
//! - [`daemon`] — the background poller that caches status in tmux options
//! - [`cards`] — building/grouping the window cards and session ordering
//! - [`search`] — fuzzy filtering of the session/window list
//! - [`preview`] — pane capture, ANSI parsing, multi-pane compositing
//! - [`ui`] — the interactive switcher (event loop, state, layout, rendering)

use ratatui::style::Color;

pub const TMUX_ORANGE: Color = Color::Indexed(202);

mod cards;
mod daemon;
mod detect;
mod model;
mod preview;
mod search;
mod spawn;
mod tmux;
mod ui;

#[cfg(test)]
mod test_support;

pub use cards::{
    build_cards, build_cards_with_previews, codex_unread_dir, codex_unread_file,
    group_cards_by_session, load_cards,
};
pub use daemon::{ensure_status_daemon, poll_agent_status_once, run_status_daemon, Debounce};
pub use detect::{detect_agent_from_process_name, detect_agent_state};
pub use model::{
    AgentEvidence, AgentKind, AgentState, AgentStatus, SessionGroup, SwitcherAction, TmuxPane,
    TmuxWindow, WindowCard,
};
pub use preview::normalize_preview_line;
pub use search::filter_sessions;
pub use tmux::{
    clear_unread_for_pane, create_session, create_window, current_window_id, env_tmux_value,
    execute_action, kill_window, mark_unread_for_pane, mark_window_read, move_window, parse_panes,
    parse_windows, rename_window, select_card,
};
pub use ui::run_tui;
pub use ui::state::{
    compact_selected_line_index, initial_grid_state, move_compact_selection, Direction, GridState,
    InputMode, ViewMode,
};
