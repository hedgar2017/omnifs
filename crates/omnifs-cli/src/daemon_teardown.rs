//! Daemon shutdown workflow.
//!
//! Teardown is deliberately a typed collection step. Output renders these
//! outcomes directly, so a receipt cannot claim the daemon was stopped when
//! the cleanup only produced a warning.

use crate::inventory::{DaemonHealth, Inventory};
use crate::ui::consent::Outcome;
use crate::ui::output::Output;
use crate::ui::render::{self, LedgerRow};
use omnifs_bootstrap::{Bootstrap, Client, Instance};
use std::fmt::Write as _;

/// One observable teardown result. The variants retain enough context for a
/// command to choose severity and wording without parsing internal prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TeardownOutcome {
    DaemonStopped {
        pid: u32,
        detached: usize,
        still_attached: Vec<String>,
    },
    DaemonAlreadyStopped,
    DaemonShutdownFailed {
        error: String,
    },
    StaleRecordRemoved,
    StaleRecordAbsent,
    StaleRecordKept {
        error: String,
    },
    OwnershipUnknown {
        error: String,
    },
}

impl TeardownOutcome {
    pub(crate) fn id(&self) -> &'static str {
        match self {
            Self::DaemonStopped { .. }
            | Self::DaemonAlreadyStopped
            | Self::DaemonShutdownFailed { .. }
            | Self::OwnershipUnknown { .. } => "daemon",
            Self::StaleRecordRemoved | Self::StaleRecordAbsent | Self::StaleRecordKept { .. } => {
                "runtime-record"
            },
        }
    }

    pub(crate) fn outcome(&self) -> Outcome {
        match self {
            Self::DaemonStopped {
                pid,
                detached,
                still_attached,
            } => {
                let mut value = format!("stopped (pid {pid}, detached {detached} filesystems)");
                if !still_attached.is_empty() {
                    let _ = write!(
                        value,
                        "; still attached: {} (run `omnifs doctor`)",
                        still_attached.join(", ")
                    );
                }
                Outcome::done(self.id(), value)
            },
            Self::DaemonAlreadyStopped => Outcome::skip(self.id(), "already stopped"),
            Self::DaemonShutdownFailed { error } => {
                Outcome::fail(self.id(), format!("shutdown failed: {error}"))
            },
            Self::StaleRecordRemoved => Outcome::done(self.id(), "stale record removed"),
            Self::StaleRecordAbsent => Outcome::skip(self.id(), "no daemon record"),
            Self::StaleRecordKept { error } => {
                Outcome::fail(self.id(), format!("record kept: {error}"))
            },
            Self::OwnershipUnknown { error } => Outcome::fail(
                self.id(),
                format!("ownership could not be verified: {error}"),
            ),
        }
    }

    pub(crate) fn is_failure(&self) -> bool {
        matches!(
            self,
            Self::DaemonShutdownFailed { .. }
                | Self::StaleRecordKept { .. }
                | Self::OwnershipUnknown { .. }
        )
    }
}

pub(crate) struct DaemonTeardown {
    rpc: crate::rpc::RpcClient,
    endpoint: Bootstrap<Client>,
    initial_identity: Option<Instance>,
    initial: Option<Inventory>,
    /// Whether the shutdown asks the attached filesystems to stop.
    stop_filesystems: bool,
}

impl DaemonTeardown {
    pub(crate) fn with_inventory(
        endpoint: Bootstrap<Client>,
        inventory: Inventory,
        stop_filesystems: bool,
    ) -> Self {
        Self {
            rpc: crate::rpc::RpcClient::from_endpoint(endpoint.clone()),
            initial_identity: endpoint.read_process_identity().ok().flatten(),
            endpoint,
            initial: Some(inventory),
            stop_filesystems,
        }
    }

