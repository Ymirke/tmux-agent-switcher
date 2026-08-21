//! The background status daemon: polls every pane, infers agent status, and
//! caches it in tmux pane/window options so the switcher and status line can
//! read it cheaply.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::{
    cards::rollup_agent_status,
    detect::{detect_agent_from_process_name, detect_agent_state, detect_agent_state_from_title},
    model::{
        format_agent_kind, format_agent_state, AgentEvidence, AgentKind, AgentState, AgentStatus,
        TmuxPane,
    },
    tmux::{parse_panes, set_pane_option, shell_quote, tmux_output, tmux_status, unix_timestamp},
};

const STATUS_DAEMON_INTERVAL: Duration = Duration::from_millis(300);
const NO_AGENT_DAEMON_INTERVAL: Duration = Duration::from_secs(2);
const STATUS_DAEMON_OWNERSHIP_CHECK_POLLS: u32 = 10;
const STATUS_CAPTURE_LINES: usize = 25;
/// Consecutive idle polls required before committing a Working/Blocked -> Idle
/// transition, so a single stray sample can't flash a spurious "done" or reset
/// the run timer. At STATUS_DAEMON_INTERVAL this is roughly a 1s settle window.
const IDLE_DEBOUNCE_POLLS: u32 = 4;
/// Consecutive polls required before committing a settled Idle pane into a busy
/// state, so a single stray Working/Blocked sample can't wipe a committed "done"
/// or restart its timer. Kept short so real work still shows promptly.
pub(crate) const BUSY_DEBOUNCE_POLLS: u32 = 2;
const STATUS_DAEMON_PID_OPTION: &str = "@tmux_agent_sidebar_status_daemon_pid";
const STATUS_DAEMON_BUILD_OPTION: &str = "@tmux_agent_sidebar_status_daemon_build";
const STATUS_AGENT_OPTION: &str = "@tmux_agent_sidebar_agent";
const STATUS_STATE_OPTION: &str = "@tmux_agent_sidebar_state";
pub(crate) const STATUS_SEEN_OPTION: &str = "@tmux_agent_sidebar_seen";
const STATUS_RUN_STARTED_OPTION: &str = "@tmux_agent_sidebar_run_started_at";
const STATUS_UPDATED_OPTION: &str = "@tmux_agent_sidebar_updated";
const STATUS_WINDOW_ICON_OPTION: &str = "@tmux_agent_sidebar_window_icon";
const TICK_COMMAND_OPTION: &str = "@agent_sidebar_tick_command";
const TICK_INTERVAL_OPTION: &str = "@agent_sidebar_tick_interval";
const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(60);
const PROCESS_TREE_MAX_AGE: Duration = Duration::from_millis(1200);
const IDLE_DAEMON_INTERVAL: Duration = Duration::from_secs(1);
const QUIET_POLLS_BEFORE_BACKOFF: u32 = 20;
const DAEMON_CHECK_INTERVAL: Duration = Duration::from_secs(5);

pub fn ensure_status_daemon() -> Result<()> {
    static LAST_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

    let mut last_check = match LAST_CHECK.lock() {
        Ok(last_check) => last_check,
        Err(poisoned) => poisoned.into_inner(),
    };
    let recently_checked = last_check
        .as_ref()
        .is_some_and(|checked| checked.elapsed() < DAEMON_CHECK_INTERVAL);
    if recently_checked {
        return Ok(());
    }
    *last_check = Some(Instant::now());
    drop(last_check);

    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    let pid = current_status_daemon_pid();
    let running_current_build = executable_fingerprint(&current_exe)
        .map(|fingerprint| tmux_option(STATUS_DAEMON_BUILD_OPTION) == fingerprint)
        .unwrap_or(true);
    if !pid.is_empty() && running_current_build && status_daemon_process_matches(&pid, &current_exe)
    {
        return Ok(());
    }

    let command = format!(
        "{} status-daemon",
        shell_quote(&current_exe.to_string_lossy())
    );
    tmux_status(Command::new("tmux").args(["run-shell", "-b", &command]))
}

