//! The interactive switcher: terminal setup, the event loop, and the key
//! handling for each input mode.

pub(crate) mod layout;
pub(crate) mod render;
pub(crate) mod state;

use std::{
    fmt, io,
    process::Command,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute, queue,
    style::force_color_output,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};

use crate::{
    cards::{
        apply_session_order, group_cards_by_session, load_cards, load_session_order,
        persist_session_order,
    },
    model::{SessionGroup, SwitcherAction, WindowCard},
    preview::PreviewMirror,
    search::{apply_query, delete_query_word, filter_sessions},
    tmux::{
        current_window_id, env_tmux_value, kill_window, move_window, rename_window, swap_windows,
        tmux_output, tmux_status,
    },
};
use layout::{compact_navigation_height, switcher_layout};
use render::draw;
use state::{
    accept_numbered_session, compact_lines, format_input_mode, format_view_mode, handle_prompt_key,
    initial_grid_state, keep_compact_selection_visible, move_compact_selection,
    move_compact_session_edge, parse_input_mode, parse_view_mode, push_matching_movement_count,
    push_numbered_choice, refresh_sessions_from_cards, remove_window_in_place,
    rename_card_in_place, select_compact_relative, select_key_action, swap_selected_session,
    swap_selected_window, sync_numbered_selection, take_counted_open_motion, Direction, GridState,
    InputMode, NumberedOpen, PromptKind, PromptState, ViewMode, WindowReorder,
};

const CARD_REFRESH_INTERVAL: Duration = Duration::from_millis(300);
/// How often the whole screen is forcibly repainted. Ratatui only rewrites
/// cells it believes changed, so anything that scribbles on the terminal
/// behind its back — tmux compositing glitches while a busy pane redraws
/// under the popup, wide glyphs in mirrored pane content nudging the cursor —
/// would otherwise stay smeared across the modal until that cell happens to
/// change. A periodic full redraw self-heals within half a second.
const FULL_REDRAW_INTERVAL: Duration = Duration::from_millis(500);
const TUI_TICK_INTERVAL: Duration = Duration::from_millis(50);
const VIEW_MODE_OPTION: &str = "@tmux_agent_sidebar_view";
const INPUT_MODE_OPTION: &str = "@tmux_agent_sidebar_input";
const CONFIGURED_INPUT_MODE_OPTION: &str = "@agent_sidebar_input";

/// tmux's `extended-keys-format xterm` can encode Shift+letter as an xterm
/// modifyOtherKeys sequence, which Crossterm 0.27 does not parse. Reset that
/// mode before requesting CSI-u with alternate keys so Shift stays intact.
#[derive(Clone, Copy, Debug)]
struct DisableModifyOtherKeys;

impl crossterm::Command for DisableModifyOtherKeys {
    fn write_ansi(&self, output: &mut impl fmt::Write) -> fmt::Result {
        output.write_str("\x1b[>4;0m")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "modifyOtherKeys reset is not implemented for the legacy Windows API",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

fn keyboard_enhancement_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
}

fn window_reorder_direction(key: KeyEvent) -> Option<Direction> {
    let alt = key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::META);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    if control {
        return None;
    }

    match key.code {
        KeyCode::Down if alt => Some(Direction::Down),
        KeyCode::Up if alt => Some(Direction::Up),
        KeyCode::Char('j' | 'J') if alt => Some(Direction::Down),
        KeyCode::Char('k' | 'K') if alt => Some(Direction::Up),
        // macOS can translate Option+j/k into printable characters instead of
        // sending an Alt modifier. Accept the standard US-layout results too.
        KeyCode::Char('∆') => Some(Direction::Down),
        KeyCode::Char('˚') => Some(Direction::Up),
        _ => None,
    }
}

fn session_reorder_direction(key: KeyEvent) -> Option<Direction> {
    let alt_or_control = key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::META | KeyModifiers::CONTROL);
    if alt_or_control {
        return None;
    }

    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('J') => Some(Direction::Down),
        KeyCode::Char('K') => Some(Direction::Up),
        KeyCode::Char('j') if shift => Some(Direction::Down),
        KeyCode::Char('k') if shift => Some(Direction::Up),
        _ => None,
    }
}

