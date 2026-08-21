//! The tmux subprocess layer: running tmux commands, parsing their
//! tab-separated output, and executing switcher actions (select / rename /
//! create windows and sessions).

use std::{
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};

use crate::{
    cards::{codex_unread_dir, codex_unread_file},
    daemon::STATUS_SEEN_OPTION,
    model::{
        parse_agent_kind, parse_agent_state, AgentKind, AgentState, AgentStatus, SwitcherAction,
        TmuxPane, TmuxWindow,
    },
};

const CODEX_STATUS_UNREAD_OPTION: &str = "@codex_status_unread";

pub fn parse_windows(output: &str) -> Result<Vec<TmuxWindow>> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields = split_tmux_fields(line, 5)?;
            Ok(TmuxWindow {
                window_id: fields[0].to_owned(),
                session_name: fields[1].to_owned(),
                window_index: fields[2].to_owned(),
                window_name: fields[3].to_owned(),
                window_flags: fields[4].to_owned(),
            })
        })
        .collect()
}

pub fn parse_panes(output: &str) -> Result<Vec<TmuxPane>> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields = split_tmux_fields(line, 6)?;
            let pane_pid = fields.get(6).and_then(|value| value.parse().ok());
            let cached_status = parse_cached_agent_status(
                fields.get(7).copied().unwrap_or_default(),
                fields.get(8).copied().unwrap_or_default(),
                fields.get(9).copied().unwrap_or_default(),
                fields.get(10).copied().unwrap_or_default(),
            )
            .or_else(|| {
                parse_codex_hook_status(
                    fields.get(11).copied().unwrap_or_default(),
                    fields.get(12).copied().unwrap_or_default(),
                )
            })
            .or_else(|| {
                parse_codex_hook_status(
                    fields.get(10).copied().unwrap_or_default(),
                    fields.get(11).copied().unwrap_or_default(),
                )
            })
            .unwrap_or_else(AgentStatus::unknown);

            Ok(TmuxPane {
                pane_id: fields[0].to_owned(),
                window_id: fields[1].to_owned(),
                pane_active: fields[2] == "1",
                pane_current_command: fields[3].to_owned(),
                pane_current_path: fields[4].to_owned(),
                pane_title: fields[5].to_owned(),
                pane_pid,
                agent_status: cached_status,
            })
        })
        .collect()
}

pub(crate) fn split_tmux_fields(line: &str, expected: usize) -> Result<Vec<&str>> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() < expected {
        return Err(anyhow!(
            "expected at least {expected} tab-separated fields, got {line:?}"
        ));
    }
    Ok(fields)
}

fn parse_cached_agent_status(
    agent: &str,
    state: &str,
    seen: &str,
    run_started_at: &str,
) -> Option<AgentStatus> {
    let state = parse_agent_state(state)?;
    Some(AgentStatus {
        agent: parse_agent_kind(agent),
        state,
        seen: seen != "0",
        run_started_at: run_started_at.parse().ok(),
    })
}

fn parse_codex_hook_status(state: &str, unread: &str) -> Option<AgentStatus> {
    match state {
        "blocked" => Some(AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Blocked,
            seen: true,
            run_started_at: None,
        }),
        "busy" => Some(AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Working,
            seen: true,
            run_started_at: None,
        }),
        "ready" if unread == "1" => Some(AgentStatus::done(Some(AgentKind::Codex))),
        "ready" | "idle" => Some(AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Idle,
            seen: true,
            run_started_at: None,
        }),
        _ => None,
    }
}

