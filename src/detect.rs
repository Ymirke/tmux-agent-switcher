//! Passive agent detection: recognizing agent processes by name and inferring
//! their state (working / blocked / idle) from the pane title and screen tail.

use crate::model::{AgentEvidence, AgentKind, AgentState};

pub fn detect_agent_from_process_name(name: &str) -> Option<AgentKind> {
    let basename = name.rsplit('/').next().unwrap_or(name);
    if basename == "codex" || basename.starts_with("codex-") {
        Some(AgentKind::Codex)
    } else if basename == "opencode" || basename.starts_with("opencode-") {
        Some(AgentKind::OpenCode)
    } else if basename == "claude"
        || basename == "claude-code"
        || basename.starts_with("claude-")
        || is_claude_version_name(basename)
    {
        Some(AgentKind::Claude)
    } else {
        None
    }
}

/// Claude Code's native installer runs the versioned binary at
/// `~/.local/share/claude/versions/<version>`, and Claude also sets its
/// `process.title` to that same version string. Either way tmux reports the
/// pane's current command as a bare `MAJOR.MINOR.PATCH` semver (e.g. `2.1.197`)
/// rather than `claude` (see anthropics/claude-code#49852). Treat that shape as
/// Claude Code so agent detection still fires.
fn is_claude_version_name(name: &str) -> bool {
    let mut parts = 0;
    for part in name.split('.') {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts == 3
}

pub fn detect_agent_state(agent: AgentKind, evidence: &AgentEvidence) -> AgentState {
    if evidence.process_exited {
        return AgentState::Idle;
    }

    match agent {
        AgentKind::Codex => detect_codex_state(evidence),
        AgentKind::Claude => detect_claude_state(evidence),
        AgentKind::OpenCode => detect_opencode_state(evidence),
    }
}

pub(crate) fn detect_agent_state_from_title(agent: AgentKind, title: &str) -> Option<AgentState> {
    let title = title.trim();
    match agent {
        AgentKind::Codex if title.contains("Action Required") => Some(AgentState::Blocked),
        AgentKind::Codex if starts_with_braille_status(title) => Some(AgentState::Working),
        AgentKind::Codex if !title.is_empty() => Some(AgentState::Idle),
        AgentKind::Claude if starts_with_claude_working_status(title) => Some(AgentState::Working),
        // OpenCode's title is a static session label ("OpenCode" or "OC | …")
        // and does not encode activity, so always fall through to screen-tail
        // detection for it.
        AgentKind::OpenCode => None,
        _ => None,
    }
}

fn detect_codex_state(evidence: &AgentEvidence) -> AgentState {
    let title = evidence.osc_title.trim();
    let tail = evidence.screen_tail.to_lowercase();

    if title.contains("Action Required")
        || contains_any(
            &tail,
            &[
                "press enter to confirm or esc to cancel",
                "enter to submit answer",
                "allow command?",
                "[y/n]",
                "yes (y)",
                "no (n)",
            ],
        )
    {
        return AgentState::Blocked;
    }

    if starts_with_braille_status(title) {
        return AgentState::Working;
    }

    if !title.is_empty() {
        return AgentState::Idle;
    }

    AgentState::Idle
}

fn detect_claude_state(evidence: &AgentEvidence) -> AgentState {
    let title = evidence.osc_title.trim();
    // Claude's prompts always sit at the bottom of the screen; only match the
    // last handful of lines so stale scrollback can't pin a state (e.g. an old
    // "do you want" line keeping a working pane marked Blocked).
    let recent = recent_screen(&evidence.screen_tail, 25);
    let recent_lower = recent.to_lowercase();

    // Blocked: a modal selection menu is on screen (the cursor is resting on one
    // of several numbered options), waiting for the user to choose. With
    // `--dangerously-skip-permissions` this is plan-mode approval, AskUserQuestion
    // menus and trust prompts rather than per-command permission asks. The match is
    // structural (wording-agnostic), plus the selection-list footer as a fallback.
    if has_selection_prompt(&recent)
        || contains_all(&recent_lower, &["enter to select", "esc to cancel"])
    {
        return AgentState::Blocked;
    }

    // Working: Claude prefixes its OSC title with an animated status glyph while
    // active. Older releases used a braille spinner; 2.1.235 uses ◐/◑ instead.
    if starts_with_claude_working_status(title) {
        return AgentState::Working;
    }

    // Working: the main loop is parked at the prompt but background subagents are
    // still running. The title has the idle ✳ prefix in this state, so the only
    // signal is the live "✻ Waiting for N background agents to finish" status line.
    if waiting_on_background_agents(&recent) {
        return AgentState::Working;
    }

    // Otherwise Claude is idle at its input prompt (title starts with ✳). The `❯`
    // input box is present while working too, so it is not an idle signal on its own.
    AgentState::Idle
}

/// True when Claude's live status line — the last content line above the input
/// prompt — says it is waiting on background agents/tasks. The same line also
/// persists in the transcript after each agent wake-up, so matching anywhere in
/// the tail would pin a settled pane Working; only the live copy counts, which
/// is why this walks up from the input prompt instead of substring-matching the
/// whole region.
fn waiting_on_background_agents(recent: &str) -> bool {
    let lines: Vec<&str> = recent.lines().collect();
    // The input prompt is the last `❯` line on screen; selection-menu cursors
    // (also `❯`-prefixed) always render above the input box.
    let Some(prompt) = lines
        .iter()
        .rposition(|line| strip_border(line).starts_with('❯'))
    else {
        return false;
    };
    for line in lines[..prompt].iter().rev() {
        let stripped = strip_border(line).trim_end();
        // Skip the chrome between the status line and the prompt: blank lines,
        // the input box border, and attached `⎿` tip/result lines.
        if stripped.is_empty()
            || stripped.starts_with('⎿')
            || stripped
                .chars()
                .all(|ch| matches!(ch, '─' | '━' | '═' | '╌' | '╭' | '╮' | '╰' | '╯'))
        {
            continue;
        }
        let lower = stripped.to_lowercase();
        return lower.contains("waiting for") && lower.contains("background");
    }
    false
}

fn detect_opencode_state(evidence: &AgentEvidence) -> AgentState {
    // OpenCode renders all live state at the bottom of the TUI. Limit matching
    // to that region so an old prompt or status line in the transcript cannot
    // keep a settled session marked busy or blocked.
    let recent = recent_screen(&evidence.screen_tail, 25);
    let recent_lower = recent.to_lowercase();

    // Permission prompts have a fixed heading and action labels. Question
    // prompts are dynamic, but consistently pair an Enter action with
    // "esc dismiss" while waiting for an answer.
    let permission_prompt = recent_lower.contains("permission required")
        && contains_any(&recent_lower, &["allow once", "allow always", "reject"]);
    let question_prompt = recent_lower.contains("esc dismiss")
        && contains_any(
            &recent_lower,
            &["enter submit", "enter confirm", "enter toggle"],
        );
    let blocked_at = if permission_prompt || question_prompt {
        [
            "permission required",
            "allow once",
            "allow always",
            "reject",
            "esc dismiss",
        ]
        .into_iter()
        .filter_map(|marker| recent_lower.rfind(marker))
        .max()
    } else {
        None
    };

    // Busy and retry states share the prompt footer's interrupt action. This
    // text remains stable even when animations are disabled, unlike the
    // preceding spinner frames. When a redraw briefly contains both old and
    // new footers, whichever marker occurs last is the current state.
    let working_at = ["esc interrupt", "esc again to interrupt"]
        .into_iter()
        .filter_map(|marker| recent_lower.rfind(marker))
        .max();

    match (blocked_at, working_at) {
        (Some(blocked), Some(working)) if working > blocked => AgentState::Working,
        (Some(_), _) => AgentState::Blocked,
        (_, Some(_)) => AgentState::Working,
        _ => AgentState::Idle,
    }
}

/// True when the screen shows a Claude selection menu: the cursor (`❯`) rests on
/// a numbered option AND at least two numbered options are present. Requiring a
/// second option distinguishes a real menu from the bare `❯` input box (even when
/// the user types a single line like "1. …" into it), and stripping the box border
/// makes it work on Claude's real bordered rendering (`│ ❯ 1. Yes │`), where the
/// option no longer begins the line.
///
/// Known ambiguity: a user composing a *multi-line* numbered list in the input box
/// is structurally identical to a menu and can read as Blocked. Anchoring on a menu
/// footer would remove it, but Claude's permission/plan modals don't render one, so
/// that would miss the real prompts this exists to catch. The idle->busy debounce
/// (`BUSY_DEBOUNCE_POLLS` in the daemon module) already absorbs the common
/// fast-typed case.
fn has_selection_prompt(text: &str) -> bool {
    let mut cursor_on_option = false;
    let mut option_lines = 0;
    for line in text.lines() {
        let line = strip_border(line);
        let (has_cursor, rest) = match line.strip_prefix('❯') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line),
        };
        let digits = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
        if digits == 0 {
            continue;
        }
        let after = &rest[digits..];
        if after.starts_with('.') || after.starts_with(')') {
            option_lines += 1;
            cursor_on_option |= has_cursor;
        }
    }
    cursor_on_option && option_lines >= 2
}