/// Runs the sidebar and live preview inside a full-screen tmux popup.
pub fn run_tui(cards: Vec<WindowCard>) -> Result<Option<SwitcherAction>> {
    if cards.is_empty() {
        return Ok(None);
    }
    force_color_output(true);
    let current_window_id = current_window_id();

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(
        stdout,
        DisableModifyOtherKeys,
        PushKeyboardEnhancementFlags(keyboard_enhancement_flags()),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_tui_loop(&mut terminal, cards, current_window_id.as_deref());
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        PopKeyboardEnhancementFlags,
        DisableModifyOtherKeys
    )?;
    terminal.show_cursor()?;
    result
}

/// An initial list move to apply as soon as the switcher opens, so a key binding
/// can drop the user straight into navigating (e.g. Ctrl+j opens moved one down).
/// Driven by the `TMUX_AGENT_SIDEBAR_INITIAL_MOVE` env var set by the launcher.
fn initial_move_direction() -> Option<Direction> {
    match env_tmux_value("TMUX_AGENT_SIDEBAR_INITIAL_MOVE").as_deref() {
        Some("down") => Some(Direction::Down),
        Some("up") => Some(Direction::Up),
        _ => None,
    }
}

/// The view style the switcher opens with. The launcher passes the configured
/// (`@agent_sidebar_view`) or last-toggled style via the
/// `TMUX_AGENT_SIDEBAR_VIEW` env var.
fn initial_view_mode() -> ViewMode {
    env_tmux_value("TMUX_AGENT_SIDEBAR_VIEW")
        .as_deref()
        .and_then(parse_view_mode)
        .unwrap_or(ViewMode::Sidebar)
}

/// Remember a toggled view style for the tmux server's lifetime so the next
/// open reuses it (the launcher reads this option back). Best-effort: a failure
/// only loses the stickiness.
fn persist_view_mode(view: ViewMode) {
    let _ = tmux_status(Command::new("tmux").args([
        "set-option",
        "-g",
        VIEW_MODE_OPTION,
        format_view_mode(view),
    ]));
}

/// The input mode the sidebar opens with. A mode selected with Tab takes
/// priority over the configured default for the rest of the tmux server life.
fn initial_input_mode() -> InputMode {
    let environment_mode = env_tmux_value("TMUX_AGENT_SIDEBAR_INPUT")
        .as_deref()
        .and_then(parse_input_mode);
    if let Some(input) = environment_mode {
        return input;
    }

    for option in [INPUT_MODE_OPTION, CONFIGURED_INPUT_MODE_OPTION] {
        let Ok(value) = tmux_output(&["show-option", "-gqv", option]) else {
            continue;
        };
        if let Some(input) = parse_input_mode(value.trim()) {
            return input;
        }
    }

    InputMode::Keys
}

/// Same stickiness as [`persist_view_mode`], for the Tab-toggled input mode.
fn persist_input_mode(input: InputMode) {
    let _ = tmux_status(Command::new("tmux").args([
        "set-option",
        "-g",
        INPUT_MODE_OPTION,
        format_input_mode(input),
    ]));
}

/// Everything the switcher tracks while open: the full and filtered session
/// lists, the selection, and the view/input modes.
struct SwitcherUi {
    sessions: Vec<SessionGroup>,
    filtered: Vec<SessionGroup>,
    query: String,
    state: GridState,
    view: ViewMode,
    input: InputMode,
    movement_count: Option<usize>,
    numbered_input: String,
    show_help: bool,
    prompt: Option<PromptState>,
}

impl SwitcherUi {
    fn new(cards: Vec<WindowCard>, current_window_id: Option<&str>, terminal_size: Rect) -> Self {
        let mut sessions = group_cards_by_session(cards);
        if let Ok(order) = load_session_order() {
            apply_session_order(&mut sessions, &order);
        }
        let filtered = filter_sessions(&sessions, "");
        let mut ui = Self {
            sessions,
            filtered,
            query: String::new(),
            state: GridState::new(),
            view: initial_view_mode(),
            input: initial_input_mode(),
            movement_count: None,
            numbered_input: String::new(),
            show_help: false,
            prompt: None,
        };
        ui.state = initial_grid_state(
            &ui.filtered,
            current_window_id,
            ui.navigation_height(terminal_size),
        );
        if let Some(direction) = initial_move_direction() {
            let navigation_height = ui.navigation_height(terminal_size);
            move_compact_selection(&mut ui.state, &ui.filtered, direction, navigation_height);
        }
        ui
    }

