//! Minimal daemon bootstrap used by commands that need the control plane.

use anyhow::{Context as _, ensure};
use omnifs_bootstrap::{Bootstrap, Client, FolderChange};
use std::process::Stdio;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use crate::error::{ExitCode, WithExitCode as _};
use crate::rpc::RpcClient;
use crate::ui::output::Output;

/// Every mutating and daemon-dependent command routes through here first.
/// A spawn narrates exactly one line before it starts waiting; an
/// already-running daemon narrates nothing. With `--wait-for-daemon`, it
/// waits for a daemon that another process starts, without starting one;
/// with `--require-daemon`, it fails at once when no daemon listens,
/// without starting or waiting for one. No time bounds a wait: the ready
/// phase of the daemon ends it, or the exit of the daemon that this command
/// started.
pub(crate) async fn start(output: &Output) -> anyhow::Result<()> {
    let endpoint = Bootstrap::<Client>::for_client()?;
    if output.wait_for_daemon() {
        return wait_for_daemon(&endpoint).await;
    }
    let rpc = RpcClient::from_endpoint(endpoint.clone());
    if output.require_daemon() {
        if !rpc.control_reachable().await? {
            return Err(anyhow::anyhow!(
                "no omnifs daemon listens on {}",
                endpoint.control_socket().display()
            ))
            .with_exit_code(ExitCode::DaemonUnavailable);
        }
        return rpc.ready_phase().await;
    }
    let _spawn_lock = endpoint
        .acquire_spawn_lock()
        .context("acquire daemon spawn lock")?;
    if rpc.control_reachable().await? {
        return rpc.ready_phase().await;
    }
    output.narrate("Starting the daemon");
    let mut child = spawn()?;
    let expected_pid = child.id();
    tokio::select! {
        status = child.wait() => {
            let status = status.context("wait for the omnifs daemon")?;
            Err(anyhow::anyhow!("omnifs daemon exited before it became ready ({status})"))
                .with_exit_code(ExitCode::DaemonUnavailable)
        },
        ready = wait_for_daemon(&endpoint) => {
            ready?;
            let pid = rpc.daemon_info().await?.pid;
            ensure!(
                expected_pid.is_none_or(|expected| expected == pid),
                "daemon readiness came from pid {pid}, not spawned pid {}",
                expected_pid.unwrap_or_default()
            );
            Ok(())
        },
    }
}

/// Waits for a daemon of the profile of `endpoint` that another process
/// starts: a watch of the folder of the profile, or of its parent while that
/// folder is missing, before each connection to its control socket, which
/// connects again at each change of that folder until a connection succeeds;
/// then the readiness of the daemon, as its `WatchPhase` stream gives it.
async fn wait_for_daemon(endpoint: &Bootstrap<Client>) -> anyhow::Result<()> {
    let folder = endpoint.bootstrap_dir();
    let socket = endpoint.control_socket();
    loop {
        let (change, missing) = match FolderChange::watch(folder) {
            Ok(change) => (change, false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = folder
                    .parent()
                    .with_context(|| format!("{} has no parent folder", folder.display()))?;
                let change = FolderChange::watch(parent)
                    .with_context(|| format!("watch {}", parent.display()))?;
                (change, true)
            },
            Err(error) => {
                return Err(error).with_context(|| format!("watch {}", folder.display()));
            },
        };
        match UnixStream::connect(&socket).await {
            Ok(stream) => {
                drop(stream);
                break;
            },
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) => {},
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("connect to daemon at {}", socket.display()));
            },
        }
        // A folder that its parent gained since the watch has its own.
        if missing && folder.exists() {
            continue;
        }
        drop(
            AsyncFd::with_interest(change, Interest::READABLE)?
                .readable()
                .await?,
        );
    }
    RpcClient::from_endpoint(endpoint.clone())
        .ready_phase()
        .await
}

fn spawn() -> anyhow::Result<Child> {
    let binary = std::env::current_exe().context("resolve the omnifs executable")?;
    let mut command = Command::new(&binary);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.process_group(0);
    command
        .spawn()
        .with_context(|| format!("spawn omnifs daemon ({})", binary.display()))
}