    /// Stop the namespace daemon and render the typed outcomes through
    /// `Output`, the same ledger-row primitive every other command's receipt
    /// uses. Bails on the first failure so the exit code reflects an
    /// incomplete teardown; that failing outcome's text becomes the bail's
    /// own message, so it is deliberately left out of the printed rows
    /// instead of showing once in the transcript and again in the error
    /// block.
    pub(crate) async fn down(&self, output: &Output) -> anyhow::Result<()> {
        let outcomes = self.down_collect().await?;
        let failure = outcomes.iter().find(|outcome| outcome.is_failure());
        match transcript(&outcomes, failure) {
            Transcript::NothingToStop => {
                output.narrate("Nothing to stop. The daemon isn't running.");
            },
            Transcript::Rows(rows) => {
                let keys: Vec<&str> = rows.iter().map(|row| row.key.as_str()).collect();
                let key_width = render::key_field_width(&keys);
                for row in &rows {
                    output.ledger_row(row, key_width);
                }
            },
        }
        if let Some(outcome) = failure {
            anyhow::bail!(outcome.outcome().value);
        }
        Ok(())
    }

    /// Run the daemon teardown workflow and return its typed outcomes without
    /// rendering. `down` renders these through Output; structured output settles
    /// them into a receipt.
    pub(crate) async fn down_collect(&self) -> anyhow::Result<Vec<TeardownOutcome>> {
        let mut outcomes = Vec::new();
        match self.initial_or_pid().await {
            Ok(Some(pid)) => {
                let outcome = self.shutdown_and_wait(pid, self.stop_filesystems).await;
                if matches!(
                    outcome,
                    TeardownOutcome::DaemonStopped { .. } | TeardownOutcome::DaemonAlreadyStopped
                ) && let Err(error) = self.remove_identity_for_expected_process(pid)
                {
                    outcomes.push(TeardownOutcome::StaleRecordKept {
                        error: error.to_string(),
                    });
                }
                outcomes.push(outcome);
            },
            Ok(None) => outcomes.push(self.remove_stale_record()),
            Err(error) => match self.recorded_pid_liveness()? {
                Some(true) => {
                    let outcome = TeardownOutcome::OwnershipUnknown {
                        error: format!(
                            "daemon status failed while the recorded process is still alive; \
                             stop it manually, then retry: {error:#}"
                        ),
                    };
                    outcomes.push(outcome);
                },
                Some(false) => outcomes.push(self.remove_stale_record()),
                None => return Err(error),
            },
        }
        Ok(outcomes)
    }

    /// Request shutdown and wait for the exit of the daemon process, which
    /// ends its control surface. The daemon acknowledges shutdown before its
    /// serving task exits, so the acknowledgement alone is not enough to
    /// report `DaemonStopped`; a daemon that does not exit is a silent peer.
    async fn shutdown_and_wait(&self, pid: u32, stop_filesystems: bool) -> TeardownOutcome {
        match self.rpc.shutdown(stop_filesystems).await {
            Ok(Some(shutdown)) => {
                let stopped = match crate::process::process_ended(
                    pid,
                    omnifs_core::SILENT_PEER_TIMEOUT,
                )
                .await
                {
                    Ok(ended) => ended || !self.initial_process_is_alive(pid),
                    Err(error) => {
                        return TeardownOutcome::DaemonShutdownFailed {
                            error: format!("{error:#}"),
                        };
                    },
                };
                if stopped {
                    return TeardownOutcome::DaemonStopped {
                        pid,
                        detached: shutdown.detached,
                        still_attached: shutdown.still_attached,
                    };
                }
                TeardownOutcome::DaemonShutdownFailed {
                    error: format!(
                        "shutdown acknowledged but the daemon process did not exit within {}s",
                        omnifs_core::SILENT_PEER_TIMEOUT.as_secs()
                    ),
                }
            },
            Ok(None) => TeardownOutcome::DaemonAlreadyStopped,
            Err(error) => TeardownOutcome::DaemonShutdownFailed {
                error: format!("{error:#}"),
            },
        }
    }

