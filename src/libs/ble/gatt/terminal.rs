//! Terminal characteristics (FB05 TX, FB06 RX). Spawns a persistent bash
//! shell via `script` (PTY) and proxies stdin/stdout over BLE notifications.

use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Commands that are flat-out rejected before reaching the shell.
const DESTRUCTIVE_PATTERNS: &[&str] = &["rm -rf /", "mkfs", "dd if="];

/// Commands that don't make sense over BLE (interactive UIs, full-screen apps).
const INTERACTIVE_COMMANDS: &[&str] = &[
    "vi", "vim", "nano", "top", "htop", "less", "more",
    "ssh", "telnet", "screen", "tmux", "man",
];

/// Decision of the security filter for an incoming command.
#[derive(Debug, PartialEq, Eq)]
pub enum CommandPolicy {
    /// Forward to the shell.
    Allow,
    /// Reject before spawning anything; show this message to the client.
    Reject(&'static str),
}

pub fn classify_command(cmd: &str) -> CommandPolicy {
    let lower = cmd.to_lowercase();
    if DESTRUCTIVE_PATTERNS.iter().any(|p| lower.contains(p)) {
        return CommandPolicy::Reject("Error: Command not allowed for security reasons");
    }
    let base = lower.split_whitespace().next().unwrap_or("");
    if INTERACTIVE_COMMANDS.iter().any(|i| base == *i) {
        return CommandPolicy::Reject("Error: Interactive commands not supported over BLE");
    }
    CommandPolicy::Allow
}

/// Persistent shell process backing the Terminal characteristic.
/// Public to the BLE module so service.rs can hand it to ServiceState.
pub struct ShellProcess {
    pub stdin: ChildStdin,
    pub child: Child,
    pub cancel_token: CancellationToken,
    pub _reader_task: tokio::task::JoinHandle<()>,
}

impl Drop for ShellProcess {
    fn drop(&mut self) {
        self.cancel_token.cancel();
        let _ = self.child.start_kill();
        eprintln!("[ble::terminal] ShellProcess dropped, process killed");
    }
}

/// Spawn a persistent bash shell process using 'script' command for PTY support.
/// The 'script' command creates its own PTY internally, bypassing permission issues.
pub(crate) async fn spawn_persistent_shell(
    terminal_notifier: Arc<Mutex<bluer::gatt::local::CharacteristicNotifier>>,
) -> Result<ShellProcess, String> {
    use tokio::process::Command as TokioCommand;

    eprintln!("[Terminal] Spawning shell via script command...");

    // Use 'script' command which creates its own PTY internally
    // This bypasses the permission issues with direct PTY creation
    // -q: quiet mode (no start/done messages)
    // -c: command to run (bash 2>&1 to merge stderr into stdout)
    // /dev/null: typescript file (we don't need the recording)
    let mut child = TokioCommand::new("script")
        .args(["-q", "-c", "bash 2>&1", "/dev/null"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())  // Discard stderr to avoid buffer deadlock
        .spawn()
        .map_err(|e| format!("Failed to spawn script: {}", e))?;

    let stdin = child.stdin.take().ok_or("Failed to get stdin")?;
    let stdout = child.stdout.take().ok_or("Failed to get stdout")?;

    // Create cancellation token for the reader task
    let cancel_token = CancellationToken::new();

    // Background task to read output and send via BLE
    let reader_task = tokio::spawn(read_shell_output(
        stdout,
        terminal_notifier,
        cancel_token.clone(),
    ));

    eprintln!("[Terminal] Script/PTY shell spawned successfully");

    Ok(ShellProcess {
        stdin,
        child,
        cancel_token,
        _reader_task: reader_task,
    })
}

/// Read shell output and send via BLE notifications.
async fn read_shell_output(
    mut stdout: tokio::process::ChildStdout,
    terminal_notifier: Arc<Mutex<bluer::gatt::local::CharacteristicNotifier>>,
    cancel_token: CancellationToken,
) {
    use std::time::{Duration, Instant};

    eprintln!("[Terminal] Reader task started");
    let mut buf = [0u8; 1024];

    // Rate limiting to prevent BLE buffer overflow
    // Max ~5KB/sec to keep BLE notifications manageable
    let mut last_send = Instant::now();
    let mut bytes_sent = 0usize;

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                eprintln!("[Terminal] Reader cancelled");
                break;
            }
            result = stdout.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        eprintln!("[Terminal] Shell EOF");
                        break;
                    }
                    Ok(n) => {
                        // Rate limiting: if we've sent too much data recently, throttle
                        if bytes_sent > 5000 && last_send.elapsed() < Duration::from_secs(1) {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            bytes_sent = 0;
                            last_send = Instant::now();
                        }

                        eprintln!("[Terminal] Read {} bytes from shell", n);
                        let mut notifier = terminal_notifier.lock().await;
                        for chunk in buf[..n].chunks(200) {
                            if let Err(e) = notifier.notify(chunk.to_vec()).await {
                                eprintln!("[Terminal] Notify error: {}, stopping reader", e);
                                return;
                            }
                        }
                        bytes_sent += n;
                    }
                    Err(e) => {
                        eprintln!("[Terminal] Shell read error: {}", e);
                        break;
                    }
                }
            }
        }
    }
    eprintln!("[Terminal] Shell reader ended");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_rm_rejected() {
        assert_eq!(classify_command("rm -rf /"),
                   CommandPolicy::Reject("Error: Command not allowed for security reasons"));
    }

    #[test]
    fn destructive_mkfs_rejected() {
        assert_eq!(classify_command("mkfs.ext4 /dev/sda1"),
                   CommandPolicy::Reject("Error: Command not allowed for security reasons"));
    }

    #[test]
    fn destructive_dd_rejected() {
        assert_eq!(classify_command("dd if=/dev/zero of=/dev/sda"),
                   CommandPolicy::Reject("Error: Command not allowed for security reasons"));
    }

    #[test]
    fn destructive_case_insensitive() {
        assert_ne!(classify_command("RM -RF /"), CommandPolicy::Allow);
    }

    #[test]
    fn interactive_top_rejected() {
        assert_eq!(classify_command("top"),
                   CommandPolicy::Reject("Error: Interactive commands not supported over BLE"));
    }

    #[test]
    fn interactive_vim_with_args_rejected() {
        assert_eq!(classify_command("vim /etc/hostname"),
                   CommandPolicy::Reject("Error: Interactive commands not supported over BLE"));
    }

    #[test]
    fn benign_ls_allowed() {
        assert_eq!(classify_command("ls -la /home"), CommandPolicy::Allow);
    }

    #[test]
    fn benign_cat_allowed() {
        assert_eq!(classify_command("cat /etc/hostname"), CommandPolicy::Allow);
    }

    #[test]
    fn empty_command_allowed() {
        // The shell will simply emit a new prompt — no security harm.
        assert_eq!(classify_command(""), CommandPolicy::Allow);
    }

    #[test]
    fn blocking_a_word_in_quotes() {
        // We deliberately use substring match for "rm -rf /", so quoted
        // appearances also trigger. This is a known false-positive — fine
        // for a debug terminal.
        assert!(matches!(classify_command(r#"echo "do not run rm -rf /""#),
                         CommandPolicy::Reject(_)));
    }
}
