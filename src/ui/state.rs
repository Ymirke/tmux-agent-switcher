//! Switcher interaction state: the grid selection, view/input modes, the
//! rename/new prompts, numbered addressing, and compact-list navigation.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    cards::{apply_session_order, group_cards_by_session},
    model::{SessionGroup, SwitcherAction, WindowCard},
    search::filter_sessions,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
}

/// Where the window list sits: fixed to the left or right edge with the
/// preview beside it (Sidebar / SidebarRight), or floating around the upper
/// middle of the screen with the preview filling the whole screen behind it,
/// like an editor command palette (Palette).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewMode {
    Sidebar,
    SidebarRight,
    Palette,
}

impl ViewMode {
    pub(crate) fn toggled(self) -> Self {
        match self {
            ViewMode::Sidebar => ViewMode::SidebarRight,
            ViewMode::SidebarRight => ViewMode::Palette,
            ViewMode::Palette => ViewMode::Sidebar,
        }
    }
}

pub(crate) fn parse_view_mode(value: &str) -> Option<ViewMode> {
    match value {
        "sidebar" | "left" => Some(ViewMode::Sidebar),
        "sidebar-right" | "right" => Some(ViewMode::SidebarRight),
        "palette" | "center" => Some(ViewMode::Palette),
        _ => None,
    }
}

pub(crate) fn format_view_mode(view: ViewMode) -> &'static str {
    match view {
        ViewMode::Sidebar => "sidebar",
        ViewMode::SidebarRight => "sidebar-right",
        ViewMode::Palette => "palette",
    }
}

/// How keystrokes are interpreted, cycled with Tab. Keys handles Vim-style
/// motions and counts, Numbers handles session/window addresses, and Search
/// sends typed characters to the fuzzy filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputMode {
    Search,
    Keys,
    Numbers,
}

impl InputMode {
    pub(crate) fn toggled(self) -> Self {
        match self {
            InputMode::Keys => InputMode::Numbers,
            InputMode::Numbers => InputMode::Search,
            InputMode::Search => InputMode::Keys,
        }
    }
}

pub(crate) fn parse_input_mode(value: &str) -> Option<InputMode> {
    match value {
        "search" => Some(InputMode::Search),
        "navigate" | "navigation" | "keys" | "vim" => Some(InputMode::Keys),
        "numbers" | "number" | "numeric" => Some(InputMode::Numbers),
        _ => None,
    }
}

pub(crate) fn format_input_mode(input: InputMode) -> &'static str {
    match input {
        InputMode::Search => "search",
        InputMode::Keys => "keys",
        InputMode::Numbers => "numbers",
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PromptKind {
    RenameWindow { window_id: String },
    NewWindow { session_name: String },
    NewSession,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PromptState {
    pub(crate) kind: PromptKind,
    pub(crate) input: String,
    pub(crate) cursor: usize,
}

impl PromptState {
    pub(crate) fn new(kind: PromptKind) -> Self {
        Self {
            kind,
            input: String::new(),
            cursor: 0,
        }
    }

    pub(crate) fn with_input(kind: PromptKind, input: String) -> Self {
        let cursor = input.chars().count();
        Self {
            kind,
            input,
            cursor,
        }
    }

    pub(crate) fn title(&self) -> &'static str {
        match self.kind {
            PromptKind::RenameWindow { .. } => "Rename window",
            PromptKind::NewWindow { .. } => "New window name",
            PromptKind::NewSession => "New session name",
        }
    }

    fn submit(&self) -> Option<SwitcherAction> {
        let name = self.input.trim();
        if name.is_empty() {
            return None;
        }

        match &self.kind {
            PromptKind::RenameWindow { window_id } => Some(SwitcherAction::RenameWindow {
                window_id: window_id.clone(),
                window_name: name.to_owned(),
            }),
            PromptKind::NewWindow { session_name } => Some(SwitcherAction::NewWindow {
                session_name: session_name.clone(),
                window_name: name.to_owned(),
            }),
            PromptKind::NewSession => Some(SwitcherAction::NewSession {
                session_name: name.to_owned(),
            }),
        }
    }
}

/// Feeds one key into the prompt. `Some(Some(action))` submits, `Some(None)`
/// cancels, `None` keeps editing.
pub(crate) fn handle_prompt_key(
    prompt: &mut PromptState,
    key: KeyEvent,
) -> Option<Option<SwitcherAction>> {
    match key.code {
        KeyCode::Esc => Some(None),
        KeyCode::Enter => prompt.submit().map(Some),
        KeyCode::Backspace => {
            if prompt.cursor > 0 {
                let start = char_byte_index(&prompt.input, prompt.cursor - 1);
                let end = char_byte_index(&prompt.input, prompt.cursor);
                prompt.input.replace_range(start..end, "");
                prompt.cursor -= 1;
            }
            None
        }
        KeyCode::Delete => {
            if prompt.cursor < prompt.input.chars().count() {
                let start = char_byte_index(&prompt.input, prompt.cursor);
                let end = char_byte_index(&prompt.input, prompt.cursor + 1);
                prompt.input.replace_range(start..end, "");
            }
            None
        }
        KeyCode::Left => {
            prompt.cursor = prompt.cursor.saturating_sub(1);
            None
        }
        KeyCode::Right => {
            prompt.cursor = prompt
                .cursor
                .saturating_add(1)
                .min(prompt.input.chars().count());
            None
        }
        KeyCode::Home => {
            prompt.cursor = 0;
            None
        }
        KeyCode::End => {
            prompt.cursor = prompt.input.chars().count();
            None
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let byte_index = char_byte_index(&prompt.input, prompt.cursor);
            prompt.input.insert(byte_index, ch);
            prompt.cursor += 1;
            None
        }
        _ => None,
    }
}

pub(crate) fn char_byte_index(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or(value.len())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GridState {
    pub selected_row: usize,
    pub selected_column: usize,
    pub preferred_column: usize,
    pub row_offset: usize,
}

impl GridState {
    pub fn new() -> Self {
        Self {
            selected_row: 0,
            selected_column: 0,
            preferred_column: 0,
            row_offset: 0,
        }
    }

    pub fn selected_card<'a>(&self, sessions: &'a [SessionGroup]) -> Option<&'a WindowCard> {
        sessions
            .get(self.selected_row)
            .and_then(|session| session.cards.get(self.selected_column))
    }

    pub fn for_window_id(sessions: &[SessionGroup], window_id: &str) -> Self {
        let mut state = Self::new();

        for (row, session) in sessions.iter().enumerate() {
            if let Some(column) = session
                .cards
                .iter()
                .position(|card| card.window_id == window_id)
            {
                state.selected_row = row;
                state.selected_column = column;
                state.preferred_column = column;
                return state;
            }
        }

        state
    }
}