pub fn current_window_id() -> Option<String> {
    if let Some(window_id) = env_tmux_value("TMUX_AGENT_SIDEBAR_CURRENT") {
        return Some(window_id);
    }

    tmux_output(&["display-message", "-p", "#{window_id}"])
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub fn env_tmux_value(name: &str) -> Option<String> {
    std::env::var_os(name)
        .map(|value| value.to_string_lossy().trim().to_owned())
        .filter(|value| !value.is_empty() && !value.contains("#{"))
}

pub fn select_card(card: &crate::model::WindowCard) -> Result<()> {
    tmux_status(Command::new("tmux").args(["switch-client", "-t", &card.session_name]))?;
    tmux_status(Command::new("tmux").args(["select-window", "-t", &card.window_id]))?;
    tmux_status(Command::new("tmux").args(["select-pane", "-t", &card.target_pane_id]))?;
    if mark_window_read(&card.window_id).is_err() {
        clear_unread_for_pane(&card.target_pane_id);
    }
    Ok(())
}

pub fn execute_action(action: SwitcherAction) -> Result<()> {
    match action {
        SwitcherAction::Select(card) => select_card(&card),
        SwitcherAction::RenameWindow {
            window_id,
            window_name,
        } => rename_window(&window_id, &window_name),
        SwitcherAction::NewWindow {
            session_name,
            window_name,
        } => create_window(&session_name, &window_name),
        SwitcherAction::NewSession { session_name } => create_session(&session_name),
    }
}

pub fn rename_window(window_id: &str, window_name: &str) -> Result<()> {
    tmux_status(Command::new("tmux").args(["rename-window", "-t", window_id, window_name]))
}

pub fn kill_window(window_id: &str) -> Result<()> {
    tmux_status(Command::new("tmux").args(["kill-window", "-t", window_id]))
}

/// Swaps two windows' positions. `-d` keeps each session's current window
/// current, so reordering from the popup never changes what is focused.
pub fn swap_windows(source_window_id: &str, target_window_id: &str) -> Result<()> {
    tmux_status(Command::new("tmux").args([
        "swap-window",
        "-d",
        "-s",
        source_window_id,
        "-t",
        target_window_id,
    ]))
}

pub fn move_window(
    source_window_id: &str,
    target_window_id: &str,
    before_target: bool,
) -> Result<()> {
    let position = if before_target { "-b" } else { "-a" };
    tmux_status(Command::new("tmux").args([
        "move-window",
        "-d",
        position,
        "-s",
        source_window_id,
        "-t",
        target_window_id,
    ]))
}

pub fn create_window(session_name: &str, window_name: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args([
            "new-window",
            "-P",
            "-F",
            "#{window_id}",
            "-t",
            &format!("{session_name}:"),
            "-n",
            window_name,
        ])
        .output()
        .context("failed to create tmux window")?;

    if !output.status.success() {
        return Err(anyhow!(
            "tmux new-window failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let window_id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    tmux_status(Command::new("tmux").args(["switch-client", "-t", session_name]))?;
    if !window_id.is_empty() {
        tmux_status(Command::new("tmux").args(["select-window", "-t", &window_id]))?;
    }
    Ok(())
}

pub fn create_session(session_name: &str) -> Result<()> {
    tmux_status(Command::new("tmux").args(["new-session", "-d", "-s", session_name]))?;
    tmux_status(Command::new("tmux").args(["switch-client", "-t", session_name]))
}

pub fn clear_unread_for_pane(pane_id: &str) {
    let _ = clear_unread_for_pane_in(&codex_unread_dir(), pane_id);
}

pub fn mark_unread_for_pane(pane_id: &str) -> Result<()> {
    mark_unread_for_pane_in(&codex_unread_dir(), pane_id)
}

fn mark_unread_for_pane_in(state_dir: &std::path::Path, pane_id: &str) -> Result<()> {
    fs::create_dir_all(state_dir).context("failed to create unread state directory")?;
    fs::write(codex_unread_file(state_dir, pane_id), b"{}\n").context("failed to mark pane unread")
}

pub fn mark_window_read(window_id: &str) -> Result<()> {
    let output = tmux_output(&["list-panes", "-t", window_id, "-F", "#{pane_id}"])?;
    for pane_id in output.lines().filter(|line| !line.trim().is_empty()) {
        clear_unread_for_pane_in(&codex_unread_dir(), pane_id)?;
        set_pane_option(pane_id, STATUS_SEEN_OPTION, "1")?;
        set_pane_option(pane_id, CODEX_STATUS_UNREAD_OPTION, "0")?;
    }
    Ok(())
}

fn clear_unread_for_pane_in(state_dir: &std::path::Path, pane_id: &str) -> Result<()> {
    match fs::remove_file(codex_unread_file(state_dir, pane_id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to clear pane unread marker"),
    }
}

pub(crate) fn set_pane_option(pane_id: &str, option: &str, value: &str) -> Result<()> {
    tmux_status(Command::new("tmux").args(["set-option", "-p", "-q", "-t", pane_id, option, value]))
}

pub(crate) fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn tmux_output(args: &[&str]) -> Result<String> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .with_context(|| format!("failed to run tmux {}", args.join(" ")))?;

    if !output.status.success() {
        return Err(anyhow!(
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) fn tmux_status(command: &mut Command) -> Result<()> {
    let status = command.status().context("failed to run tmux command")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("tmux command exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marking_a_pane_unread_creates_its_marker() {
        let dir = tempfile::tempdir().unwrap();

        mark_unread_for_pane_in(dir.path(), "%42").unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("42.json")).unwrap(),
            "{}\n"
        );
    }

    #[test]
    fn clearing_a_pane_unread_removes_its_marker_and_allows_missing_markers() {
        let dir = tempfile::tempdir().unwrap();
        mark_unread_for_pane_in(dir.path(), "%42").unwrap();

        clear_unread_for_pane_in(dir.path(), "%42").unwrap();
        clear_unread_for_pane_in(dir.path(), "%42").unwrap();

        assert!(!dir.path().join("42.json").exists());
    }

    #[test]
    fn parses_tmux_window_rows() {
        let windows = parse_windows("@1\twork\t1\teditor\t*\n@2\twork\t2\tagents\t-\n").unwrap();

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].window_id, "@1");
        assert_eq!(windows[0].session_name, "work");
        assert_eq!(windows[0].window_index, "1");
        assert_eq!(windows[0].window_name, "editor");
        assert_eq!(windows[0].window_flags, "*");
    }

    #[test]
    fn parses_pane_rows_with_empty_fields() {
        let panes =
            parse_panes("%1\t@1\t1\tnvim\t/Users/example\t\n%2\t@1\t0\tcodex\t/tmp\tagent\n")
                .unwrap();

        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%1");
        assert_eq!(panes[0].window_id, "@1");
        assert!(panes[0].pane_active);
        assert_eq!(panes[0].pane_title, "");
        assert_eq!(panes[1].pane_title, "agent");
    }

    #[test]
    fn parses_cached_agent_status_from_pane_rows() {
        let panes = parse_panes("%1\t@1\t1\tcodex\t/tmp\t⠋ working\t123\tcodex\tworking\t1\t1000\t\t\n%2\t@1\t0\tzsh\t/tmp\t\t124\tclaude\tidle\t0\t\t\t\n%3\t@2\t1\topencode\t/tmp\tOC | task\t125\topencode\tblocked\t1\t2000\t\t\n").unwrap();

        assert_eq!(panes[0].pane_pid, Some(123));
        assert_eq!(
            panes[0].agent_status,
            AgentStatus {
                agent: Some(AgentKind::Codex),
                state: AgentState::Working,
                seen: true,
                run_started_at: Some(1000),
            }
        );
        assert_eq!(
            panes[1].agent_status,
            AgentStatus {
                agent: Some(AgentKind::Claude),
                state: AgentState::Idle,
                seen: false,
                run_started_at: None,
            }
        );
        assert_eq!(
            panes[2].agent_status,
            AgentStatus {
                agent: Some(AgentKind::OpenCode),
                state: AgentState::Blocked,
                seen: true,
                run_started_at: Some(2000),
            }
        );
    }

    #[test]
    fn codex_hook_status_is_a_fallback_signal() {
        let panes = parse_panes("%1\t@1\t1\tcodex\t/tmp\t\t123\t\t\t\tready\t1\n").unwrap();

        assert_eq!(
            panes[0].agent_status,
            AgentStatus {
                agent: Some(AgentKind::Codex),
                state: AgentState::Idle,
                seen: false,
                run_started_at: None,
            }
        );
    }

    #[test]
    fn env_tmux_value_ignores_unexpanded_tmux_formats() {
        std::env::set_var("TMUX_AGENT_SIDEBAR_TEST_LITERAL", "#{window_id}");
        assert_eq!(env_tmux_value("TMUX_AGENT_SIDEBAR_TEST_LITERAL"), None);

        std::env::set_var("TMUX_AGENT_SIDEBAR_TEST_LITERAL", "@42");
        assert_eq!(
            env_tmux_value("TMUX_AGENT_SIDEBAR_TEST_LITERAL"),
            Some("@42".to_owned())
        );

        std::env::remove_var("TMUX_AGENT_SIDEBAR_TEST_LITERAL");
    }
}
