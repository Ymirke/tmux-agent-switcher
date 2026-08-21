//! Helpers for detached child processes that must not stall the daemon.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

pub(crate) fn detached(command: &mut Command) {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();

    if let Ok(mut child) = child {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

pub(crate) fn shell_detached(command_line: &str) {
    if command_line.trim().is_empty() {
        return;
    }
    detached(Command::new("sh").args(["-c", command_line]));
}