    fn initial_process_is_alive(&self, pid: u32) -> bool {
        self.initial_identity
            .as_ref()
            .filter(|identity| identity.pid() == pid)
            .map_or_else(
                || crate::process::is_alive(pid),
                Instance::still_identifies_running_process,
            )
    }

    async fn initial_or_pid(&self) -> anyhow::Result<Option<u32>> {
        match self.initial.as_ref().map(|inventory| &inventory.daemon) {
            Some(daemon) if daemon.health() == DaemonHealth::Stopped => Ok(None),
            Some(daemon) if daemon.health() != DaemonHealth::Unreachable => Ok(daemon.pid()),
            _ => Ok(self.rpc.status_optional().await?.map(|status| status.pid)),
        }
    }
    fn remove_stale_record(&self) -> TeardownOutcome {
        match self.recorded_pid_liveness() {
            Ok(Some(true)) => TeardownOutcome::StaleRecordKept {
                error: "the recorded daemon process is still alive; ownership cannot be verified"
                    .to_owned(),
            },
            Ok(Some(false)) => match self.remove_identity_for_expected_process(0) {
                Ok(()) => TeardownOutcome::StaleRecordRemoved,
                Err(error) => TeardownOutcome::StaleRecordKept {
                    error: error.to_string(),
                },
            },
            Ok(None) => TeardownOutcome::StaleRecordAbsent,
            Err(error) => TeardownOutcome::StaleRecordKept {
                error: error.to_string(),
            },
        }
    }

    fn remove_identity_for_expected_process(&self, pid: u32) -> anyhow::Result<()> {
        let Some(expected) = &self.initial_identity else {
            return Ok(());
        };
        if pid != 0 && expected.pid() != pid {
            anyhow::bail!(
                "process identity pid changed from {} to {pid}",
                expected.pid()
            );
        }
        if self.endpoint.remove_daemon_bootstrap_if(expected)? {
            return Ok(());
        }
        match self.endpoint.read_process_identity()? {
            None => Ok(()),
            Some(current) if current == *expected => {
                anyhow::bail!("process identity still exists after cleanup")
            },
            Some(_) => anyhow::bail!("process identity changed during teardown; refusing removal"),
        }
    }

    fn recorded_pid_liveness(&self) -> anyhow::Result<Option<bool>> {
        let Some(identity) = self.endpoint.read_process_identity()? else {
            return Ok(None);
        };
        Ok(Some(identity.still_identifies_running_process()))
    }
}

/// What `down`'s human output renders, pure and independent of the real
/// terminal so it is deterministically testable: either the shared "nothing
/// to stop" sentence, or one ledger row per visible outcome. Visible means
/// the `daemon` row plus any *failing* `runtime-record` outcome: successful
/// record bookkeeping is implementation detail a human never asked to see,
/// but a kept record is exactly why the command is about to exit nonzero, so
/// hiding it would print a transcript that contradicts the error block. The
/// row matching `failure` (if any) is left out of `Rows` too: its text
/// becomes the caller's bailed error message instead, and printing it twice
/// would contradict the error block the same way hiding a kept record would.
///
/// No visible outcome at all means the daemon was never running and its
/// bookkeeping succeeded: an already-absent record needs no cleanup line,
/// and a removed stale record isn't a "stop" either, since nothing was
/// actually running.
enum Transcript {
    NothingToStop,
    Rows(Vec<LedgerRow>),
}