struct Pace {
    quiet_polls: u32,
}

impl Pace {
    fn new() -> Self {
        Self { quiet_polls: 0 }
    }

    fn after(&mut self, any_agent: bool, changed: bool) -> Duration {
        if !any_agent {
            self.quiet_polls = 0;
            return NO_AGENT_DAEMON_INTERVAL;
        }

        if changed {
            self.quiet_polls = 0;
            return STATUS_DAEMON_INTERVAL;
        }

        self.quiet_polls = self.quiet_polls.saturating_add(1);
        if self.quiet_polls >= QUIET_POLLS_BEFORE_BACKOFF {
            return IDLE_DAEMON_INTERVAL;
        }
        STATUS_DAEMON_INTERVAL
    }
}

struct Tick {
    last_run: Instant,
    interval: Duration,
}

impl Tick {
    fn new() -> Self {
        Self {
            last_run: Instant::now(),
            interval: read_tick_interval(),
        }
    }

    fn run_if_due(&mut self) {
        if self.last_run.elapsed() < self.interval {
            return;
        }
        self.last_run = Instant::now();
        self.interval = read_tick_interval();
        crate::spawn::shell_detached(&tmux_option(TICK_COMMAND_OPTION));
    }
}

fn read_tick_interval() -> Duration {
    parse_tick_interval(&tmux_option(TICK_INTERVAL_OPTION))
}

fn parse_tick_interval(value: &str) -> Duration {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TICK_INTERVAL)
}

fn tmux_option(name: &str) -> String {
    tmux_output(&["show-option", "-gqv", name])
        .unwrap_or_default()
        .trim()
        .to_owned()
}

pub fn run_status_daemon() -> Result<()> {
    let pid = std::process::id().to_string();
    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    let fingerprint = executable_fingerprint(&current_exe).unwrap_or_default();
    tmux_status(Command::new("tmux").args([
        "set-option",
        "-g",
        "-q",
        STATUS_DAEMON_PID_OPTION,
        &pid,
        ";",
        "set-option",
        "-g",
        "-q",
        STATUS_DAEMON_BUILD_OPTION,
        &fingerprint,
    ]))?;

    let mut debounce: HashMap<String, Debounce> = HashMap::new();
    let mut ownership_check = 0;
    let mut tick = Tick::new();
    let mut pace = Pace::new();
    let mut previous_panes: Option<HashSet<String>> = None;
    loop {
        if ownership_check == 0 && current_status_daemon_pid() != pid {
            break;
        }
        ownership_check = (ownership_check + 1) % STATUS_DAEMON_OWNERSHIP_CHECK_POLLS;

        let Ok(outcome) = poll_agent_status_once_with_activity(&mut debounce) else {
            break;
        };
        let pane_set_changed = previous_panes.as_ref() != Some(&outcome.panes);
        previous_panes = Some(outcome.panes);
        tick.run_if_due();
        thread::sleep(pace.after(outcome.any_agent, outcome.raw_changed || pane_set_changed));
    }

    if current_status_daemon_pid() == pid {
        let _ = tmux_status(Command::new("tmux").args([
            "set-option",
            "-g",
            "-u",
            "-q",
            STATUS_DAEMON_PID_OPTION,
            ";",
            "set-option",
            "-g",
            "-u",
            "-q",
            STATUS_DAEMON_BUILD_OPTION,
        ]));
    }
    Ok(())
}

pub fn poll_agent_status_once(debounce: &mut HashMap<String, Debounce>) -> Result<()> {
    poll_agent_status_once_with_activity(debounce).map(|_| ())
}

struct PollOutcome {
    panes: HashSet<String>,
    raw_changed: bool,
    any_agent: bool,
}

