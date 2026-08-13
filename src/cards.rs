//! Building the window cards the switcher displays: joining tmux windows with
//! their panes, rolling pane statuses up per window, grouping cards by session,
//! and persisting the user's session order.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Result;

use crate::{
    daemon::ensure_status_daemon,
    detect::detect_agent_from_process_name,
    model::{AgentKind, AgentState, AgentStatus, SessionGroup, TmuxPane, TmuxWindow, WindowCard},
    tmux::{parse_panes, parse_windows, tmux_output, tmux_status},
};

const SESSION_ORDER_OPTION: &str = "@tmux_agent_sidebar_order";

pub fn codex_unread_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
            PathBuf::from(home).join(".local/state")
        })
        .join("tmux-codex-unread")
}

pub fn codex_unread_file(state_dir: &Path, pane_id: &str) -> PathBuf {
    state_dir.join(format!("{}.json", pane_id.trim_start_matches('%')))
}

pub fn build_cards(
    windows: &[TmuxWindow],
    panes: &[TmuxPane],
    unread_dir: &Path,
) -> Vec<WindowCard> {
    build_cards_with_previews(windows, panes, &HashMap::new(), unread_dir)
}

pub fn build_cards_with_previews(
    windows: &[TmuxWindow],
    panes: &[TmuxPane],
    previews: &HashMap<String, Vec<String>>,
    unread_dir: &Path,
) -> Vec<WindowCard> {
    windows
        .iter()
        .filter_map(|window| {
            let active_pane = panes
                .iter()
                .find(|pane| pane.window_id == window.window_id && pane.pane_active)
                .or_else(|| panes.iter().find(|pane| pane.window_id == window.window_id))?;

            let window_panes: Vec<&TmuxPane> = panes
                .iter()
                .filter(|pane| pane.window_id == window.window_id)
                .collect();
            let codex_unread = window_panes
                .iter()
                .any(|pane| codex_unread_file(unread_dir, &pane.pane_id).exists());
            let agent_status =
                rollup_agent_status(window_panes.iter().map(|pane| pane.agent_status));

            Some(WindowCard {
                window_id: window.window_id.clone(),
                target_pane_id: active_pane.pane_id.clone(),
                session_name: window.session_name.clone(),
                window_index: window.window_index.clone(),
                window_name: window.window_name.clone(),
                window_flags: window.window_flags.clone(),
                command: active_pane.pane_current_command.clone(),
                path: active_pane.pane_current_path.clone(),
                title: active_pane.pane_title.clone(),
                preview: previews
                    .get(&active_pane.pane_id)
                    .cloned()
                    .unwrap_or_default(),
                codex_unread: codex_unread || agent_status.is_done(),
                agent_status,
            })
        })
        .collect()
}

pub(crate) fn rollup_agent_status(statuses: impl Iterator<Item = AgentStatus>) -> AgentStatus {
    let mut best = AgentStatus::unknown();
    let mut best_priority = 0;

    for status in statuses {
        let priority = agent_status_priority(status);
        if priority > best_priority {
            best = status;
            best_priority = priority;
        }
    }

    best
}

fn agent_status_priority(status: AgentStatus) -> u8 {
    match status.state {
        AgentState::Blocked => 5,
        AgentState::Idle if !status.seen => 4,
        AgentState::Working => 3,
        AgentState::Idle => 2,
        AgentState::Unknown => 1,
    }
}

pub fn group_cards_by_session(cards: Vec<WindowCard>) -> Vec<SessionGroup> {
    let mut sessions: Vec<SessionGroup> = Vec::new();

    for card in cards {
        if let Some(session) = sessions
            .iter_mut()
            .find(|session| session.session_name == card.session_name)
        {
            session.cards.push(card);
        } else {
            sessions.push(SessionGroup {
                session_name: card.session_name.clone(),
                cards: vec![card],
            });
        }
    }

    sessions
}