fn transcript(outcomes: &[TeardownOutcome], failure: Option<&TeardownOutcome>) -> Transcript {
    let visible: Vec<&TeardownOutcome> = outcomes
        .iter()
        .filter(|outcome| outcome.id() == "daemon" || outcome.is_failure())
        .collect();
    if visible.is_empty() {
        return Transcript::NothingToStop;
    }
    Transcript::Rows(
        visible
            .into_iter()
            .filter(|outcome| Some(*outcome) != failure)
            .map(|outcome| outcome.outcome().ledger_row())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::style::Glyph;

    /// Unwrap the `Rows` variant, panicking with a useful message on
    /// `NothingToStop`: every test below that reaches this expects rows.
    fn rows(transcript: Transcript) -> Vec<LedgerRow> {
        match transcript {
            Transcript::Rows(rows) => rows,
            Transcript::NothingToStop => panic!("expected rows, got NothingToStop"),
        }
    }

    #[test]
    fn teardown_outcomes_have_truthful_severity_and_ids() {
        let stopped = TeardownOutcome::DaemonStopped {
            pid: 42,
            detached: 2,
            still_attached: Vec::new(),
        }
        .outcome();
        assert_eq!(stopped.id, "daemon");
        assert_eq!(stopped.glyph(), Glyph::Done);

        let failed = TeardownOutcome::DaemonShutdownFailed {
            error: "busy".to_owned(),
        }
        .outcome();
        assert_eq!(failed.id, "daemon");
        // Daemon shutdown failure is a hard failure, not a warning.
        assert_eq!(failed.glyph(), Glyph::Fail);
        assert!(failed.value.contains("busy"));
    }

    /// The "daemon was running" branch: one `daemon` ledger row, the
    /// successful `runtime-record` bookkeeping staying invisible.
    #[test]
    fn transcript_shows_the_stopped_daemon_row() {
        let outcomes = vec![
            TeardownOutcome::DaemonStopped {
                pid: 31114,
                detached: 2,
                still_attached: Vec::new(),
            },
            TeardownOutcome::StaleRecordRemoved,
        ];
        let rows = rows(transcript(&outcomes, None));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].key, "daemon");
        assert_eq!(rows[0].glyph, Glyph::Done);
        assert_eq!(rows[0].value, "stopped (pid 31114, detached 2 filesystems)");
    }

    /// The "nothing running" branch: `Nothing to stop. The daemon
    /// isn't running.` No orphan `runtime-record` ledger fragment leaks
    /// through even when a stale record needed cleanup.
    #[test]
    fn transcript_matches_the_nothing_running_shape() {
        assert!(matches!(
            transcript(&[TeardownOutcome::StaleRecordAbsent], None),
            Transcript::NothingToStop
        ));
        assert!(matches!(
            transcript(&[TeardownOutcome::StaleRecordRemoved], None),
            Transcript::NothingToStop
        ));
    }

    /// A kept record is why `down` is about to exit nonzero, so without an
    /// exclusion it must show as its own fail row instead of the
    /// contradictory "Nothing to stop" claim hiding it.
    #[test]
    fn a_failing_record_outcome_is_never_hidden_behind_nothing_to_stop() {
        let outcome = TeardownOutcome::StaleRecordKept {
            error: "the recorded daemon process is still alive".to_owned(),
        };
        let rows = rows(transcript(std::slice::from_ref(&outcome), None));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].value.contains("record kept"), "{rows:?}");
    }

    /// The one outcome that will bail is excluded from the printed rows: its
    /// text becomes the propagated error's own message, so a caller that
    /// still printed it as a row too would show the same failure twice.
    #[test]
    fn the_bailing_outcome_is_excluded_from_the_printed_rows() {
        let outcome = TeardownOutcome::StaleRecordKept {
            error: "the recorded daemon process is still alive".to_owned(),
        };
        let rows = rows(transcript(std::slice::from_ref(&outcome), Some(&outcome)));
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn a_teardown_failure_never_shows_the_filesystems_stay_attached_line() {
        let outcomes = vec![TeardownOutcome::DaemonShutdownFailed {
            error: "busy".to_owned(),
        }];
        let rows = rows(transcript(&outcomes, None));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].value.contains("shutdown failed"), "{rows:?}");
    }

    #[test]
    fn successful_rpc_shutdown_needs_no_missing_identity_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = Bootstrap::<Client>::under_root(root.path());
        let teardown = DaemonTeardown {
            rpc: crate::rpc::RpcClient::from_endpoint(endpoint.clone()),
            endpoint,
            initial_identity: None,
            initial: None,
            stop_filesystems: true,
        };
        teardown.remove_identity_for_expected_process(42).unwrap();
    }
}