fn raw_sample_changed(
    previous: AgentStatus,
    previous_raw: Option<AgentState>,
    agent: AgentKind,
    raw: AgentState,
) -> bool {
    previous.agent != Some(agent)
        || previous_raw
            .map(|state| state != raw)
            .unwrap_or(previous.state != raw)
}

fn poll_agent_status_once_with_activity(
    debounce: &mut HashMap<String, Debounce>,
) -> Result<PollOutcome> {
    let mut panes = parse_panes(&tmux_output(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}\t#{window_id}\t#{pane_active}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_title}\t#{pane_pid}\t#{@tmux_agent_sidebar_agent}\t#{@tmux_agent_sidebar_state}\t#{@tmux_agent_sidebar_seen}\t#{@tmux_agent_sidebar_run_started_at}",
    ])?)?;

    let now = unix_timestamp();
    let live: HashSet<String> = panes.iter().map(|pane| pane.pane_id.clone()).collect();
    // Built lazily: only needed when a pane that was an agent no longer reports
    // one as its foreground command, to tell an exit from a foreground subprocess.
    let mut processes: Option<Arc<ProcessTree>> = None;
    let mut raw_changed = false;
    let mut any_agent = false;

    for pane in &mut panes {
        let previous = pane.agent_status;
        let agent = match detect_agent_from_process_name(&pane.pane_current_command) {
            Some(agent) => Some(agent),
            // The foreground command is no longer an agent. Keep the previous agent
            // only while the agent process is genuinely still alive under this pane
            // (e.g. it spawned a foreground child); otherwise treat it as exited so
            // the pane doesn't latch to a stale "claude idle". If the process table
            // can't be read this poll, keep the previous agent rather than clearing
            // it — a transient `ps` failure shouldn't drop a live agent to unknown.
            None => previous.agent.filter(|_| {
                let tree = processes.get_or_insert_with(ProcessTree::cached);
                tree.is_empty() || tree.has_agent_descendant(pane.pane_pid)
            }),
        };
        let next = if let Some(agent) = agent {
            any_agent = true;
            let raw = detect_agent_state_from_title(agent, &pane.pane_title).unwrap_or_else(|| {
                let evidence = AgentEvidence {
                    screen_tail: capture_pane_tail(&pane.pane_id, STATUS_CAPTURE_LINES),
                    osc_title: pane.pane_title.clone(),
                    osc_progress: String::new(),
                    process_exited: false,
                };
                detect_agent_state(agent, &evidence)
            });
            let previous_raw = debounce.get(&pane.pane_id).map(|state| state.candidate);
            raw_changed |= raw_sample_changed(previous, previous_raw, agent, raw);
            let pane_debounce = debounce
                .entry(pane.pane_id.clone())
                .or_insert_with(|| Debounce::new(raw));
            debounce_state(previous, agent, raw, pane_debounce, now)
        } else {
            raw_changed |= previous.agent.is_some() || debounce.contains_key(&pane.pane_id);
            debounce.remove(&pane.pane_id);
            AgentStatus::unknown()
        };
        write_agent_status(&pane.pane_id, previous, next)?;
        pane.agent_status = next;
    }

    write_window_status_icons(&panes)?;
    debounce.retain(|pane_id, _| live.contains(pane_id));
    Ok(PollOutcome {
        panes: live,
        raw_changed,
        any_agent,
    })
}