    /// The list viewport height used for scrolling, derived from the current
    /// layout (and therefore from the view/input modes and help visibility).
    fn navigation_height(&self, terminal_size: Rect) -> u16 {
        compact_navigation_height(
            terminal_size,
            false,
            self.view,
            compact_lines(&self.filtered).len(),
            self.input,
        )
    }

    fn refilter(&mut self, navigation_height: u16) {
        apply_query(
            &mut self.filtered,
            &mut self.state,
            &self.sessions,
            &self.query,
            navigation_height,
        );
    }

    fn refresh_cards(&mut self, cards: Vec<WindowCard>, navigation_height: u16) {
        refresh_sessions_from_cards(
            &mut self.sessions,
            &mut self.filtered,
            &mut self.state,
            cards,
            &self.query,
            navigation_height,
        );
    }

    fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    fn open_new_window_prompt(&mut self) {
        if let Some(card) = self.state.selected_card(&self.filtered) {
            let session_name = card.session_name.clone();
            self.show_help = false;
            self.prompt = Some(PromptState::new(PromptKind::NewWindow { session_name }));
        }
    }

    fn open_new_session_prompt(&mut self) {
        self.show_help = false;
        self.prompt = Some(PromptState::new(PromptKind::NewSession));
    }

    fn open_rename_prompt(&mut self) {
        if let Some(card) = self.state.selected_card(&self.filtered) {
            let kind = PromptKind::RenameWindow {
                window_id: card.window_id.clone(),
            };
            let window_name = card.window_name.clone();
            self.show_help = false;
            self.prompt = Some(PromptState::with_input(kind, window_name));
        }
    }

    /// Renames the window in tmux and patches the cached card lists so the
    /// sidebar shows the new name immediately (the periodic card refresh would
    /// otherwise lag by one interval).
    fn apply_rename(&mut self, window_id: &str, window_name: &str) {
        if rename_window(window_id, window_name).is_err() {
            return;
        }
        rename_card_in_place(
            &mut self.sessions,
            &mut self.filtered,
            window_id,
            window_name,
        );
    }

    /// Closes the selected window and patches the cached card lists so the
    /// list stays open with the nearest remaining window selected.
    fn close_selected_window_with<F>(&mut self, navigation_height: u16, close_window: F)
    where
        F: FnOnce(&str) -> Result<()>,
    {
        let Some(window_id) = self
            .state
            .selected_card(&self.filtered)
            .map(|card| card.window_id.clone())
        else {
            return;
        };
        if close_window(&window_id).is_err() {
            return;
        }
        remove_window_in_place(
            &mut self.sessions,
            &mut self.filtered,
            &mut self.state,
            &window_id,
            &self.query,
            navigation_height,
        );
    }

    /// Moves the selected window one slot, including across session boundaries.
    /// The cache changes immediately and tmux mirrors the move. If tmux rejects
    /// it because a window vanished, the next card refresh restores real state.
    fn move_selected_window(&mut self, direction: Direction, navigation_height: u16) {
        let Some(reorder) = swap_selected_window(
            &mut self.sessions,
            &mut self.filtered,
            &mut self.state,
            &self.query,
            direction,
            navigation_height,
        ) else {
            return;
        };

        match reorder {
            WindowReorder::Swap {
                source_window_id,
                target_window_id,
            } => {
                let _ = swap_windows(&source_window_id, &target_window_id);
            }
            WindowReorder::Move {
                source_window_id,
                target_window_id,
                before_target,
            } => {
                let _ = move_window(&source_window_id, &target_window_id, before_target);
            }
        }
    }

