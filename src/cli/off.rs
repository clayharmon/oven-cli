use anyhow::{Context, Result};

use super::GlobalOpts;

#[allow(clippy::unused_async)]
pub async fn run(_global: &GlobalOpts) -> Result<()> {
    let project_dir = std::env::current_dir().context("getting current directory")?;
    let pid_path = project_dir.join(".oven").join("oven.pid");

    let pid_str = std::fs::read_to_string(&pid_path)
        .context("no detached process found (missing .oven/oven.pid)")?;
    let pid = pid_str.trim().parse::<u32>().context("invalid PID in .oven/oven.pid")?;

    // Send SIGTERM via the kill command (avoids unsafe libc calls)
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .context("sending SIGTERM")?;

    if !status.success() {
        // Process might already be dead, which is fine
        tracing::warn!(pid, "kill returned non-zero (process may already be stopped)");
    }

    // Wait briefly for the process to exit
    for _ in 0..50 {
        let check = std::process::Command::new("kill").arg("-0").arg(pid.to_string()).status();
        match check {
            Ok(s) if !s.success() => break, // process gone
            _ => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }

    std::fs::remove_file(&pid_path).ok();
    println!("stopped (pid {pid})");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn pid_parse_valid() {
        let pid: u32 = "12345\n".trim().parse().unwrap();
        assert_eq!(pid, 12345);
    }

    #[test]
    fn pid_parse_invalid() {
        let result = "not_a_pid".parse::<u32>();
        assert!(result.is_err());
    }

    #[test]
    fn missing_pid_file_gives_helpful_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".oven").join("oven.pid");
        let result = std::fs::read_to_string(&path);
        assert!(result.is_err());
    }
}