fn current_status_daemon_pid() -> String {
    tmux_output(&["show-option", "-gqv", STATUS_DAEMON_PID_OPTION])
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn status_daemon_process_matches(pid: &str, current_exe: &Path) -> bool {
    if !process_exists(pid) {
        return false;
    }

    let output = Command::new("ps")
        .args(["-p", pid, "-o", "command="])
        .output();
    let Ok(output) = output else {
        return true;
    };
    if !output.status.success() {
        return false;
    }

    let command = String::from_utf8_lossy(&output.stdout);
    command.contains(" status-daemon") && command.contains(current_exe.to_string_lossy().as_ref())
}

fn executable_fingerprint(path: &Path) -> Option<String> {
    let metadata = path.metadata().ok()?;
    let modified = metadata.modified().ok()?;
    let modified = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(format!("{}:{}", metadata.len(), modified.as_nanos()))
}

fn process_exists(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A snapshot of the process table used to decide whether an agent is still
/// running under a pane once its foreground command stops looking like one.
struct ProcessTree {
    children: HashMap<u32, Vec<u32>>,
    agent_pids: HashSet<u32>,
}

impl ProcessTree {
    fn cached() -> Arc<Self> {
        static CACHE: Mutex<Option<(Instant, Arc<ProcessTree>)>> = Mutex::new(None);

        let mut cache = match CACHE.lock() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some((taken, tree)) = cache.as_ref() {
            if taken.elapsed() < PROCESS_TREE_MAX_AGE {
                return Arc::clone(tree);
            }
        }

        let tree = Arc::new(Self::snapshot());
        *cache = Some((Instant::now(), Arc::clone(&tree)));
        tree
    }

    fn snapshot() -> Self {
        let output = Command::new("ps")
            .args(["-Ao", "pid=,ppid=,comm="])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        Self::parse(&output)
    }

    fn parse(output: &str) -> Self {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut agent_pids = HashSet::new();
        for line in output.lines() {
            let mut fields = line.split_whitespace();
            let (Some(pid), Some(ppid)) = (fields.next(), fields.next()) else {
                continue;
            };
            let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
                continue;
            };
            children.entry(ppid).or_default().push(pid);
            if detect_agent_from_process_name(fields.next().unwrap_or_default()).is_some() {
                agent_pids.insert(pid);
            }
        }
        Self {
            children,
            agent_pids,
        }
    }

    /// True when the snapshot captured no processes at all, i.e. `ps` failed or
    /// produced nothing — a signal to treat its answers as unavailable.
    fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// True if `root` or any of its descendants is an agent process.
    fn has_agent_descendant(&self, root: Option<u32>) -> bool {
        let Some(root) = root else {
            return false;
        };
        let mut stack = vec![root];
        let mut seen = HashSet::new();
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if self.agent_pids.contains(&pid) {
                return true;
            }
            if let Some(children) = self.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        false
    }
}

/// Per-pane transition debounce carried across polls in memory by the daemon.
/// Tracks the candidate raw state and how many consecutive polls have seen it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Debounce {
    candidate: AgentState,
    count: u32,
}

impl Debounce {
    fn new(state: AgentState) -> Self {
        Self {
            candidate: state,
            count: 0,
        }
    }
}

/// Polls required before a `committed -> raw` transition is committed. Both
/// directions across the idle boundary are debounced so a single noisy sample
/// (a one-frame braille title, a transient menu-shaped line) can neither flash a
/// premature "done"/timer reset nor re-arm a settled "done". Other transitions
/// (fresh detection from Unknown, Working<->Blocked) commit promptly.
fn debounce_threshold(committed: AgentState, raw: AgentState) -> u32 {
    use AgentState::{Blocked, Idle, Working};
    match (committed, raw) {
        (Working | Blocked, Idle) => IDLE_DEBOUNCE_POLLS,
        (Idle, Working | Blocked) => BUSY_DEBOUNCE_POLLS,
        _ => 1,
    }
}

