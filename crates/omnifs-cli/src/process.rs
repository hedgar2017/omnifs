//! Unix process probes shared by detached runtime owners: the exit of a
//! process, an event of the kernel, and the poll of a Docker mount, which
//! has no event.

use std::future::Future;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};

/// Poll `check` at `interval` until it returns `Ok(Some(value))` (ready), an
/// `Err` (an immediate hard failure, not subject to the deadline), or return
/// `Ok(None)` once `timeout` elapses without either. `check` returning
/// `Ok(None)` means "not ready yet, keep polling."
///
/// A caller whose own body cannot fail folds any hard-abort condition into
/// `Ok(Some(value))` instead and ignores the (then-unreachable) `Err` case,
/// rather than inventing a placeholder error type.
pub(crate) async fn poll_until<T, Fut>(
    timeout: Duration,
    interval: Duration,
    mut check: impl FnMut() -> Fut,
) -> anyhow::Result<Option<T>>
where
    Fut: Future<Output = anyhow::Result<Option<T>>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = check().await? {
            return Ok(Some(value));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(interval).await;
    }
}

/// How a detached child's log file should be opened. The host runner's log
/// accumulates across every launch (the runner itself is long-lived state);
/// the libkrun helper's diagnostic log is per-launch and holds credential-free
/// but still restricted host-owned VM detail, so it starts fresh and 0600.
#[derive(Clone, Copy)]
pub(crate) enum LogMode {
    Append,
    TruncateRestricted0600,
}

/// Wire a detached child's stdio and process group: stdin closed, stdout and
/// stderr both pointed at one freshly opened `log_path` (cloned rather than
/// opened twice, so a truncating open cannot race itself), and its own
/// process group so a parent CLI exit never takes the child down with it.
/// Shared by the host runner and the libkrun helper, whose spawn recipes
/// otherwise repeated this five-step dance identically apart from the log's
/// open mode.
pub(crate) fn configure_detached_child(
    command: &mut Command,
    log_path: &Path,
    mode: LogMode,
) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true);
    match mode {
        LogMode::Append => {
            options.append(true);
        },
        LogMode::TruncateRestricted0600 => {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.truncate(true).mode(0o600);
        },
    }
    let log = options
        .open(log_path)
        .with_context(|| format!("open log {}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("clone log {}", log_path.display()))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    Ok(())
}

/// A fresh random 128-bit instance id, hex-encoded. Shared by every runner
/// that needs to name one launch distinctly from the next; `what` names the
/// runner kind for the error message if the system RNG itself fails.
pub(crate) fn new_instance_id(what: &str) -> anyhow::Result<String> {
    use anyhow::Context as _;

    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).with_context(|| format!("generate {what} instance id"))?;
    Ok(hex::encode(bytes))
}

/// How the omnifs process is running, which sets its default tracing level.
#[derive(Clone, Copy)]
pub(crate) enum ProcessRole {
    /// A foreground CLI invocation: stays quiet so ordinary commands are not
    /// noisy.
    Cli,
    /// A background daemon the CLI spawned: defaults louder so its startup
    /// diagnostics are captured in daemon.log rather than hidden.
    Daemon,
}

impl ProcessRole {
    /// The default `RUST_LOG` level for this process role.
    pub(crate) const fn default_log_level(self) -> &'static str {
        match self {
            Self::Cli => "warn",
            Self::Daemon => "info",
        }
    }
}

/// Whether `kill -0` reports `pid` as a live process.
pub(crate) fn is_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The exit of the process `pid`, an event of the kernel that becomes
/// readable at it; none for a process that has already exited.
pub(crate) fn exit_event(
    pid: u32,
) -> Result<Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>> {
    Ok(omnifs_mtab::process_exit(pid)
        .with_context(|| format!("watch the exit of process {pid}"))?
        .map(|exit| tokio::io::unix::AsyncFd::with_interest(exit, tokio::io::Interest::READABLE))
        .transpose()?)
}

/// The exit that `exit` watches; at once without one, for a process that
/// has already exited.
pub(crate) async fn exited(exit: Option<&tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>) {
    if let Some(exit) = exit {
        drop(exit.readable().await);
    }
}

/// Waits for the exit of the process `pid`, an event of the kernel: true at
/// its exit, or for a process that has already exited; false once it is
/// silent for `silence` without an exit.
pub(crate) async fn process_ended(pid: u32, silence: Duration) -> Result<bool> {
    let exit = exit_event(pid)?;
    Ok(tokio::time::timeout(silence, exited(exit.as_ref()))
        .await
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::{is_alive, process_ended};
    use std::time::Duration;

    /// The wait for the exit of a process ends at its exit, past which it
    /// would have waited for the bound of a silent peer.
    #[tokio::test]
    async fn the_wait_for_an_exit_ends_at_the_exit() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let wait = tokio::spawn(process_ended(pid, omnifs_core::SILENT_PEER_TIMEOUT));
        tokio::time::sleep(Duration::from_millis(100)).await;
        child.kill().unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the wait outlived the exit")
            .unwrap()
            .unwrap();
        assert!(ended);
        child.wait().unwrap();
    }

    /// The wait for the exit of a live process that does not exit ends once
    /// it is silent for its bound.
    #[tokio::test]
    async fn the_wait_for_an_exit_of_a_live_process_ends_after_its_silence() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let ended = tokio::time::timeout(
            Duration::from_secs(5),
            process_ended(child.id(), Duration::from_millis(50)),
        )
        .await
        .expect("the wait outlived its silence")
        .unwrap();
        assert!(!ended);
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn distinguishes_current_and_exited_processes() {
        assert!(is_alive(std::process::id()));

        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!is_alive(pid));
    }
}
