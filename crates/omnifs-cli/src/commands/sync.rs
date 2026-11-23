//! `omnifs sync` begins and ends the sync of a mount: each feed of the mount
//! is due once in a sync, and an operation outside a sync is served from the
//! projection of the mount.

use clap::{Args, Subcommand};
use serde::Serialize;

use crate::error::ExitCode;
use crate::ui::output::{Output, ResultVerdict};

#[derive(Args, Debug, Clone)]
pub struct SyncArgs {
    #[command(subcommand)]
    pub command: SyncCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum SyncCommand {
    /// Begin a sync of a mount, which ends the sync of it in progress, and
    /// print its number.
    Begin(BeginArgs),
    /// End the sync of a mount by its number.
    End(EndArgs),
}

#[derive(Args, Debug, Clone)]
pub struct BeginArgs {
    /// Name of the mount.
    pub mount: String,
    /// A path of the mount for this sync, `/` for the whole mount. Once a
    /// mount has such a path, only those paths exist in it, and no operation
    /// outside a sync asks its upstream.
    #[arg(long = "path", value_name = "PATH")]
    pub paths: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct EndArgs {
    /// Name of the mount.
    pub mount: String,
    /// Number of the sync, as `omnifs sync begin` printed it.
    pub sync: u64,
}

/// The result of a begin or an end of a sync.
#[derive(Debug, Serialize)]
struct SyncResult {
    mount: String,
    sync: u64,
}

impl SyncArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        let rpc = crate::rpc::RpcClient::resolve()?;
        let result = match self.command {
            SyncCommand::Begin(args) => {
                let sync = rpc.begin_sync(&args.mount, &args.paths).await?;
                SyncResult {
                    mount: args.mount,
                    sync,
                }
            },
            SyncCommand::End(args) => {
                rpc.end_sync(&args.mount, args.sync).await?;
                SyncResult {
                    mount: args.mount,
                    sync: args.sync,
                }
            },
        };
        if output.is_structured() {
            output.emit_result(ResultVerdict::Ok, result)?;
        } else {
            output.report(format!("{}\n", result.sync));
        }
        Ok(ExitCode::Success)
    }
}