    fn handle_mouse(&mut self, kind: MouseEventKind, navigation_height: u16) {
        match kind {
            MouseEventKind::ScrollDown => {
                self.numbered_input.clear();
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Down,
                    navigation_height,
                );
            }
            MouseEventKind::ScrollUp => {
                self.numbered_input.clear();
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Up,
                    navigation_height,
                );
            }
            _ => {}
        }
    }

    /// Feeds one key into the switcher. `Some(result)` closes it with that
    /// outcome (an action to run, or None for a plain quit); `None` keeps it
    /// open.
    fn handle_key(&mut self, key: KeyEvent, terminal_size: Rect) -> Option<Option<SwitcherAction>> {
        self.handle_key_with_window_closer(key, terminal_size, kill_window)
    }

    fn handle_key_with_window_closer<F>(
        &mut self,
        key: KeyEvent,
        terminal_size: Rect,
        close_window: F,
    ) -> Option<Option<SwitcherAction>>
    where
        F: FnOnce(&str) -> Result<()>,
    {
        if let Some(active_prompt) = self.prompt.as_mut() {
            if let Some(result) = handle_prompt_key(active_prompt, key) {
                match result {
                    // Renames run in place so the switcher stays open on the
                    // updated list; other prompt actions close it and execute
                    // after the TUI has torn down.
                    Some(SwitcherAction::RenameWindow {
                        window_id,
                        window_name,
                    }) => {
                        self.prompt = None;
                        self.apply_rename(&window_id, &window_name);
                    }
                    Some(action) => return Some(Some(action)),
                    None => self.prompt = None,
                }
            }
            return None;
        }

        let modified_question_mark = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::META);
        if key.code == KeyCode::Char('?') && !modified_question_mark {
            self.toggle_help();
            return None;
        }

        let navigation_height = self.navigation_height(terminal_size);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::META);

        // A Vim-style count survives until j/k consumes it or another digit
        // leaves it with no matching relative target.
        let keys_count_key = self.input == InputMode::Keys
            && !ctrl
            && matches!(key.code, KeyCode::Char(ch) if ch.is_ascii_digit());
        let keys_count_motion = self.input == InputMode::Keys
            && !ctrl
            && !alt
            && matches!(key.code, KeyCode::Char('j' | 'k'));
        if !keys_count_key && !keys_count_motion {
            self.movement_count = None;
        }

        if let Some(direction) = window_reorder_direction(key) {
            self.numbered_input.clear();
            self.move_selected_window(direction, navigation_height);
            return None;
        }
        if let Some(direction) = session_reorder_direction(key) {
            self.numbered_input.clear();
            if swap_selected_session(
                &mut self.sessions,
                &mut self.filtered,
                &mut self.state,
                &self.query,
                direction,
                navigation_height,
            ) {
                persist_session_order(&self.sessions);
            }
            return None;
        }

        match key.code {
            KeyCode::Esc => {
                // Telescope-style: first Esc clears the filter, a second
                // one closes the switcher. Numbers similarly clears a
                // partial numbered address before closing.
                if self.input == InputMode::Numbers && !self.numbered_input.is_empty() {
                    self.numbered_input.clear();
                    return None;
                }
                if self.input != InputMode::Search || self.query.is_empty() {
                    return Some(None);
                }
                self.query.clear();
                self.refilter(navigation_height);
            }
            KeyCode::Enter => {
                if self.input == InputMode::Numbers {
                    if self.numbered_input.is_empty() {
                        return self.selected_action();
                    }
                    if self.numbered_input.contains(',') {
                        if let Some(card) = sync_numbered_selection(
                            &self.numbered_input,
                            &self.filtered,
                            &mut self.state,
                            NumberedOpen::Force,
                        ) {
                            return Some(Some(SwitcherAction::Select(card)));
                        }
                    } else {
                        accept_numbered_session(&mut self.numbered_input, &self.filtered);
                        sync_numbered_selection(
                            &self.numbered_input,
                            &self.filtered,
                            &mut self.state,
                            NumberedOpen::Never,
                        );
                    }
                    return None;
                }
                if let Some(action) = select_key_action(key, &self.state, &self.filtered) {
                    return Some(Some(action));
                }
            }
            KeyCode::Char(' ')
                if self.input == InputMode::Numbers && self.numbered_input.is_empty() =>
            {
                return self.selected_action();
            }
            KeyCode::Tab => {
                self.input = self.input.toggled();
                self.numbered_input.clear();
                persist_input_mode(self.input);
                let navigation_height = self.navigation_height(terminal_size);
                keep_compact_selection_visible(&mut self.state, &self.filtered, navigation_height);
            }
            KeyCode::BackTab => {
                self.numbered_input.clear();
                self.view = self.view.toggled();
                persist_view_mode(self.view);
                let navigation_height = self.navigation_height(terminal_size);
                keep_compact_selection_visible(&mut self.state, &self.filtered, navigation_height);
            }
            KeyCode::Backspace => {
                if self.input == InputMode::Numbers {
                    self.numbered_input.clear();
                    return None;
                }
                if self.query.pop().is_some() {
                    self.refilter(navigation_height);
                }
            }
            KeyCode::Down => {
                self.numbered_input.clear();
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Down,
                    navigation_height,
                );
            }
            KeyCode::Up => {
                self.numbered_input.clear();
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Up,
                    navigation_height,
                );
            }
            KeyCode::Left => {
                self.numbered_input.clear();
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Left,
                    navigation_height,
                );
            }
            KeyCode::Right => {
                self.numbered_input.clear();
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Right,
                    navigation_height,
                );
            }
            KeyCode::Char(ch) if ctrl => {
                self.numbered_input.clear();
                return self.handle_ctrl_char(ch, navigation_height);
            }
            KeyCode::Char('r') if self.input != InputMode::Search => {
                self.numbered_input.clear();
                self.open_rename_prompt();
            }
            KeyCode::Char('x') if self.input != InputMode::Search => {
                self.numbered_input.clear();
                self.close_selected_window_with(navigation_height, close_window);
            }
            KeyCode::Char('j' | 'k') if self.input == InputMode::Numbers => {
                self.numbered_input.clear();
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    if key.code == KeyCode::Char('j') {
                        Direction::Down
                    } else {
                        Direction::Up
                    },
                    navigation_height,
                );
            }
            KeyCode::Char('h' | 'l') if self.input == InputMode::Numbers => {
                self.numbered_input.clear();
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    if key.code == KeyCode::Char('h') {
                        Direction::Left
                    } else {
                        Direction::Right
                    },
                    navigation_height,
                );
            }
            KeyCode::Char(ch) if self.input == InputMode::Numbers && ch.is_ascii_digit() => {
                if let Some(card) = push_numbered_choice(
                    &mut self.numbered_input,
                    ch,
                    &self.filtered,
                    &mut self.state,
                ) {
                    return Some(Some(SwitcherAction::Select(card)));
                }
                keep_compact_selection_visible(&mut self.state, &self.filtered, navigation_height);
            }
            KeyCode::Char(',') if self.input == InputMode::Numbers => {
                if accept_numbered_session(&mut self.numbered_input, &self.filtered) {
                    sync_numbered_selection(
                        &self.numbered_input,
                        &self.filtered,
                        &mut self.state,
                        NumberedOpen::Never,
                    );
                    keep_compact_selection_visible(
                        &mut self.state,
                        &self.filtered,
                        navigation_height,
                    );
                }
            }
            KeyCode::Char(_) if self.input == InputMode::Numbers => {}
            KeyCode::Char(ch) if self.input == InputMode::Keys => {
                return self.handle_keys_mode_char(ch, navigation_height);
            }
            KeyCode::Char(ch) => {
                self.query.push(ch);
                self.refilter(navigation_height);
            }
            _ => {}
        }

        None
    }

    fn selected_action(&self) -> Option<Option<SwitcherAction>> {
        self.state
            .selected_card(&self.filtered)
            .cloned()
            .map(SwitcherAction::Select)
            .map(Some)
    }

    /// Ctrl-modified keys, active in every input mode.
    fn handle_ctrl_char(
        &mut self,
        ch: char,
        navigation_height: u16,
    ) -> Option<Option<SwitcherAction>> {
        match ch {
            'c' => return Some(None),
            'j' => {
                if let Some(card) = select_compact_relative(
                    &mut self.state,
                    &self.filtered,
                    Direction::Down,
                    1,
                    navigation_height,
                ) {
                    return Some(Some(SwitcherAction::Select(card)));
                }
            }
            'k' => {
                if let Some(card) = select_compact_relative(
                    &mut self.state,
                    &self.filtered,
                    Direction::Up,
                    1,
                    navigation_height,
                ) {
                    return Some(Some(SwitcherAction::Select(card)));
                }
            }
            'n' => {
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Down,
                    navigation_height,
                );
            }
            'p' => {
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Up,
                    navigation_height,
                );
            }
            'h' => {
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Left,
                    navigation_height,
                );
            }
            'l' => {
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Right,
                    navigation_height,
                );
            }
            'u' => {
                self.query.clear();
                self.refilter(navigation_height);
            }
            'w' => {
                delete_query_word(&mut self.query);
                self.refilter(navigation_height);
            }
            't' => self.open_new_window_prompt(),
            's' => self.open_new_session_prompt(),
            _ => {}
        }
        None
    }

    /// Unmodified characters in Keys (Vim) mode: motions, counts, and the
    /// prompt/select shortcuts.
    fn handle_keys_mode_char(
        &mut self,
        ch: char,
        navigation_height: u16,
    ) -> Option<Option<SwitcherAction>> {
        match ch {
            'q' => return Some(None),
            ' ' => {
                if let Some(card) = self.state.selected_card(&self.filtered) {
                    return Some(Some(SwitcherAction::Select(card.clone())));
                }
            }
            'n' => self.open_new_window_prompt(),
            'N' => self.open_new_session_prompt(),
            'h' => {
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Left,
                    navigation_height,
                );
            }
            'H' => {
                move_compact_session_edge(
                    &mut self.state,
                    &self.filtered,
                    Direction::Up,
                    navigation_height,
                );
            }
            'j' | 'k' => {
                if let Some((direction, count)) =
                    take_counted_open_motion(&mut self.movement_count, ch)
                {
                    if let Some(card) = select_compact_relative(
                        &mut self.state,
                        &self.filtered,
                        direction,
                        count,
                        navigation_height,
                    ) {
                        return Some(Some(SwitcherAction::Select(card)));
                    }
                } else {
                    move_compact_selection(
                        &mut self.state,
                        &self.filtered,
                        if ch == 'j' {
                            Direction::Down
                        } else {
                            Direction::Up
                        },
                        navigation_height,
                    );
                }
            }
            'l' => {
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Right,
                    navigation_height,
                );
            }
            'L' => {
                move_compact_session_edge(
                    &mut self.state,
                    &self.filtered,
                    Direction::Down,
                    navigation_height,
                );
            }
            _ if ch.is_ascii_digit() => {
                push_matching_movement_count(
                    &mut self.movement_count,
                    ch,
                    &self.state,
                    &self.filtered,
                );
            }
            _ => {}
        }
        None
    }
}

