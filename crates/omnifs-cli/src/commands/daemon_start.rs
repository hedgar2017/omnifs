//! Minimal daemon bootstrap used by commands that need the control plane.

use anyhow::{Context as _, ensure};
use omnifs_api::DaemonPhase;
use omnifs_bootstrap::{Bootstrap, Client, DaemonOwner, FolderChange};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use crate::error::{ExitCode, WithExitCode as _};
use crate::rpc::RpcClient;
use crate::ui::output::Output;

const READY_TIMEOUT: Duration = Duration::from_mins(2);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Every mutating and daemon-dependent command routes through here first.
/// Readiness polling can take up to `READY_TIMEOUT`, so a spawn narrates
/// exactly one line before it starts waiting; an already-running daemon
/// narrates nothing, since there is nothing this command is doing that the
/// operator does not already know about. With `--wait-for-daemon`, it waits
/// for a daemon that another process starts, without starting one.
pub(crate) async fn start(output: &Output) -> anyhow::Result<()> {
    start_in(&Bootstrap::<Client>::for_client()?, output).await
}

/// Starts the daemon of the profile of `endpoint` as the process of this
/// command, or waits for the one that another process starts: with
/// `--wait-for-daemon`, and in a profile whose last daemon recorded another
/// parent than a command as the one that started it, as a service manager,
/// which starts the next one too.
async fn start_in(endpoint: &Bootstrap<Client>, output: &Output) -> anyhow::Result<()> {
    if output.wait_for_daemon() {
        return wait_for_daemon(endpoint).await;
    }
    let spawn_lock = endpoint
        .acquire_spawn_lock()
        .context("acquire daemon spawn lock")?;
    let rpc = RpcClient::from_endpoint(endpoint.clone());
    if rpc.control_reachable().await? {
        return wait_until_ready(&rpc, None).await;
    }
    let owner = endpoint
        .read_daemon_owner()
        .with_context(|| format!("read {}", endpoint.daemon_owner_path().display()))?;
    match owner {
        Some(DaemonOwner::Service) => {
            // The wait has no bound: no other command waits on this one.
            drop(spawn_lock);
            output.narrate(format!(
                "Waiting for the daemon, which a service manager starts for this profile ({})",
                endpoint.daemon_owner_path().display()
            ));
            wait_for_daemon(endpoint).await
        },
        Some(DaemonOwner::Command) | None => {
            output.narrate("Starting the daemon");
            let mut child = spawn(endpoint)?;
            wait_until_ready(&rpc, Some(&mut child)).await
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

/// The daemon of the profile of `endpoint` as a process of this command,
/// which the daemon records by its flag.
fn spawn(endpoint: &Bootstrap<Client>) -> anyhow::Result<Child> {
    let binary = std::env::current_exe().context("resolve the omnifs executable")?;
    let mut command = Command::new(&binary);
    command
        .arg("daemon")
        .arg(crate::cli::DaemonArgs::STARTED_BY_COMMAND)
        .env(omnifs_bootstrap::OMNIFS_HOME_ENV, endpoint.bootstrap_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.process_group(0);
    command
        .spawn()
        .with_context(|| format!("spawn omnifs daemon ({})", binary.display()))
}

async fn wait_until_ready(rpc: &RpcClient, child: Option<&mut Child>) -> anyhow::Result<()> {
    // Bundled so the check closure captures nothing from the enclosing scope
    // and takes everything by explicit `&mut` argument instead: an `FnMut`
    // closure cannot itself return a future that borrows its own captured
    // environment, but a fresh reborrow of an argument can.
    struct Wait<'a> {
        rpc: &'a RpcClient,
        child: Option<&'a mut Child>,
        expected_pid: Option<u32>,
    }
    let expected_pid = child.as_ref().and_then(|child| child.id());
    let mut wait = Wait {
        rpc,
        child,
        expected_pid,
    };
    let ready =
        crate::process::poll_until_mut(READY_TIMEOUT, READY_POLL_INTERVAL, &mut wait, |wait| {
            Box::pin(async move {
                if let Some(child) = wait.child.as_mut()
                    && let Some(status) = child.try_wait().context("poll daemon child status")?
                {
                    return Err(anyhow::anyhow!(
                        "omnifs daemon exited before it became ready ({status})"
                    ))
                    .with_exit_code(ExitCode::DaemonUnavailable)
                    .map(Some);
                }

                if let Ok(inventory) = wait.rpc.inventory().await {
                    if let Some(expected_pid) = wait.expected_pid {
                        ensure!(
                            inventory.info.pid == expected_pid,
                            "daemon readiness came from pid {}, not spawned pid {expected_pid}",
                            inventory.info.pid
                        );
                    }
                    match inventory.phase {
                        DaemonPhase::Ready => {
                            wait.rpc.ready().await?;
                            return Ok(Some(()));
                        },
                        DaemonPhase::RecoveryRequired => {
                            return Err(anyhow::anyhow!(
                                "daemon requires recovery: {}",
                                inventory.health.mounts.message
                            ))
                            .with_exit_code(ExitCode::DaemonUnavailable)
                            .map(Some);
                        },
                        DaemonPhase::Starting => {},
                    }
                }
                Ok(None)
            })
        })
        .await?;
    if ready.is_some() {
        return Ok(());
    }
    if let Some(child) = wait.child.as_mut() {
        let _ = child.kill().await;
    }
    Err(anyhow::anyhow!(
        "omnifs daemon did not become ready within {}s",
        READY_TIMEOUT.as_secs()
    ))
    .with_exit_code(ExitCode::DaemonUnavailable)
}