impl Default for GridState {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn select_key_action(
    key: KeyEvent,
    state: &GridState,
    sessions: &[SessionGroup],
) -> Option<SwitcherAction> {
    match key.code {
        KeyCode::Enter => state
            .selected_card(sessions)
            .cloned()
            .map(SwitcherAction::Select),
        _ => None,
    }
}

pub(crate) fn push_numbered_digit(input: &mut String, ch: char) -> bool {
    if !ch.is_ascii_digit() {
        return false;
    }
    let segment_is_empty = input.is_empty() || input.ends_with(',');
    if ch == '0' && segment_is_empty {
        return false;
    }
    input.push(ch);
    true
}

pub(crate) fn numbered_session_index(input: &str, sessions: &[SessionGroup]) -> Option<usize> {
    let session_number = input.split_once(',').map_or(input, |(session, _)| session);
    session_number
        .parse::<usize>()
        .ok()?
        .checked_sub(1)
        .filter(|&index| index < sessions.len())
}

pub(crate) fn accept_numbered_session(input: &mut String, sessions: &[SessionGroup]) -> bool {
    if input.contains(',') || numbered_session_index(input, sessions).is_none() {
        return false;
    }
    input.push(',');
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NumberedOpen {
    Never,
    Unambiguous,
    Force,
}

pub(crate) fn sync_numbered_selection(
    input: &str,
    sessions: &[SessionGroup],
    state: &mut GridState,
    open: NumberedOpen,
) -> Option<WindowCard> {
    let session_index = numbered_session_index(input, sessions)?;
    let session = sessions.get(session_index)?;
    let Some((_, window_number)) = input.split_once(',') else {
        state.selected_row = session_index;
        state.selected_column = 0;
        state.preferred_column = 0;
        return None;
    };
    if window_number.is_empty() {
        state.selected_row = session_index;
        state.selected_column = 0;
        state.preferred_column = 0;
        return None;
    }

    let window_index = window_number.parse::<usize>().ok()?.checked_sub(1)?;
    let card = session.cards.get(window_index)?.clone();
    state.selected_row = session_index;
    state.selected_column = window_index;
    state.preferred_column = window_index;

    let has_longer_match = (window_index + 2..=session.cards.len())
        .any(|number| number.to_string().starts_with(window_number));
    match open {
        NumberedOpen::Never => None,
        NumberedOpen::Unambiguous if !has_longer_match => Some(card),
        NumberedOpen::Force => Some(card),
        NumberedOpen::Unambiguous => None,
    }
}

pub(crate) fn push_numbered_choice(
    input: &mut String,
    ch: char,
    sessions: &[SessionGroup],
    state: &mut GridState,
) -> Option<WindowCard> {
    if !input.contains(',') {
        input.clear();
        if push_numbered_digit(input, ch) && accept_numbered_session(input, sessions) {
            sync_numbered_selection(input, sessions, state, NumberedOpen::Never);
        } else {
            input.clear();
        }
        return None;
    }

    if push_numbered_digit(input, ch) {
        sync_numbered_selection(input, sessions, state, NumberedOpen::Unambiguous)
    } else {
        None
    }
}

/// One row of the compact list: a session header or a window line under it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactLine {
    Session {
        session_index: usize,
    },
    Card {
        session_index: usize,
        card_index: usize,
    },
}

fn compact_visible_rows(terminal_height: u16) -> usize {
    terminal_height.saturating_sub(1).max(1) as usize
}

pub fn initial_grid_state(
    sessions: &[SessionGroup],
    current_window_id: Option<&str>,
    terminal_height: u16,
) -> GridState {
    let mut state = current_window_id
        .map(|window_id| GridState::for_window_id(sessions, window_id))
        .unwrap_or_default();

    keep_compact_selection_visible(&mut state, sessions, terminal_height);

    state
}

pub(crate) fn compact_lines(sessions: &[SessionGroup]) -> Vec<CompactLine> {
    sessions
        .iter()
        .enumerate()
        .flat_map(|(session_index, session)| {
            std::iter::once(CompactLine::Session { session_index }).chain(
                (0..session.cards.len()).map(move |card_index| CompactLine::Card {
                    session_index,
                    card_index,
                }),
            )
        })
        .collect()
}

pub fn compact_selected_line_index(sessions: &[SessionGroup], state: &GridState) -> Option<usize> {
    let mut line_index = 0;

    for (session_index, session) in sessions.iter().enumerate() {
        if session_index == state.selected_row {
            if state.selected_column < session.cards.len() {
                return Some(line_index + 1 + state.selected_column);
            }
            return Some(line_index);
        }
        line_index += 1 + session.cards.len();
    }

    None
}

pub(crate) fn compact_card_positions(sessions: &[SessionGroup]) -> Vec<(usize, usize)> {
    sessions
        .iter()
        .enumerate()
        .flat_map(|(session_index, session)| {
            (0..session.cards.len()).map(move |card_index| (session_index, card_index))
        })
        .collect()
}

fn compact_rendered_line_capacity(
    lines: &[CompactLine],
    row_offset: usize,
    visible_rows: usize,
) -> usize {
    if visible_rows == 0 {
        return 0;
    }

    if visible_rows > 1 && matches!(lines.get(row_offset), Some(CompactLine::Card { .. })) {
        visible_rows - 1
    } else {
        visible_rows
    }
}

fn compact_bottom_aligned_offset(
    lines: &[CompactLine],
    selected_line: usize,
    visible_rows: usize,
) -> usize {
    if visible_rows <= 1 {
        return selected_line;
    }

    let mut row_offset = selected_line.saturating_add(1).saturating_sub(visible_rows);
    if matches!(lines.get(row_offset), Some(CompactLine::Card { .. })) {
        row_offset = selected_line
            .saturating_add(1)
            .saturating_sub(visible_rows - 1);
    }
    row_offset
}

pub(crate) fn keep_compact_selection_visible(
    state: &mut GridState,
    sessions: &[SessionGroup],
    terminal_height: u16,
) {
    let Some(selected_line) = compact_selected_line_index(sessions, state) else {
        state.row_offset = 0;
        return;
    };
    let lines = compact_lines(sessions);
    let visible_rows = compact_visible_rows(terminal_height);
    let row_offset = state.row_offset.min(lines.len().saturating_sub(1));
    let visible_capacity = compact_rendered_line_capacity(&lines, row_offset, visible_rows);

    if selected_line < row_offset {
        state.row_offset = selected_line;
    } else if selected_line >= row_offset + visible_capacity {
        state.row_offset = compact_bottom_aligned_offset(&lines, selected_line, visible_rows);
    } else {
        state.row_offset = row_offset;
    }
}

pub(crate) fn push_movement_count(count: &mut Option<usize>, ch: char) -> bool {
    let Some(digit) = ch.to_digit(10).map(|digit| digit as usize) else {
        return false;
    };
    if count.is_none() && digit == 0 {
        return false;
    }

    *count = Some(count.unwrap_or(0).saturating_mul(10).saturating_add(digit));
    true
}

/// Extends a Vim movement count only while it remains a prefix of at least one
/// relative window number. An unmatched digit clears the count so the next
/// digit begins a fresh sequence.
pub(crate) fn push_matching_movement_count(
    count: &mut Option<usize>,
    ch: char,
    state: &GridState,
    sessions: &[SessionGroup],
) -> bool {
    if !push_movement_count(count, ch) {
        return false;
    }

    let positions = compact_card_positions(sessions);
    let Some(selected_position) = positions
        .iter()
        .position(|&(row, column)| row == state.selected_row && column == state.selected_column)
    else {
        *count = None;
        return false;
    };
    let prefix = count.unwrap_or_default().to_string();
    let has_match = positions.iter().enumerate().any(|(position, _)| {
        position != selected_position
            && position
                .abs_diff(selected_position)
                .to_string()
                .starts_with(&prefix)
    });

    if !has_match {
        *count = None;
    }
    has_match
}

pub(crate) fn take_counted_open_motion(
    count: &mut Option<usize>,
    ch: char,
) -> Option<(Direction, usize)> {
    let direction = match ch {
        'j' => Direction::Down,
        'k' => Direction::Up,
        _ => return None,
    };
    Some((direction, count.take()?))
}

pub fn move_compact_selection(
    state: &mut GridState,
    sessions: &[SessionGroup],
    direction: Direction,
    terminal_height: u16,
) {
    move_compact_selection_by(state, sessions, direction, 1, terminal_height);
}

pub(crate) fn move_compact_selection_by(
    state: &mut GridState,
    sessions: &[SessionGroup],
    direction: Direction,
    count: usize,
    terminal_height: u16,
) {
    if sessions.is_empty() {
        *state = GridState::new();
        return;
    }

    match direction {
        Direction::Up | Direction::Down => {
            let positions = compact_card_positions(sessions);
            if positions.is_empty() {
                *state = GridState::new();
                return;
            }
            let current = positions
                .iter()
                .position(|&(row, column)| {
                    row == state.selected_row && column == state.selected_column
                })
                .unwrap_or(0);
            let next = match direction {
                Direction::Up => current.saturating_sub(count),
                Direction::Down => current.saturating_add(count).min(positions.len() - 1),
                Direction::Left | Direction::Right => unreachable!(),
            };
            let (row, column) = positions[next];
            state.selected_row = row;
            state.selected_column = column;
            state.preferred_column = column;
        }
        Direction::Left | Direction::Right => {
            let session_indices: Vec<usize> = sessions
                .iter()
                .enumerate()
                .filter_map(|(index, session)| (!session.cards.is_empty()).then_some(index))
                .collect();
            if session_indices.is_empty() {
                *state = GridState::new();
                return;
            }
            let current = session_indices
                .iter()
                .position(|&index| index == state.selected_row)
                .unwrap_or(0);
            let next = match direction {
                Direction::Left if current == 0 => session_indices.len() - 1,
                Direction::Left => current - 1,
                Direction::Right => (current + 1) % session_indices.len(),
                Direction::Up | Direction::Down => unreachable!(),
            };
            state.selected_row = session_indices[next];
            let max_column = sessions[state.selected_row].cards.len().saturating_sub(1);
            state.selected_column = state.preferred_column.min(max_column);
        }
    }

    keep_compact_selection_visible(state, sessions, terminal_height);
}

pub(crate) fn select_compact_relative(
    state: &mut GridState,
    sessions: &[SessionGroup],
    direction: Direction,
    count: usize,
    terminal_height: u16,
) -> Option<WindowCard> {
    move_compact_selection_by(state, sessions, direction, count, terminal_height);
    state.selected_card(sessions).cloned()
}

pub(crate) fn move_compact_session_edge(
    state: &mut GridState,
    sessions: &[SessionGroup],
    direction: Direction,
    terminal_height: u16,
) {
    let Some(session) = sessions.get(state.selected_row) else {
        return;
    };
    if session.cards.is_empty() {
        return;
    }

    match direction {
        Direction::Down => {
            let last_column = session.cards.len() - 1;
            if state.selected_column < last_column {
                state.selected_column = last_column;
            } else if let Some((next_row, _)) = sessions
                .iter()
                .enumerate()
                .skip(state.selected_row + 1)
                .find(|(_, session)| !session.cards.is_empty())
            {
                state.selected_row = next_row;
                state.selected_column = 0;
            }
        }
        Direction::Up => {
            if state.selected_column > 0 {
                state.selected_column = 0;
            } else if let Some((previous_row, previous_session)) = sessions
                .iter()
                .enumerate()
                .take(state.selected_row)
                .rev()
                .find(|(_, session)| !session.cards.is_empty())
            {
                state.selected_row = previous_row;
                state.selected_column = previous_session.cards.len() - 1;
            }
        }
        Direction::Left | Direction::Right => return,
    }

    state.preferred_column = state.selected_column;
    keep_compact_selection_visible(state, sessions, terminal_height);
}

pub(crate) fn swap_selected_session(
    sessions: &mut [SessionGroup],
    filtered: &mut Vec<SessionGroup>,
    state: &mut GridState,
    query: &str,
    direction: Direction,
    terminal_height: u16,
) -> bool {
    let Some(selected_session) = filtered.get(state.selected_row) else {
        return false;
    };
    let target_row = match direction {
        Direction::Down if state.selected_row + 1 < filtered.len() => state.selected_row + 1,
        Direction::Up if state.selected_row > 0 => state.selected_row - 1,
        _ => return false,
    };
    let Some(target_session) = filtered.get(target_row) else {
        return false;
    };
    let selected_name = selected_session.session_name.clone();
    let target_name = target_session.session_name.clone();
    let selected_window_id = state
        .selected_card(filtered)
        .map(|card| card.window_id.clone());
    let Some(selected_index) = sessions
        .iter()
        .position(|session| session.session_name == selected_name)
    else {
        return false;
    };
    let Some(target_index) = sessions
        .iter()
        .position(|session| session.session_name == target_name)
    else {
        return false;
    };

    sessions.swap(selected_index, target_index);
    *filtered = filter_sessions(sessions, query);
    if let Some(window_id) = selected_window_id {
        *state = GridState::for_window_id(filtered, &window_id);
    } else {
        *state = fallback_grid_state(filtered, target_row, state.selected_column);
    }
    keep_compact_selection_visible(state, filtered, terminal_height);
    true
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WindowReorder {
    Swap {
        source_window_id: String,
        target_window_id: String,
    },
    Move {
        source_window_id: String,
        target_window_id: String,
        before_target: bool,
    },
}

/// Moves the selected window by one visible position. A move inside a session
/// swaps adjacent windows. A move across a session boundary changes the
/// window's session and inserts it next to the boundary window.
pub(crate) fn swap_selected_window(
    sessions: &mut Vec<SessionGroup>,
    filtered: &mut Vec<SessionGroup>,
    state: &mut GridState,
    query: &str,
    direction: Direction,
    terminal_height: u16,
) -> Option<WindowReorder> {
    let selected_session = filtered.get(state.selected_row)?;
    let selected_card = selected_session.cards.get(state.selected_column)?;
    let selected_session_name = selected_session.session_name.clone();
    let selected_id = selected_card.window_id.clone();

    let (target_row, target_column) = match direction {
        Direction::Down if state.selected_column + 1 < selected_session.cards.len() => {
            (state.selected_row, state.selected_column + 1)
        }
        Direction::Down if state.selected_row + 1 < filtered.len() => (state.selected_row + 1, 0),
        Direction::Up if state.selected_column > 0 => {
            (state.selected_row, state.selected_column - 1)
        }
        Direction::Up if state.selected_row > 0 => {
            let target_row = state.selected_row - 1;
            let target_column = filtered.get(target_row)?.cards.len().checked_sub(1)?;
            (target_row, target_column)
        }
        _ => return None,
    };
    let target_session = filtered.get(target_row)?;
    let target_session_name = target_session.session_name.clone();
    let target_id = target_session.cards.get(target_column)?.window_id.clone();

    if selected_session_name != target_session_name {
        let source_session_index = sessions
            .iter()
            .position(|session| session.session_name == selected_session_name)?;
        let source_card_index = sessions[source_session_index]
            .cards
            .iter()
            .position(|card| card.window_id == selected_id)?;
        let mut moved_card = sessions[source_session_index]
            .cards
            .remove(source_card_index);
        moved_card.session_name.clone_from(&target_session_name);
        if sessions[source_session_index].cards.is_empty() {
            sessions.remove(source_session_index);
        }

        let target_session = sessions
            .iter_mut()
            .find(|session| session.session_name == target_session_name)?;
        let target_index = target_session
            .cards
            .iter()
            .position(|card| card.window_id == target_id)?;
        let before_target = direction == Direction::Down;
        let insert_index = if before_target {
            target_index
        } else {
            target_index + 1
        };
        target_session.cards.insert(insert_index, moved_card);

        *filtered = filter_sessions(sessions, query);
        *state = GridState::for_window_id(filtered, &selected_id);
        keep_compact_selection_visible(state, filtered, terminal_height);
        return Some(WindowReorder::Move {
            source_window_id: selected_id,
            target_window_id: target_id,
            before_target,
        });
    }

    let full_session = sessions
        .iter_mut()
        .find(|session| session.session_name == selected_session_name)?;
    let selected_index = full_session
        .cards
        .iter()
        .position(|card| card.window_id == selected_id)?;
    let target_index = full_session
        .cards
        .iter()
        .position(|card| card.window_id == target_id)?;

    // tmux swap-window exchanges the windows' indexes along with their
    // positions; mirror both so the cached cards stay accurate until the
    // next refresh.
    let selected_tmux_index = full_session.cards[selected_index].window_index.clone();
    let target_tmux_index = std::mem::replace(
        &mut full_session.cards[target_index].window_index,
        selected_tmux_index,
    );
    full_session.cards[selected_index].window_index = target_tmux_index;
    full_session.cards.swap(selected_index, target_index);

    *filtered = filter_sessions(sessions, query);
    *state = GridState::for_window_id(filtered, &selected_id);
    keep_compact_selection_visible(state, filtered, terminal_height);
    Some(WindowReorder::Swap {
        source_window_id: selected_id,
        target_window_id: target_id,
    })
}

pub(crate) fn refresh_sessions_from_cards(
    sessions: &mut Vec<SessionGroup>,
    filtered: &mut Vec<SessionGroup>,
    state: &mut GridState,
    cards: Vec<WindowCard>,
    query: &str,
    terminal_height: u16,
) {
    if cards.is_empty() {
        return;
    }

    let selected_window_id = state
        .selected_card(filtered)
        .map(|card| card.window_id.clone());
    let fallback_row = state.selected_row;
    let fallback_column = state.selected_column;
    let fallback_offset = state.row_offset;
    let current_order: Vec<String> = sessions
        .iter()
        .map(|session| session.session_name.clone())
        .collect();
    let mut next_sessions = group_cards_by_session(cards);
    apply_session_order(&mut next_sessions, &current_order);
    if next_sessions.is_empty() {
        return;
    }
    let next_filtered = filter_sessions(&next_sessions, query);

    let mut next_state = if let Some(window_id) = selected_window_id.as_deref() {
        if next_filtered
            .iter()
            .flat_map(|session| session.cards.iter())
            .any(|card| card.window_id == window_id)
        {
            GridState::for_window_id(&next_filtered, window_id)
        } else {
            fallback_grid_state(&next_filtered, fallback_row, fallback_column)
        }
    } else {
        fallback_grid_state(&next_filtered, fallback_row, fallback_column)
    };
    next_state.row_offset = fallback_offset;

    *sessions = next_sessions;
    *filtered = next_filtered;
    *state = next_state;
    keep_compact_selection_visible(state, filtered, terminal_height);
}

/// Removes a closed window from the cached lists and keeps the selection on
/// the next window in that slot, falling back to the previous one at an edge.
pub(crate) fn remove_window_in_place(
    sessions: &mut Vec<SessionGroup>,
    filtered: &mut Vec<SessionGroup>,
    state: &mut GridState,
    window_id: &str,
    query: &str,
    terminal_height: u16,
) {
    let fallback_row = state.selected_row;
    let fallback_column = state.selected_column;
    let fallback_offset = state.row_offset;

    for session in sessions.iter_mut() {
        session.cards.retain(|card| card.window_id != window_id);
    }
    sessions.retain(|session| !session.cards.is_empty());

    *filtered = filter_sessions(sessions, query);
    *state = fallback_grid_state(filtered, fallback_row, fallback_column);
    state.row_offset = fallback_offset;
    keep_compact_selection_visible(state, filtered, terminal_height);
}

/// Updates the cached name of a window across the full and filtered session
/// lists, after a successful tmux rename.
pub(crate) fn rename_card_in_place(
    sessions: &mut [SessionGroup],
    filtered: &mut [SessionGroup],
    window_id: &str,
    window_name: &str,
) {
    for card in sessions
        .iter_mut()
        .chain(filtered.iter_mut())
        .flat_map(|session| session.cards.iter_mut())
        .filter(|card| card.window_id == window_id)
    {
        card.window_name = window_name.to_owned();
    }
}

pub(crate) fn fallback_grid_state(
    sessions: &[SessionGroup],
    row: usize,
    column: usize,
) -> GridState {
    let mut state = GridState::new();
    state.selected_row = row.min(sessions.len().saturating_sub(1));
    let max_column = sessions
        .get(state.selected_row)
        .map(|session| session.cards.len().saturating_sub(1))
        .unwrap_or(0);
    state.selected_column = column.min(max_column);
    state.preferred_column = state.selected_column;
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{AgentKind, AgentState, AgentStatus},
        test_support::test_card,
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn refreshing_sessions_updates_statuses_and_preserves_selected_window() {
        let mut first = test_card("work", "1");
        first.agent_status = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Idle,
            seen: true,
            run_started_at: None,
        };
        let mut selected = test_card("work", "2");
        selected.agent_status = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Idle,
            seen: true,
            run_started_at: None,
        };
        let mut sessions = group_cards_by_session(vec![first.clone(), selected.clone()]);
        let mut filtered = sessions.clone();
        let mut state = GridState::new();
        state.selected_column = 1;
        state.preferred_column = 1;

        selected.agent_status = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Blocked,
            seen: true,
            run_started_at: Some(1000),
        };