/// Debounces a raw state sample against the pane's committed state on top of
/// [`stabilize_agent_status_at`]. A differing sample must persist for
/// [`debounce_threshold`] consecutive polls before it is committed; until then
/// the previously committed status (including its run timer and seen flag) is
/// held unchanged.
fn debounce_state(
    previous: AgentStatus,
    agent: AgentKind,
    raw: AgentState,
    debounce: &mut Debounce,
    now: u64,
) -> AgentStatus {
    if raw == previous.state {
        *debounce = Debounce::new(raw);
        return stabilize_agent_status_at(previous, agent, raw, now);
    }

    if debounce.candidate == raw {
        debounce.count += 1;
    } else {
        debounce.candidate = raw;
        debounce.count = 1;
    }

    if debounce.count >= debounce_threshold(previous.state, raw) {
        debounce.count = 0;
        stabilize_agent_status_at(previous, agent, raw, now)
    } else {
        AgentStatus {
            agent: Some(agent),
            ..previous
        }
    }
}

fn stabilize_agent_status_at(
    previous: AgentStatus,
    agent: AgentKind,
    state: AgentState,
    now: u64,
) -> AgentStatus {
    let seen = match state {
        AgentState::Idle
            if previous.state == AgentState::Working || previous.state == AgentState::Blocked =>
        {
            false
        }
        AgentState::Idle => previous.seen,
        AgentState::Working | AgentState::Blocked | AgentState::Unknown => true,
    };
    let run_started_at = match state {
        AgentState::Working | AgentState::Blocked => previous.run_started_at.or(Some(now)),
        AgentState::Idle | AgentState::Unknown => None,
    };

    AgentStatus {
        agent: Some(agent),
        state,
        seen,
        run_started_at,
    }
}

fn capture_pane_tail(pane_id: &str, lines: usize) -> String {
    // No `-e`: state detection matches plain text and glyphs (e.g. the `❯`
    // selection cursor), which ANSI escape sequences would otherwise split.
    tmux_output(&[
        "capture-pane",
        "-pJ",
        "-t",
        pane_id,
        "-S",
        &format!("-{lines}"),
    ])
    .unwrap_or_default()
}

fn write_agent_status(pane_id: &str, previous: AgentStatus, status: AgentStatus) -> Result<()> {
    let updates = status_option_updates(previous, status);
    for (option, value) in &updates {
        set_pane_option(pane_id, option, value)?;
    }
    if updates.is_empty() {
        return Ok(());
    }
    set_pane_option(
        pane_id,
        STATUS_UPDATED_OPTION,
        &unix_timestamp().to_string(),
    )
}

fn status_option_updates(
    previous: AgentStatus,
    status: AgentStatus,
) -> Vec<(&'static str, String)> {
    let mut updates = Vec::new();
    if previous.agent != status.agent {
        updates.push((
            STATUS_AGENT_OPTION,
            format_agent_kind(status.agent).to_owned(),
        ));
    }
    if previous.state != status.state {
        updates.push((
            STATUS_STATE_OPTION,
            format_agent_state(status.state).to_owned(),
        ));
    }
    if previous.seen != status.seen {
        updates.push((
            STATUS_SEEN_OPTION,
            if status.seen { "1" } else { "0" }.to_owned(),
        ));
    }
    if previous.run_started_at != status.run_started_at {
        updates.push((
            STATUS_RUN_STARTED_OPTION,
            status
                .run_started_at
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ));
    }
    updates
}

fn window_status_icons(panes: &[TmuxPane]) -> HashMap<String, &'static str> {
    let mut statuses: HashMap<&str, Vec<AgentStatus>> = HashMap::new();
    for pane in panes {
        statuses
            .entry(&pane.window_id)
            .or_default()
            .push(pane.agent_status);
    }

    statuses
        .into_iter()
        .map(|(window_id, statuses)| {
            let status = rollup_agent_status(statuses.into_iter());
            (window_id.to_owned(), tmux_window_status_icon(status))
        })
        .collect()
}

fn tmux_window_status_icon(status: AgentStatus) -> &'static str {
    if status.agent.is_none() {
        return "";
    }

    match status.state {
        AgentState::Blocked => " #[fg=red,bold]◉#[default]",
        AgentState::Working => " #[fg=yellow,bold]⠋#[default]",
        AgentState::Idle if !status.seen => " #[fg=cyan,bold]●#[default]",
        AgentState::Idle => " #[fg=green]✓#[default]",
        AgentState::Unknown => " #[fg=colour8]○#[default]",
    }
}

