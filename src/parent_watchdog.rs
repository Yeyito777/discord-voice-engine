use std::path::Path;
use std::time::Duration;

use signal_hook::consts::signal::SIGTERM;
use signal_hook::low_level::raise;

/// Returns true while `parent_pid` still appears to be the direct parent of this
/// process.  Record launches the voice helpers directly, so a reparent to init/a
/// subreaper means Record died without running its normal cleanup path.
pub fn is_direct_parent_alive(parent_pid: u32) -> bool {
    if parent_pid == 0 {
        return false;
    }
    if !Path::new(&format!("/proc/{parent_pid}")).exists() {
        return false;
    }
    match current_parent_pid() {
        Some(current) => current == parent_pid,
        None => true,
    }
}

pub fn install_parent_exit_watchdog(parent_pid: Option<u32>, label: &'static str) {
    let Some(parent_pid) = parent_pid else {
        return;
    };
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if is_direct_parent_alive(parent_pid) {
                continue;
            }
            eprintln!(
                "discord-voice-engine {label}: parent process {parent_pid} disappeared; exiting"
            );
            let _ = raise(SIGTERM);
            std::thread::sleep(Duration::from_secs(2));
            std::process::exit(0);
        }
    });
}

fn current_parent_pid() -> Option<u32> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = stat.rsplit_once(") ")?.1;
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_parent_pid_is_available_on_linux_procfs() {
        assert!(current_parent_pid().is_some());
    }

    #[test]
    fn zero_pid_is_not_alive() {
        assert!(!is_direct_parent_alive(0));
    }
}