        refresh_sessions_from_cards(
            &mut sessions,
            &mut filtered,
            &mut state,
            vec![first, selected],
            "",
            20,
        );

        assert_eq!(state.selected_row, 0);
        assert_eq!(state.selected_column, 1);
        assert_eq!(sessions[0].cards[1].agent_status.state, AgentState::Blocked);
        assert_eq!(filtered, sessions);
    }

    #[test]
    fn refreshing_sessions_keeps_the_active_filter_applied() {
        let mut sessions = group_cards_by_session(vec![test_card("work", "1")]);
        let mut filtered = filter_sessions(&sessions, "ops");
        let mut state = GridState::new();
        assert!(filtered.is_empty());

        let mut ops = test_card("ops", "1");
        ops.window_name = "server".to_owned();
        refresh_sessions_from_cards(
            &mut sessions,
            &mut filtered,
            &mut state,
            vec![test_card("work", "1"), ops],
            "ops",
            20,
        );

        assert_eq!(sessions.len(), 2);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session_name, "ops");
    }

    #[test]
    fn view_mode_toggle_cycles_through_all_three_positions() {
        assert_eq!(ViewMode::Sidebar.toggled(), ViewMode::SidebarRight);
        assert_eq!(ViewMode::SidebarRight.toggled(), ViewMode::Palette);
        assert_eq!(ViewMode::Palette.toggled(), ViewMode::Sidebar);
    }

    #[test]
    fn view_mode_round_trips_through_parse_and_format() {
        for view in [ViewMode::Sidebar, ViewMode::SidebarRight, ViewMode::Palette] {
            assert_eq!(parse_view_mode(format_view_mode(view)), Some(view));
        }
        assert_eq!(parse_view_mode("left"), Some(ViewMode::Sidebar));
        assert_eq!(parse_view_mode("right"), Some(ViewMode::SidebarRight));
        assert_eq!(parse_view_mode("center"), Some(ViewMode::Palette));
    }

    #[test]
    fn input_mode_cycles_between_keys_numbers_and_search() {
        assert_eq!(InputMode::Keys.toggled(), InputMode::Numbers);
        assert_eq!(InputMode::Numbers.toggled(), InputMode::Search);
        assert_eq!(InputMode::Search.toggled(), InputMode::Keys);

        for input in [InputMode::Search, InputMode::Keys, InputMode::Numbers] {
            assert_eq!(parse_input_mode(format_input_mode(input)), Some(input));
        }
        assert_eq!(parse_input_mode("navigate"), Some(InputMode::Keys));
        assert_eq!(parse_input_mode("numeric"), Some(InputMode::Numbers));
    }

    #[test]
    fn numbered_input_opens_session_two_window_five() {
        let groups = group_cards_by_session(vec![
            test_card("alpha", "1"),
            test_card("beta", "1"),
            test_card("beta", "2"),
            test_card("beta", "3"),
            test_card("beta", "4"),
            test_card("beta", "5"),
        ]);
        let mut state = GridState::new();
        let mut input = String::new();

        assert_eq!(
            push_numbered_choice(&mut input, '2', &groups, &mut state),
            None
        );
        assert_eq!(input, "2,");
        assert_eq!((state.selected_row, state.selected_column), (1, 0));

        let selected = push_numbered_choice(&mut input, '5', &groups, &mut state).unwrap();

        assert_eq!(input, "2,5");
        assert_eq!(selected.window_id, "@beta-5");
        assert_eq!((state.selected_row, state.selected_column), (1, 4));
    }

    #[test]
    fn numbered_input_waits_for_ambiguous_multi_digit_window() {
        let cards = (1..=12)
            .map(|window| test_card("work", &window.to_string()))
            .collect();
        let groups = group_cards_by_session(cards);
        let mut state = GridState::new();
        let mut input = "1,".to_owned();

        assert!(push_numbered_digit(&mut input, '1'));
        assert_eq!(
            sync_numbered_selection(&input, &groups, &mut state, NumberedOpen::Unambiguous,),
            None
        );
        assert_eq!(
            sync_numbered_selection(&input, &groups, &mut state, NumberedOpen::Force)
                .unwrap()
                .window_id,
            "@work-1"
        );

        assert!(push_numbered_digit(&mut input, '0'));
        assert_eq!(
            sync_numbered_selection(&input, &groups, &mut state, NumberedOpen::Unambiguous,)
                .unwrap()
                .window_id,
            "@work-10"
        );
    }

    #[test]
    fn prompt_submits_new_window_action_for_selected_session() {
        let mut prompt = PromptState::new(PromptKind::NewWindow {
            session_name: "work".to_owned(),
        });

        for ch in "server".chars() {
            assert_eq!(handle_prompt_key(&mut prompt, key(KeyCode::Char(ch))), None);
        }

        assert_eq!(
            handle_prompt_key(&mut prompt, key(KeyCode::Enter)),
            Some(Some(SwitcherAction::NewWindow {
                session_name: "work".to_owned(),
                window_name: "server".to_owned(),
            }))
        );
    }

    #[test]
    fn prompt_submits_new_session_action_with_same_input_flow() {
        let mut prompt = PromptState::new(PromptKind::NewSession);

        for ch in "ops".chars() {
            assert_eq!(handle_prompt_key(&mut prompt, key(KeyCode::Char(ch))), None);
        }

        assert_eq!(
            handle_prompt_key(&mut prompt, key(KeyCode::Enter)),
            Some(Some(SwitcherAction::NewSession {
                session_name: "ops".to_owned(),
            }))
        );
    }

    #[test]
    fn rename_prompt_starts_with_current_name_and_supports_cursor_editing() {
        let mut prompt = PromptState::with_input(
            PromptKind::RenameWindow {
                window_id: "@42".to_owned(),
            },
            "editor".to_owned(),
        );

        assert_eq!(prompt.title(), "Rename window");
        assert_eq!(handle_prompt_key(&mut prompt, key(KeyCode::Home)), None);
        assert_eq!(handle_prompt_key(&mut prompt, key(KeyCode::Delete)), None);
        assert_eq!(
            handle_prompt_key(&mut prompt, key(KeyCode::Char('E'))),
            None
        );
        assert_eq!(prompt.input, "Editor");

        assert_eq!(
            handle_prompt_key(&mut prompt, key(KeyCode::Enter)),
            Some(Some(SwitcherAction::RenameWindow {
                window_id: "@42".to_owned(),
                window_name: "Editor".to_owned(),
            }))
        );

        let mut canceled = PromptState::with_input(
            PromptKind::RenameWindow {
                window_id: "@42".to_owned(),
            },
            "Editor".to_owned(),
        );
        assert_eq!(
            handle_prompt_key(&mut canceled, key(KeyCode::Esc)),
            Some(None)
        );
    }

    #[test]
    fn prompt_backspace_and_blank_submission_do_not_create_actions() {
        let mut prompt = PromptState::new(PromptKind::NewSession);

        assert_eq!(
            handle_prompt_key(&mut prompt, key(KeyCode::Char('x'))),
            None
        );
        assert_eq!(
            handle_prompt_key(&mut prompt, key(KeyCode::Backspace)),
            None
        );
        assert_eq!(handle_prompt_key(&mut prompt, key(KeyCode::Enter)), None);
        assert_eq!(prompt.input, "");
    }

    #[test]
    fn only_enter_selects_now_that_typing_filters() {
        let cards = vec![test_card("work", "1"), test_card("work", "2")];
        let sessions = group_cards_by_session(cards);
        let mut state = GridState::new();
        state.selected_column = 1;

        assert_eq!(
            select_key_action(key(KeyCode::Enter), &state, &sessions),
            Some(SwitcherAction::Select(test_card("work", "2")))
        );
        // Space is query input, not a select key.
        assert_eq!(
            select_key_action(key(KeyCode::Char(' ')), &state, &sessions),
            None
        );
    }

    #[test]
    fn renaming_in_place_updates_full_and_filtered_lists() {
        let mut sessions = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
        ]);
        let mut filtered = sessions.clone();

        rename_card_in_place(&mut sessions, &mut filtered, "@work-2", "server");

        assert_eq!(sessions[0].cards[1].window_name, "server");
        assert_eq!(filtered[0].cards[1].window_name, "server");
        // Other windows keep their names.
        assert_eq!(sessions[0].cards[0].window_name, "window-1");
        assert_eq!(sessions[1].cards[0].window_name, "window-1");
    }

    #[test]
    fn swapping_session_keeps_selected_window_and_updates_visible_order() {
        let mut groups = group_cards_by_session(vec![
            test_card("alpha", "1"),
            test_card("alpha", "2"),
            test_card("beta", "1"),
            test_card("gamma", "1"),
        ]);
        let mut filtered = groups.clone();
        let mut state = GridState::new();
        state.selected_row = 1;

        assert!(swap_selected_session(
            &mut groups,
            &mut filtered,
            &mut state,
            "",
            Direction::Down,
            10,
        ));

        let names: Vec<&str> = groups
            .iter()
            .map(|session| session.session_name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "gamma", "beta"]);
        assert_eq!(state.selected_card(&filtered).unwrap().session_name, "beta");
        assert_eq!(state.selected_row, 2);
    }

    #[test]
    fn swapping_session_clamps_at_the_list_edges() {
        let mut groups =
            group_cards_by_session(vec![test_card("alpha", "1"), test_card("beta", "1")]);
        let mut filtered = groups.clone();
        let mut state = GridState::new();

        assert!(!swap_selected_session(
            &mut groups,
            &mut filtered,
            &mut state,
            "",
            Direction::Up,
            10,
        ));
        state.selected_row = 1;
        assert!(!swap_selected_session(
            &mut groups,
            &mut filtered,
            &mut state,
            "",
            Direction::Down,
            10,
        ));
    }

    #[test]
    fn swapping_window_moves_it_within_the_session_and_follows_it() {
        let mut groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("work", "3"),
            test_card("ops", "1"),
        ]);
        let mut filtered = groups.clone();
        let mut state = GridState::new();

        assert_eq!(
            swap_selected_window(
                &mut groups,
                &mut filtered,
                &mut state,
                "",
                Direction::Down,
                10,
            ),
            Some(WindowReorder::Swap {
                source_window_id: "@work-1".to_owned(),
                target_window_id: "@work-2".to_owned(),
            })
        );

        let names: Vec<&str> = groups[0]
            .cards
            .iter()
            .map(|card| card.window_name.as_str())
            .collect();
        assert_eq!(names, ["window-2", "window-1", "window-3"]);
        // The selection follows the moved window, and the tmux indexes stay
        // with the slots (as swap-window leaves them).
        assert_eq!(state.selected_column, 1);
        assert_eq!(state.selected_card(&filtered).unwrap().window_id, "@work-1");
        assert_eq!(groups[0].cards[0].window_index, "1");
        assert_eq!(groups[0].cards[1].window_index, "2");
    }

    #[test]
    fn moving_window_crosses_session_edges_and_follows_it() {
        let mut groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
        ]);
        let mut filtered = groups.clone();
        let mut state = GridState::new();

        assert_eq!(
            swap_selected_window(
                &mut groups,
                &mut filtered,
                &mut state,
                "",
                Direction::Up,
                10
            ),
            None
        );
        state.selected_column = 1;
        assert_eq!(
            swap_selected_window(
                &mut groups,
                &mut filtered,
                &mut state,
                "",
                Direction::Down,
                10,
            ),
            Some(WindowReorder::Move {
                source_window_id: "@work-2".to_owned(),
                target_window_id: "@ops-1".to_owned(),
                before_target: true,
            })
        );
        assert_eq!(groups[0].cards.len(), 1);
        assert_eq!(groups[1].cards.len(), 2);
        assert_eq!(groups[1].cards[0].window_id, "@work-2");
        assert_eq!(groups[1].cards[0].session_name, "ops");
        assert_eq!(state.selected_row, 1);
        assert_eq!(state.selected_column, 0);

        assert_eq!(
            swap_selected_window(
                &mut groups,
                &mut filtered,
                &mut state,
                "",
                Direction::Up,
                10,
            ),
            Some(WindowReorder::Move {
                source_window_id: "@work-2".to_owned(),
                target_window_id: "@work-1".to_owned(),
                before_target: false,
            })
        );
        assert_eq!(groups[0].cards.len(), 2);
        assert_eq!(groups[0].cards[1].window_id, "@work-2");
        assert_eq!(groups[0].cards[1].session_name, "work");
        assert_eq!(state.selected_row, 0);
        assert_eq!(state.selected_column, 1);
    }

    #[test]
    fn moving_the_only_window_removes_the_empty_source_session() {
        let mut groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("ops", "1"),
            test_card("ops", "2"),
        ]);
        let mut filtered = groups.clone();
        let mut state = GridState::new();

        assert!(matches!(
            swap_selected_window(
                &mut groups,
                &mut filtered,
                &mut state,
                "",
                Direction::Down,
                10,
            ),
            Some(WindowReorder::Move { .. })
        ));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].session_name, "ops");
        assert_eq!(groups[0].cards[0].window_id, "@work-1");
    }

    #[test]
    fn compact_navigation_uses_terminal_height_for_vertical_viewport() {
        let groups = group_cards_by_session(vec![
            test_card("one", "1"),
            test_card("two", "1"),
            test_card("three", "1"),
            test_card("four", "1"),
            test_card("five", "1"),
        ]);
        let mut state = GridState::new();

        move_compact_selection(&mut state, &groups, Direction::Down, 5);
        move_compact_selection(&mut state, &groups, Direction::Down, 5);
        move_compact_selection(&mut state, &groups, Direction::Down, 5);
        assert_eq!(state.selected_row, 3);
        assert_eq!(state.row_offset, 4);

        move_compact_selection(&mut state, &groups, Direction::Down, 5);
        assert_eq!(state.selected_row, 4);
        assert_eq!(state.row_offset, 6);
    }

    #[test]
    fn compact_navigation_moves_up_and_down_between_window_rows() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
            test_card("ops", "2"),
        ]);
        let mut state = GridState::new();

        move_compact_selection(&mut state, &groups, Direction::Down, 6);
        assert_eq!((state.selected_row, state.selected_column), (0, 1));

        move_compact_selection(&mut state, &groups, Direction::Down, 6);
        assert_eq!((state.selected_row, state.selected_column), (1, 0));

        move_compact_selection(&mut state, &groups, Direction::Up, 6);
        assert_eq!((state.selected_row, state.selected_column), (0, 1));
    }

    #[test]
    fn compact_navigation_jumps_left_and_right_between_sessions() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
            test_card("ops", "2"),
            test_card("db", "1"),
        ]);
        let mut state = GridState::new();
        state.selected_column = 1;
        state.preferred_column = 1;

        move_compact_selection(&mut state, &groups, Direction::Right, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 1));

        move_compact_selection(&mut state, &groups, Direction::Right, 8);
        assert_eq!((state.selected_row, state.selected_column), (2, 0));
        assert_eq!(state.preferred_column, 1);

        move_compact_selection(&mut state, &groups, Direction::Left, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 1));
    }

    #[test]
    fn compact_session_edge_navigation_walks_first_and_last_items() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("work", "3"),
            test_card("ops", "1"),
            test_card("ops", "2"),
            test_card("db", "1"),
        ]);
        let mut state = GridState::new();
        state.selected_column = 1;

        move_compact_session_edge(&mut state, &groups, Direction::Down, 8);
        assert_eq!((state.selected_row, state.selected_column), (0, 2));

        move_compact_session_edge(&mut state, &groups, Direction::Down, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 0));

        move_compact_session_edge(&mut state, &groups, Direction::Down, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 1));

        move_compact_session_edge(&mut state, &groups, Direction::Up, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 0));

        move_compact_session_edge(&mut state, &groups, Direction::Up, 8);
        assert_eq!((state.selected_row, state.selected_column), (0, 2));

        move_compact_session_edge(&mut state, &groups, Direction::Up, 8);
        assert_eq!((state.selected_row, state.selected_column), (0, 0));
    }

    #[test]
    fn compact_session_edge_navigation_clamps_at_list_ends() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
        ]);
        let mut state = GridState::new();

        move_compact_session_edge(&mut state, &groups, Direction::Up, 8);
        assert_eq!((state.selected_row, state.selected_column), (0, 0));

        state.selected_row = 1;
        move_compact_session_edge(&mut state, &groups, Direction::Down, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 0));
    }

    #[test]
    fn keys_mode_count_moves_across_sessions_like_vim() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
            test_card("ops", "2"),
            test_card("ops", "3"),
        ]);
        let mut state = GridState::new();
        let mut count = None;

        assert!(push_movement_count(&mut count, '3'));
        move_compact_selection_by(
            &mut state,
            &groups,
            Direction::Down,
            count.take().unwrap(),
            8,
        );

        assert_eq!((state.selected_row, state.selected_column), (1, 1));
        assert_eq!(count, None);
    }

    #[test]
    fn keys_mode_count_accepts_multiple_digits_and_clamps_at_the_edge() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
        ]);
        let mut state = GridState::new();
        let mut count = None;

        assert!(push_movement_count(&mut count, '1'));
        assert!(push_movement_count(&mut count, '0'));
        assert_eq!(count, Some(10));
        move_compact_selection_by(&mut state, &groups, Direction::Down, count.unwrap(), 8);

        assert_eq!((state.selected_row, state.selected_column), (1, 0));

        let mut leading_zero = None;
        assert!(!push_movement_count(&mut leading_zero, '0'));
        assert_eq!(leading_zero, None);
    }

    #[test]
    fn unmatched_keys_mode_count_resets_before_the_next_sequence() {
        let cards = (1..=13)
            .map(|index| test_card("work", &index.to_string()))
            .collect();
        let groups = group_cards_by_session(cards);
        let mut state = GridState::new();
        state.selected_column = 11;
        let mut count = None;

        assert!(push_matching_movement_count(
            &mut count, '1', &state, &groups
        ));
        assert_eq!(count, Some(1));
        assert!(push_matching_movement_count(
            &mut count, '1', &state, &groups
        ));
        assert_eq!(count, Some(11));

        assert!(!push_matching_movement_count(
            &mut count, '1', &state, &groups
        ));
        assert_eq!(count, None);

        assert!(push_matching_movement_count(
            &mut count, '1', &state, &groups
        ));
        assert_eq!(count, Some(1));
    }

    #[test]
    fn counted_lowercase_motion_selects_the_relative_target_for_opening() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("work", "3"),
            test_card("ops", "1"),
            test_card("ops", "2"),
        ]);
        let mut state = GridState::new();
        state.selected_column = 1;

        let mut down_count = Some(2);
        assert_eq!(
            take_counted_open_motion(&mut down_count, 'j'),
            Some((Direction::Down, 2))
        );
        assert_eq!(down_count, None);

        let down = select_compact_relative(&mut state, &groups, Direction::Down, 2, 8).unwrap();
        assert_eq!(down.window_id, "@ops-1");
        assert_eq!((state.selected_row, state.selected_column), (1, 0));

        let mut up_count = Some(2);
        let (up_direction, up_count) = take_counted_open_motion(&mut up_count, 'k').unwrap();
        let up = select_compact_relative(&mut state, &groups, up_direction, up_count, 8).unwrap();
        assert_eq!(up.window_id, "@work-2");
        assert_eq!((state.selected_row, state.selected_column), (0, 1));

        let mut no_count = None;
        assert_eq!(take_counted_open_motion(&mut no_count, 'k'), None);
    }

    #[test]
    fn compact_selected_line_index_counts_session_headers() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
        ]);
        let mut state = GridState::new();
        state.selected_row = 1;

        assert_eq!(compact_selected_line_index(&groups, &state), Some(4));
    }

    #[test]
    fn compact_scroll_preserves_context_when_selection_is_visible() {
        let groups = group_cards_by_session(vec![
            test_card("alpha", "1"),
            test_card("alpha", "2"),
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("work", "3"),
        ]);
        let mut state = GridState::new();
        state.selected_row = 1;
        state.row_offset = 1;

        keep_compact_selection_visible(&mut state, &groups, 6);

        assert_eq!(state.row_offset, 1);
    }

    #[test]
    fn launch_state_uses_restored_compact_viewport() {
        let groups = group_cards_by_session(vec![
            test_card("one", "1"),
            test_card("two", "1"),
            test_card("three", "1"),
            test_card("four", "1"),
            test_card("five", "1"),
        ]);

        let state = initial_grid_state(&groups, Some("@five-1"), 5);

        assert_eq!(state.selected_row, 4);
        assert_eq!(state.row_offset, 6);
    }
}