pub(crate) fn apply_session_order(sessions: &mut [SessionGroup], order: &[String]) {
    let positions: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();
    sessions.sort_by_key(|session| {
        positions
            .get(session.session_name.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
}

pub(crate) fn load_session_order() -> Result<Vec<String>> {
    let output = tmux_output(&[
        "list-sessions",
        "-F",
        "#{session_name}\t#{@tmux_agent_sidebar_order}",
    ])?;
    let mut rows: Vec<(usize, Option<usize>, String)> = output
        .lines()
        .enumerate()
        .filter_map(|(default_index, line)| {
            let (session_name, rank) = line.split_once('\t')?;
            Some((
                default_index,
                rank.parse::<usize>().ok(),
                session_name.to_owned(),
            ))
        })
        .collect();
    rows.sort_by_key(|(default_index, rank, _)| {
        (rank.is_none(), rank.unwrap_or(usize::MAX), *default_index)
    });
    Ok(rows
        .into_iter()
        .map(|(_, _, session_name)| session_name)
        .collect())
}

pub(crate) fn persist_session_order(sessions: &[SessionGroup]) {
    for (rank, session) in sessions.iter().enumerate() {
        let _ = tmux_status(Command::new("tmux").args([
            "set-option",
            "-q",
            "-t",
            &session.session_name,
            SESSION_ORDER_OPTION,
            &rank.to_string(),
        ]));
    }
}

/// The process label shown for a card (and matched when filtering): agent
/// binaries are normalized to their plain names, anything else shows as-is.
pub(crate) fn compact_tab_process_text(card: &WindowCard) -> String {
    match detect_agent_from_process_name(&card.command) {
        Some(AgentKind::Codex) => "codex".to_owned(),
        Some(AgentKind::Claude) => "claude".to_owned(),
        Some(AgentKind::OpenCode) => "opencode".to_owned(),
        None => card.command.clone(),
    }
}

pub fn load_cards() -> Result<Vec<WindowCard>> {
    let _ = ensure_status_daemon();
    let windows = parse_windows(&tmux_output(&[
        "list-windows",
        "-a",
        "-F",
        "#{window_id}\t#{session_name}\t#{window_index}\t#{window_name}\t#{window_flags}",
    ])?)?;
    let panes = parse_panes(&tmux_output(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}\t#{window_id}\t#{pane_active}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_title}\t#{pane_pid}\t#{@tmux_agent_sidebar_agent}\t#{@tmux_agent_sidebar_state}\t#{@tmux_agent_sidebar_seen}\t#{@tmux_agent_sidebar_run_started_at}\t#{@codex_status_state}\t#{@codex_status_unread}",
    ])?)?;
    Ok(build_cards_with_previews(
        &windows,
        &panes,
        &HashMap::new(),
        &codex_unread_dir(),
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_support::test_card;

    #[test]
    fn codex_unread_file_strips_tmux_pane_prefix() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            codex_unread_file(dir.path(), "%42"),
            dir.path().join("42.json")
        );
    }

    #[test]
    fn builds_cards_from_windows_and_active_panes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("2.json"), "{}").unwrap();

        let windows = parse_windows("@1\twork\t1\teditor\t*\n@2\twork\t2\tagents\t-\n").unwrap();
        let panes = parse_panes(
            "%1\t@1\t1\tnvim\t/Users/example/project\teditor\n%2\t@2\t1\tcodex\t/tmp\tagent\n",
        )
        .unwrap();
        let cards = build_cards(&windows, &panes, dir.path());

        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].target_pane_id, "%1");
        assert!(!cards[0].codex_unread);
        assert_eq!(cards[1].target_pane_id, "%2");
        assert!(cards[1].codex_unread);
    }

    #[test]
    fn rolls_window_status_up_from_panes() {
        let dir = tempfile::tempdir().unwrap();
        let windows = parse_windows("@1\twork\t1\tagents\t*\n").unwrap();
        let panes = parse_panes(
            "%1\t@1\t1\tzsh\t/tmp\t\t11\tcodex\tworking\t1\n%2\t@1\t0\tzsh\t/tmp\t\t12\tclaude\tblocked\t1\n",
        )
        .unwrap();
        let cards = build_cards(&windows, &panes, dir.path());

        assert_eq!(cards[0].agent_status.state, AgentState::Blocked);
    }

    #[test]
    fn groups_cards_by_session_in_window_order() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
            test_card("work", "3"),
        ]);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].session_name, "work");
        assert_eq!(groups[0].cards.len(), 3);
        assert_eq!(groups[1].session_name, "ops");
        assert_eq!(groups[1].cards.len(), 1);
    }

    #[test]
    fn persisted_session_order_overrides_tmux_order_and_appends_new_sessions() {
        let mut groups = group_cards_by_session(vec![
            test_card("alpha", "1"),
            test_card("beta", "1"),
            test_card("new", "1"),
        ]);

        apply_session_order(&mut groups, &["beta".to_owned(), "alpha".to_owned()]);

        let names: Vec<&str> = groups
            .iter()
            .map(|session| session.session_name.as_str())
            .collect();
        assert_eq!(names, ["beta", "alpha", "new"]);
    }
}