/// Strips a line's leading whitespace and box-drawing verticals so matching works
/// whether or not the content is wrapped in a border.
fn strip_border(line: &str) -> &str {
    line.trim_start_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, '│' | '┃' | '║' | '╎' | '┆' | '┊' | '|')
    })
}

fn recent_screen(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

fn starts_with_braille_status(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(ch) if ('\u{2800}'..='\u{28ff}').contains(&ch))
        && matches!(chars.next(), Some(' '))
}

fn starts_with_claude_working_status(value: &str) -> bool {
    if starts_with_braille_status(value) {
        return true;
    }

    let mut chars = value.chars();
    matches!(chars.next(), Some('◐' | '◑')) && matches!(chars.next(), Some(' '))
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn contains_all(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().all(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_agent_processes_and_states() {
        assert_eq!(
            detect_agent_from_process_name("/opt/bin/codex"),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            detect_agent_from_process_name("codex-aarch64-a"),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            detect_agent_from_process_name("claude-code"),
            Some(AgentKind::Claude)
        );
        // Native-installer / process.title path: tmux sees a bare semver.
        assert_eq!(
            detect_agent_from_process_name("2.1.197"),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            detect_agent_from_process_name("/Users/x/.local/share/claude/versions/2.1.197"),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            detect_agent_from_process_name("/Users/x/.opencode/bin/opencode"),
            Some(AgentKind::OpenCode)
        );
        assert_eq!(
            detect_agent_from_process_name("opencode-darwin-arm64"),
            Some(AgentKind::OpenCode)
        );
        // Non-semver commands must not be mistaken for Claude.
        assert_eq!(detect_agent_from_process_name("zsh"), None);
        assert_eq!(detect_agent_from_process_name("2.1"), None);
        assert_eq!(detect_agent_from_process_name("node"), None);

        let codex = AgentEvidence {
            screen_tail: "press enter to confirm or esc to cancel".to_owned(),
            osc_title: String::new(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Codex, &codex),
            AgentState::Blocked
        );

        let claude = AgentEvidence {
            screen_tail: "anything".to_owned(),
            osc_title: "⠋ thinking".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &claude),
            AgentState::Working
        );

        let opencode = AgentEvidence {
            screen_tail: "⬝⬝⬝⬝⬝⬝⬝⬝  esc interrupt".to_owned(),
            osc_title: "OC | implement status support".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::OpenCode, &opencode),
            AgentState::Working
        );
    }

    #[test]
    fn title_fast_path_detects_unambiguous_agent_states() {
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Codex, "[ ! ] Action Required | repo"),
            Some(AgentState::Blocked)
        );
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Codex, "⠋ working"),
            Some(AgentState::Working)
        );
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Codex, "repo"),
            Some(AgentState::Idle)
        );
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Claude, "⠋ thinking"),
            Some(AgentState::Working)
        );
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Claude, "◐ using tools"),
            Some(AgentState::Working)
        );
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Claude, "◑ using tools"),
            Some(AgentState::Working)
        );
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Claude, "✳ review this"),
            None
        );
        assert_eq!(
            detect_agent_state_from_title(AgentKind::OpenCode, "OC | working or idle"),
            None
        );
    }

    #[test]
    fn opencode_permission_and_question_prompts_are_blocked() {
        for screen_tail in [
            [
                "△ Permission required",
                "Shell command",
                "Allow once  Allow always  Reject",
            ]
            .join("\n"),
            [
                "Which database should we use?",
                "1. Postgres",
                "2. SQLite",
                "enter submit  esc dismiss",
            ]
            .join("\n"),
        ] {
            let evidence = AgentEvidence {
                screen_tail,
                osc_title: "OC | choose a database".to_owned(),
                osc_progress: String::new(),
                process_exited: false,
            };
            assert_eq!(
                detect_agent_state(AgentKind::OpenCode, &evidence),
                AgentState::Blocked
            );
        }
    }

    #[test]
    fn opencode_static_title_and_settled_prompt_are_idle() {
        let evidence = AgentEvidence {
            screen_tail: [
                "The implementation is complete.",
                "Build · Claude Sonnet 4",
                "/Users/example/project  ctrl+p commands  • OpenCode 1.18.10",
            ]
            .join("\n"),
            osc_title: "OC | implement status support".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::OpenCode, &evidence),
            AgentState::Idle
        );
    }

    #[test]
    fn opencode_latest_footer_wins_over_stale_prompt_text() {
        let evidence = AgentEvidence {
            screen_tail: [
                "Reviewed UI text: enter submit  esc dismiss",
                "Continuing implementation.",
                "⬝⬝⬝⬝⬝⬝⬝⬝  esc interrupt",
            ]
            .join("\n"),
            osc_title: "OC | inspect question UI".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::OpenCode, &evidence),
            AgentState::Working
        );
    }

    #[test]
    fn claude_selection_menu_is_blocked_even_with_idle_title() {
        let evidence = AgentEvidence {
            screen_tail: [
                "│ Would you like to proceed?              │",
                "│ ❯ 1. Yes, and auto-accept edits         │",
                "│   2. Yes, and manually approve edits    │",
                "│   3. No, keep planning                  │",
            ]
            .join("\n"),
            osc_title: "✳ design the thing".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &evidence),
            AgentState::Blocked
        );
    }

    #[test]
    fn claude_idle_input_box_is_not_blocked() {
        let evidence = AgentEvidence {
            screen_tail: [
                "※ recap: did the thing. next: your review.",
                "──────────────── ultracode ─",
                "❯ ",
                "────────────────",
                "  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents",
            ]
            .join("\n"),
            osc_title: "✳ clarify the logic".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &evidence),
            AgentState::Idle
        );
    }

    #[test]
    fn claude_waiting_on_background_agents_is_working() {
        // Main loop parked at the prompt (idle ✳ title) while subagents run:
        // the live waiting line sits directly above the input box.
        let evidence = AgentEvidence {
            screen_tail: [
                "⏺ The schema/migrations review is in — one Critical finding.",
                "",
                "✻ Waiting for 2 background agents to finish",
                "",
                "───────────────────────────────",
                "❯ ",
                "───────────────────────────────",
                "  ⏵⏵ bypass permissions on (shift+tab to cycle) · /tasks to see subagents · ← for agents",
                "",
                "  ⏺ main",
                "  ◯ pr-reviewer  Reviewing routes diff        4m 29s · ↓ 131.4k tokens",
            ]
            .join("\n"),
            osc_title: "✳ review the PR".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &evidence),
            AgentState::Working
        );
    }

    #[test]
    fn claude_stale_waiting_line_in_transcript_is_idle() {
        // After the run settles, old waiting lines remain in the transcript but
        // the last content above the prompt is the final answer — not the live
        // status line — so the pane must read Idle, not stuck Working.
        let evidence = AgentEvidence {
            screen_tail: [
                "✻ Waiting for 1 background agent to finish",
                "",
                "⏺ Agent \"Review routes\" finished · 9m 02s",
                "",
                "⏺ All three reviews are done; merged report above.",
                "",
                "───────────────────────────────",
                "❯ ",
                "───────────────────────────────",
                "  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents",
            ]
            .join("\n"),
            osc_title: "✳ review the PR".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &evidence),
            AgentState::Idle
        );
    }

    #[test]
    fn claude_stale_prompt_scrolled_off_does_not_block() {
        let mut lines = vec!["Do you want to proceed?".to_owned()];
        for index in 0..30 {
            lines.push(format!("build output line {index}"));
        }
        let evidence = AgentEvidence {
            screen_tail: lines.join("\n"),
            osc_title: "⠙ working".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &evidence),
            AgentState::Working
        );
    }

    #[test]
    fn claude_bordered_menu_without_known_phrase_is_blocked() {
        // A custom AskUserQuestion menu: no wording from any phrase list, non-1
        // numbering, ')' delimiter, drawn inside a border.
        let evidence = AgentEvidence {
            screen_tail: [
                "│ Which database should we use?           │",
                "│ ❯ 2) Postgres                           │",
                "│   3) SQLite                             │",
            ]
            .join("\n"),
            osc_title: "✳ pick a database".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &evidence),
            AgentState::Blocked
        );
    }

    #[test]
    fn selection_prompt_requires_cursor_and_a_second_option() {
        // Real bordered menu: cursor on one of several options.
        assert!(has_selection_prompt("│ ❯ 1. Yes │\n│   2. No │"));
        // Unbordered menu also matches.
        assert!(has_selection_prompt("❯ 10) ten\n  11) eleven"));
        // Bare input box, or the user typing a single "1." line into it, is not a menu.
        assert!(!has_selection_prompt("❯ "));
        assert!(!has_selection_prompt("❯ 1. fix the parser and then rebase"));
        // A plain numbered list in output (no cursor) is not a menu.
        assert!(!has_selection_prompt("1. first\n2. second"));
    }
}
