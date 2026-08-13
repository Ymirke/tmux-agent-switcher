use tmux_agent_sidebar::{
    detect_agent_from_process_name, detect_agent_state, parse_panes, AgentEvidence, AgentKind,
    AgentState,
};

fn evidence(screen_tail: &str) -> AgentEvidence {
    AgentEvidence {
        screen_tail: screen_tail.to_owned(),
        osc_title: "OC | status integration test".to_owned(),
        osc_progress: String::new(),
        process_exited: false,
    }
}

#[test]
fn recognizes_opencode_processes() {
    assert_eq!(
        detect_agent_from_process_name("/Users/example/.opencode/bin/opencode"),
        Some(AgentKind::OpenCode)
    );
    assert_eq!(
        detect_agent_from_process_name("opencode-linux-x64"),
        Some(AgentKind::OpenCode)
    );
}

#[test]
fn detects_opencode_working_blocked_and_idle_states() {
    assert_eq!(
        detect_agent_state(AgentKind::OpenCode, &evidence("⬝⬝⬝⬝⬝⬝⬝⬝  esc interrupt")),
        AgentState::Working
    );
    assert_eq!(
        detect_agent_state(
            AgentKind::OpenCode,
            &evidence("△ Permission required\nAllow once  Allow always  Reject"),
        ),
        AgentState::Blocked
    );
    assert_eq!(
        detect_agent_state(
            AgentKind::OpenCode,
            &evidence("Which option?\n1. First\n2. Second\nenter submit  esc dismiss"),
        ),
        AgentState::Blocked
    );
    assert_eq!(
        detect_agent_state(
            AgentKind::OpenCode,
            &evidence("Task complete.\nBuild · Claude Sonnet 4\n• OpenCode 1.18.10"),
        ),
        AgentState::Idle
    );
}

#[test]
fn restores_cached_opencode_status_from_tmux() {
    let panes =
        parse_panes("%3\t@2\t1\topencode\t/tmp\tOC | task\t125\topencode\tblocked\t1\t2000\t\t\n")
            .unwrap();

    assert_eq!(panes[0].agent_status.agent, Some(AgentKind::OpenCode));
    assert_eq!(panes[0].agent_status.state, AgentState::Blocked);
    assert_eq!(panes[0].agent_status.run_started_at, Some(2000));
}