fn write_window_status_icons(panes: &[TmuxPane]) -> Result<()> {
    let desired = window_status_icons(panes);
    let current = tmux_output(&[
        "list-windows",
        "-a",
        "-F",
        "#{window_id}\t#{@tmux_agent_sidebar_window_icon}",
    ])?;

    for line in current.lines() {
        let Some((window_id, current_icon)) = line.split_once('\t') else {
            continue;
        };
        let desired_icon = desired.get(window_id).copied().unwrap_or_default();
        if desired_icon == current_icon {
            continue;
        }

        if desired_icon.is_empty() {
            tmux_status(Command::new("tmux").args([
                "set-option",
                "-w",
                "-u",
                "-q",
                "-t",
                window_id,
                STATUS_WINDOW_ICON_OPTION,
            ]))?;
        } else {
            tmux_status(Command::new("tmux").args([
                "set-option",
                "-w",
                "-q",
                "-t",
                window_id,
                STATUS_WINDOW_ICON_OPTION,
                desired_icon,
            ]))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_option_updates_only_includes_changed_fields() {
        let previous = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(1000),
        };

        assert!(status_option_updates(previous, previous).is_empty());

        let done = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Idle,
            seen: false,
            run_started_at: None,
        };
        assert_eq!(
            status_option_updates(previous, done),
            vec![
                (STATUS_STATE_OPTION, "idle".to_owned()),
                (STATUS_SEEN_OPTION, "0".to_owned()),
                (STATUS_RUN_STARTED_OPTION, String::new()),
            ]
        );

        let claude = AgentStatus {
            agent: Some(AgentKind::Claude),
            ..previous
        };
        assert_eq!(
            status_option_updates(previous, claude),
            vec![(STATUS_AGENT_OPTION, "claude".to_owned())]
        );
    }

    #[test]
    fn tmux_tab_icons_match_sidebar_status_meanings() {
        assert_eq!(tmux_window_status_icon(AgentStatus::unknown()), "");
        assert_eq!(
            tmux_window_status_icon(AgentStatus {
                agent: Some(AgentKind::Codex),
                state: AgentState::Blocked,
                seen: true,
                run_started_at: Some(1000),
            }),
            " #[fg=red,bold]◉#[default]"
        );
        assert_eq!(
            tmux_window_status_icon(AgentStatus::done(Some(AgentKind::Claude))),
            " #[fg=cyan,bold]●#[default]"
        );
    }

    #[test]
    fn tmux_tab_icon_rolls_up_all_panes_in_a_window() {
        let panes = parse_panes(
            "%1\t@1\t1\tcodex\t/tmp\tone\t101\tcodex\tworking\t1\t1000\n\
             %2\t@1\t0\tclaude\t/tmp\ttwo\t102\tclaude\tblocked\t1\t1000\n\
             %3\t@2\t1\tzsh\t/tmp\tthree\t103\t\tunknown\t1\t\n",
        )
        .unwrap();

        let icons = window_status_icons(&panes);
        assert_eq!(icons.get("@1"), Some(&" #[fg=red,bold]◉#[default]"));
        assert_eq!(icons.get("@2"), Some(&""));
    }

    #[test]
    fn daemon_timer_starts_on_working_and_survives_blocked_until_idle() {
        let idle = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Idle,
            seen: true,
            run_started_at: None,
        };

        let working = stabilize_agent_status_at(idle, AgentKind::Codex, AgentState::Working, 2000);
        assert_eq!(working.run_started_at, Some(2000));

        let blocked =
            stabilize_agent_status_at(working, AgentKind::Codex, AgentState::Blocked, 2030);
        assert_eq!(blocked.run_started_at, Some(2000));

        let done = stabilize_agent_status_at(blocked, AgentKind::Codex, AgentState::Idle, 2040);
        assert_eq!(done.run_started_at, None);
    }

    #[test]
    fn pace_uses_no_agent_interval_and_recovers_immediately() {
        let mut pace = Pace::new();

        assert_eq!(pace.after(false, false), NO_AGENT_DAEMON_INTERVAL);
        for _ in 0..QUIET_POLLS_BEFORE_BACKOFF - 1 {
            assert_eq!(pace.after(true, false), STATUS_DAEMON_INTERVAL);
        }
        assert_eq!(pace.after(true, false), IDLE_DAEMON_INTERVAL);
        assert_eq!(pace.after(true, false), IDLE_DAEMON_INTERVAL);
        assert_eq!(pace.after(true, true), STATUS_DAEMON_INTERVAL);
        assert_eq!(pace.after(true, false), STATUS_DAEMON_INTERVAL);
    }

    #[test]
    fn first_different_raw_sample_is_activity_before_debounce_commits_it() {
        let committed = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(1000),
        };

        assert!(raw_sample_changed(
            committed,
            Some(AgentState::Working),
            AgentKind::Claude,
            AgentState::Idle,
        ));
        assert!(!raw_sample_changed(
            committed,
            Some(AgentState::Idle),
            AgentKind::Claude,
            AgentState::Idle,
        ));
    }

    #[test]
    fn heartbeat_interval_rejects_values_that_could_create_a_busy_loop() {
        assert_eq!(parse_tick_interval(""), DEFAULT_TICK_INTERVAL);
        assert_eq!(parse_tick_interval("0"), DEFAULT_TICK_INTERVAL);
        assert_eq!(parse_tick_interval("-1"), DEFAULT_TICK_INTERVAL);
        assert_eq!(parse_tick_interval("nonsense"), DEFAULT_TICK_INTERVAL);
        assert_eq!(parse_tick_interval(" 15 "), Duration::from_secs(15));
    }

    #[test]
    fn process_tree_cache_reuses_a_recent_snapshot() {
        let first = ProcessTree::cached();
        let second = ProcessTree::cached();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn debounce_holds_busy_until_idle_streak_then_completes() {
        let working = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(1000),
        };
        let mut debounce = Debounce::new(AgentState::Working);

        // Early idle samples hold Working and keep the run timer intact.
        for poll in 1..IDLE_DEBOUNCE_POLLS {
            let held = debounce_state(
                working,
                AgentKind::Claude,
                AgentState::Idle,
                &mut debounce,
                1000 + poll as u64,
            );
            assert_eq!(held.state, AgentState::Working);
            assert_eq!(held.run_started_at, Some(1000));
            assert!(held.seen);
        }

        // The threshold sample commits Idle and flags it unseen ("done").
        let done = debounce_state(
            working,
            AgentKind::Claude,
            AgentState::Idle,
            &mut debounce,
            2000,
        );
        assert_eq!(done.state, AgentState::Idle);
        assert!(!done.seen);
        assert_eq!(done.run_started_at, None);
    }

    #[test]
    fn debounce_ignores_a_lone_busy_sample_after_done() {
        // A committed "done" (unread) pane.
        let done = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Idle,
            seen: false,
            run_started_at: None,
        };
        let mut debounce = Debounce::new(AgentState::Idle);

        // A lone stray Working sample must not wipe the done or start a timer.
        let held = debounce_state(
            done,
            AgentKind::Claude,
            AgentState::Working,
            &mut debounce,
            3000,
        );
        assert_eq!(held.state, AgentState::Idle);
        assert!(!held.seen);
        assert_eq!(held.run_started_at, None);

        // Sustained work reaches the busy threshold and commits a fresh run.
        assert_eq!(BUSY_DEBOUNCE_POLLS, 2);
        let working = debounce_state(
            done,
            AgentKind::Claude,
            AgentState::Working,
            &mut debounce,
            3005,
        );
        assert_eq!(working.state, AgentState::Working);
        assert!(working.seen);
        assert_eq!(working.run_started_at, Some(3005));
    }

    #[test]
    fn debounce_absorbs_single_idle_blip_without_false_done() {
        let working = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(500),
        };
        let mut debounce = Debounce::new(AgentState::Working);

        // One idle blip is held as Working with the timer intact.
        let held = debounce_state(
            working,
            AgentKind::Claude,
            AgentState::Idle,
            &mut debounce,
            510,
        );
        assert_eq!(held.state, AgentState::Working);
        assert_eq!(held.run_started_at, Some(500));
        assert!(held.seen);

        // Work resumes before the streak completes: no "done" is ever committed.
        let resumed = debounce_state(
            working,
            AgentKind::Claude,
            AgentState::Working,
            &mut debounce,
            520,
        );
        assert_eq!(resumed.state, AgentState::Working);
        assert!(resumed.seen);
        assert_eq!(resumed.run_started_at, Some(500));
    }

    #[test]
    fn debounce_commits_promptly_for_fresh_and_cross_busy_transitions() {
        // Fresh detection out of Unknown shows immediately (no settle delay).
        let mut debounce = Debounce::new(AgentState::Unknown);
        let first = debounce_state(
            AgentStatus::unknown(),
            AgentKind::Claude,
            AgentState::Idle,
            &mut debounce,
            10,
        );
        assert_eq!(first.state, AgentState::Idle);

        // Blocked appearing while Working is not delayed, and the timer carries over.
        let working = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(100),
        };
        let mut debounce = Debounce::new(AgentState::Working);
        let blocked = debounce_state(
            working,
            AgentKind::Claude,
            AgentState::Blocked,
            &mut debounce,
            150,
        );
        assert_eq!(blocked.state, AgentState::Blocked);
        assert_eq!(blocked.run_started_at, Some(100));
    }

    #[test]
    fn debounce_threshold_is_directional() {
        assert_eq!(
            debounce_threshold(AgentState::Working, AgentState::Idle),
            IDLE_DEBOUNCE_POLLS
        );
        assert_eq!(
            debounce_threshold(AgentState::Idle, AgentState::Working),
            BUSY_DEBOUNCE_POLLS
        );
        // Fresh detection and busy<->busy commit on the first sample.
        assert_eq!(debounce_threshold(AgentState::Unknown, AgentState::Idle), 1);
        assert_eq!(
            debounce_threshold(AgentState::Working, AgentState::Blocked),
            1
        );
    }

    #[test]
    fn process_tree_distinguishes_live_agent_from_exit() {
        // pane shell 100 -> claude 200 -> its bash subprocess 300.
        let running = ProcessTree::parse("100 1 zsh\n200 100 claude\n300 200 bash\n");
        assert!(running.has_agent_descendant(Some(100)));
        assert!(running.has_agent_descendant(Some(200)));

        // Same pane shell once Claude has exited: no agent left underneath.
        let exited = ProcessTree::parse("100 1 zsh\n400 100 nvim\n");
        assert!(!exited.has_agent_descendant(Some(100)));
        assert!(!exited.has_agent_descendant(None));

        // The versioned native binary is recognized by its comm path too.
        let versioned =
            ProcessTree::parse("10 1 -zsh\n11 10 /Users/x/.local/share/claude/versions/2.1.197\n");
        assert!(versioned.has_agent_descendant(Some(10)));
    }

    #[test]
    fn empty_process_tree_signals_ps_unavailable() {
        // A failed/empty `ps` yields an empty tree, which the poll treats as
        // "unknown" (keep the previous agent) rather than "no agent".
        assert!(ProcessTree::parse("").is_empty());
        assert!(!ProcessTree::parse("1 0 launchd\n").is_empty());
    }
}
