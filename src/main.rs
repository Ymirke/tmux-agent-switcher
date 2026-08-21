use anyhow::Result;
use tmux_agent_sidebar::{execute_action, load_cards, run_status_daemon, run_tui};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("status-daemon") => run_status_daemon(),
        Some(command) => anyhow::bail!("unknown tmux-agent-sidebar command: {command}"),
        None => {
            if let Some(action) = run_tui(load_cards()?)? {
                execute_action(action)?;
            }
            Ok(())
        }
    }
}