fn queue_full_repaint(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    queue!(terminal.backend_mut(), Clear(ClearType::All))?;
    terminal.swap_buffers();
    Ok(())
}

fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cards: Vec<WindowCard>,
    current_window_id: Option<&str>,
) -> Result<Option<SwitcherAction>> {
    let mut ui = SwitcherUi::new(cards, current_window_id, terminal.size()?);
    let spinner_started_at = Instant::now();
    let mut last_card_refresh = Instant::now();
    let mut last_full_redraw = Instant::now();
    let mut preview = PreviewMirror::default();

    loop {
        let now = Instant::now();
        if now.duration_since(last_card_refresh) >= CARD_REFRESH_INTERVAL {
            if let Ok(cards) = load_cards() {
                let navigation_height = ui.navigation_height(terminal.size()?);
                ui.refresh_cards(cards, navigation_height);
            }
            last_card_refresh = now;
        }
        let preview_area = switcher_layout(
            terminal.size()?,
            ui.show_help,
            ui.view,
            compact_lines(&ui.filtered).len(),
        )
        .preview;
        preview.refresh_for(ui.state.selected_card(&ui.filtered), preview_area, now);
        if now.duration_since(last_full_redraw) >= FULL_REDRAW_INTERVAL {
            queue_full_repaint(terminal)?;
            last_full_redraw = now;
        }
        let spinner_frame = spinner_started_at.elapsed().as_millis() as usize / 120;
        let input_value = if ui.input == InputMode::Numbers {
            ui.numbered_input.as_str()
        } else {
            ui.query.as_str()
        };
        terminal.draw(|frame| {
            draw(
                frame,
                &ui.filtered,
                &ui.state,
                ui.view,
                ui.input,
                ui.show_help,
                input_value,
                ui.movement_count,
                ui.prompt.as_ref(),
                &preview,
                spinner_frame,
            )
        })?;

        if !event::poll(TUI_TICK_INTERVAL)? {
            continue;
        }

        match event::read()? {
            Event::Mouse(mouse) if ui.prompt.is_none() => {
                let navigation_height = ui.navigation_height(terminal.size()?);
                ui.handle_mouse(mouse.kind, navigation_height);
            }
            Event::Resize(_, _) => queue_full_repaint(terminal)?,
            Event::Key(key) => {
                if let Some(result) = ui.handle_key(key, terminal.size()?) {
                    return Ok(result);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cards::group_cards_by_session, test_support::test_card};

    fn test_ui(input: InputMode) -> SwitcherUi {
        let sessions = group_cards_by_session(vec![test_card("work", "1"), test_card("work", "2")]);
        let filtered = sessions.clone();
        let mut state = GridState::new();
        state.selected_column = 1;

        SwitcherUi {
            sessions,
            filtered,
            query: String::new(),
            state,
            view: ViewMode::Sidebar,
            input,
            movement_count: None,
            numbered_input: String::new(),
            show_help: false,
            prompt: None,
        }
    }

    #[test]
    fn shift_j_and_k_accept_both_terminal_key_encodings() {
        assert_eq!(
            session_reorder_direction(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::SHIFT,)),
            Some(Direction::Down)
        );
        assert_eq!(
            session_reorder_direction(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::SHIFT,)),
            Some(Direction::Up)
        );
        assert_eq!(
            session_reorder_direction(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE,)),
            Some(Direction::Down)
        );
        assert_eq!(
            session_reorder_direction(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::NONE,)),
            Some(Direction::Up)
        );
    }

    #[test]
    fn popup_disables_xterm_modify_other_keys() {
        let mut ansi = String::new();
        crossterm::Command::write_ansi(&DisableModifyOtherKeys, &mut ansi).unwrap();

        assert_eq!(ansi, "\x1b[>4;0m");
    }

    #[test]
    fn popup_requests_shifted_alternate_keys() {
        let mut ansi = String::new();
        crossterm::Command::write_ansi(
            &PushKeyboardEnhancementFlags(keyboard_enhancement_flags()),
            &mut ansi,
        )
        .unwrap();

        assert_eq!(ansi, "\x1b[>5u");
    }

    #[test]
    fn alt_j_and_k_match_alt_arrow_keys() {
        for (key, direction) in [
            (KeyCode::Char('j'), Direction::Down),
            (KeyCode::Down, Direction::Down),
            (KeyCode::Char('k'), Direction::Up),
            (KeyCode::Up, Direction::Up),
        ] {
            assert_eq!(
                window_reorder_direction(KeyEvent::new(key, KeyModifiers::ALT)),
                Some(direction)
            );
        }

        assert_eq!(
            window_reorder_direction(KeyEvent::new(KeyCode::Char('∆'), KeyModifiers::NONE,)),
            Some(Direction::Down)
        );
        assert_eq!(
            window_reorder_direction(KeyEvent::new(KeyCode::Char('˚'), KeyModifiers::NONE,)),
            Some(Direction::Up)
        );
    }

    #[test]
    fn question_mark_toggles_help_in_every_input_mode() {
        for input in [InputMode::Keys, InputMode::Numbers, InputMode::Search] {
            let mut ui = test_ui(input);
            ui.query = "existing filter".to_owned();
            let selected_window = ui
                .state
                .selected_card(&ui.filtered)
                .unwrap()
                .window_id
                .clone();
            let question_mark = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT);
            let terminal_size = Rect::new(0, 0, 100, 40);
            let list_height = ui.navigation_height(terminal_size);
            let row_offset = ui.state.row_offset;

            assert_eq!(ui.handle_key(question_mark, terminal_size), None);
            assert!(ui.show_help);
            assert_eq!(ui.query, "existing filter");
            assert_eq!(ui.navigation_height(terminal_size), list_height);
            assert_eq!(ui.state.row_offset, row_offset);
            assert_eq!(
                ui.state.selected_card(&ui.filtered).unwrap().window_id,
                selected_window
            );

            assert_eq!(ui.handle_key(question_mark, terminal_size), None);
            assert!(!ui.show_help);
            assert_eq!(ui.query, "existing filter");
            assert_eq!(ui.navigation_height(terminal_size), list_height);
            assert_eq!(ui.state.row_offset, row_offset);
        }
    }

    #[test]
    fn enter_and_space_open_the_highlighted_window_in_navigation_modes() {
        let terminal_size = Rect::new(0, 0, 100, 40);
        for input in [InputMode::Keys, InputMode::Numbers] {
            for key in [KeyCode::Enter, KeyCode::Char(' ')] {
                let mut ui = test_ui(input);
                ui.handle_key(
                    KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
                    terminal_size,
                );
                ui.handle_key(
                    KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
                    terminal_size,
                );

                let action = ui.handle_key(KeyEvent::new(key, KeyModifiers::NONE), terminal_size);
                let Some(Some(SwitcherAction::Select(card))) = action else {
                    panic!("{key:?} did not select a window in {input:?} mode");
                };
                assert_eq!(card.window_id, "@work-2");
            }
        }
    }

    #[test]
    fn numbered_enter_keeps_resolving_a_typed_address() {
        let mut ui = test_ui(InputMode::Numbers);
        ui.numbered_input = "1,1".to_owned();

        let action = ui.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Rect::new(0, 0, 100, 40),
        );
        let Some(Some(SwitcherAction::Select(card))) = action else {
            panic!("typed numbered address did not select a window");
        };
        assert_eq!(card.window_id, "@work-1");
    }

    #[test]
    fn h_and_l_switch_sessions_in_navigation_modes() {
        let sessions = group_cards_by_session(vec![test_card("work", "1"), test_card("ops", "1")]);
        let terminal_size = Rect::new(0, 0, 100, 40);

        for input in [InputMode::Keys, InputMode::Numbers] {
            let mut ui = test_ui(input);
            ui.sessions = sessions.clone();
            ui.filtered = sessions.clone();
            ui.state = GridState::new();

            ui.handle_key(
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
                terminal_size,
            );
            assert_eq!(
                ui.state.selected_card(&ui.filtered).unwrap().session_name,
                "ops"
            );

            ui.handle_key(
                KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
                terminal_size,
            );
            assert_eq!(
                ui.state.selected_card(&ui.filtered).unwrap().session_name,
                "work"
            );
        }
    }

    #[test]
    fn x_closes_the_selected_window_without_closing_the_sidebar() {
        for input in [InputMode::Keys, InputMode::Numbers] {
            let mut ui = test_ui(input);

            assert_eq!(
                ui.handle_key_with_window_closer(
                    KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                    Rect::new(0, 0, 100, 40),
                    |window_id| {
                        assert_eq!(window_id, "@work-2");
                        Ok(())
                    },
                ),
                None
            );
            assert_eq!(ui.sessions[0].cards.len(), 1);
            assert_eq!(
                ui.state.selected_card(&ui.filtered).unwrap().window_id,
                "@work-1"
            );
        }
    }

    #[test]
    fn x_remains_query_text_in_search_mode() {
        let mut ui = test_ui(InputMode::Search);

        assert_eq!(
            ui.handle_key_with_window_closer(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                Rect::new(0, 0, 100, 40),
                |_| panic!("search input must not close a window"),
            ),
            None
        );
        assert_eq!(ui.query, "x");
    }
}
