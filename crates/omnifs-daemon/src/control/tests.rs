//! Control-plane integration tests: one live daemon over a real socket.

use super::*;
use crate::daemon::DaemonParts;
use hyper_util::rt::TokioIo;
use omnifs_api::{CredentialStatusKind, ProviderImportDisposition};
use std::time::Duration;
use tokio_stream::StreamExt as _;
use tonic::transport::Endpoint;
use tower::service_fn;

type Client = wire::control_client::ControlClient<tonic::transport::Channel>;

async fn client(path: &std::path::Path) -> Client {
    try_client(path).await.unwrap()
}

async fn try_client(path: &std::path::Path) -> Result<Client, tonic::transport::Error> {
    let socket = path.to_owned();
    let channel = Endpoint::try_from("http://[::]:50051")
        .expect("valid test endpoint")
        .connect_with_connector(service_fn(move |_| {
            let socket = socket.clone();
            async move {
                tokio::net::UnixStream::connect(socket)
                    .await
                    .map(TokioIo::new)
            }
        }))
        .await?;
    Ok(Client::new(channel))
}

fn provider_wasm() -> Vec<u8> {
    let metadata = serde_json::to_vec(&serde_json::json!({
        "id": "demo",
        "displayName": "Demo",
        "description": "A test provider",
        "provider": "demo.wasm",
        "defaultMount": "demo",
        "projection": 1,
        "capabilities": [{
            "kind": "domain",
            "value": "api.demo.test",
            "why": "Test credential injection."
        }],
        "auth": {
            "default": "pat",
            "schemes": [{
                "staticToken": {
                    "key": "pat",
                    "valuePrefix": "Bearer ",
                    "description": "Demo token",
                    "injectDomains": ["api.demo.test"]
                }
            }]
        }
    }))
    .unwrap();
    let name = omnifs_provider::PROVIDER_METADATA_SECTION_NAME.as_bytes();
    let mut payload = Vec::new();
    append_uleb(&mut payload, name.len());
    payload.extend_from_slice(name);
    payload.extend_from_slice(&metadata);

    let mut wasm = b"\0asm\x01\0\0\0".to_vec();
    wasm.push(0);
    append_uleb(&mut wasm, payload.len());
    wasm.extend_from_slice(&payload);
    wasm
}

fn append_uleb(output: &mut Vec<u8>, mut value: usize) {
    loop {
        let byte = u8::try_from(value & 0x7f).unwrap();
        value >>= 7;
        if value == 0 {
            output.push(byte);
            return;
        }
        output.push(byte | 0x80);
    }
}

fn begin_request(id: omnifs_core::MutationId) -> wire::BeginMutationRequest {
    wire::BeginMutationRequest {
        mutation_id: id.as_bytes().to_vec().into(),
    }
}

fn apply_request(
    id: omnifs_core::MutationId,
    ops: &[omnifs_api::MutationOp],
) -> wire::ApplyMutationRequest {
    wire::ApplyMutationRequest {
        mutation_id: id.as_bytes().to_vec().into(),
        ops: ops.iter().map(grpc::to_mutation_op).collect(),
    }
}

fn drop_request(id: omnifs_core::MutationId) -> wire::DropMutationRequest {
    wire::DropMutationRequest {
        mutation_id: id.as_bytes().to_vec().into(),
    }
}

/// Decode the structured error code an aborted/failed-precondition status
/// carries in its details, so tests assert on the domain code rather than
/// just the transport-level `tonic::Code`.
fn error_code(status: &tonic::Status) -> omnifs_api::ControlErrorCode {
    grpc::error_detail(&wire::ErrorDetail::decode(status.details()).unwrap())
        .unwrap()
        .code
}

async fn import_provider_bytes(
    c: &mut Client,
    bytes: Vec<u8>,
) -> omnifs_api::ProviderImportReceipt {
    let digest = omnifs_core::ProviderId::from_wasm_bytes(&bytes);
    let start = grpc::to_provider_upload_start("demo.wasm", bytes.len() as u64, &digest);
    let upload = tokio_stream::iter([
        wire::ImportProviderRequest {
            value: Some(wire::import_provider_request::Value::Start(start)),
        },
        wire::ImportProviderRequest {
            value: Some(wire::import_provider_request::Value::Chunk(bytes.into())),
        },
    ]);
    let response = c.import_provider(upload).await.unwrap().into_inner();
    grpc::provider_import_receipt(response.receipt.as_ref().unwrap()).unwrap()
}

async fn import_test_provider(c: &mut Client, bytes: Vec<u8>) -> omnifs_core::ProviderId {
    import_provider_bytes(c, bytes).await.provider.id
}

/// A daemon names the control protocol of its build in its info, in each
/// phase, and answers a request for its state after the begin of its
/// teardown with the error that it is shutting down.
#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn a_daemon_names_its_control_protocol_and_its_teardown() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let protocol =
        |info: wire::DaemonInfoResponse| grpc::daemon_info(&info.info.unwrap()).unwrap().protocol;
    assert_eq!(
        protocol(
            c.get_daemon_info(wire::Empty {})
                .await
                .unwrap()
                .into_inner()
        ),
        omnifs_api::CONTROL_PROTOCOL
    );
    c.get_inventory(wire::Empty {}).await.unwrap();

    runtime.control.set_shutting_down();
    assert_eq!(
        protocol(
            c.get_daemon_info(wire::Empty {})
                .await
                .unwrap()
                .into_inner()
        ),
        omnifs_api::CONTROL_PROTOCOL
    );
    let status = c.get_inventory(wire::Empty {}).await.unwrap_err();
    assert_eq!(error_code(&status), ControlErrorCode::ShuttingDown);
    let status = c.get_recovery_state(wire::Empty {}).await.unwrap_err();
    assert_eq!(error_code(&status), ControlErrorCode::ShuttingDown);
    runtime.shutdown().await;
}

/// The begin and the end of a sync of a mount that the daemon does not
/// serve are errors of a mount that is not found.
#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn a_sync_of_a_mount_that_the_daemon_does_not_serve_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let status = c
        .begin_sync(wire::BeginSyncRequest {
            mount: "missing".to_owned(),
        })
        .await
        .unwrap_err();
    assert_eq!(error_code(&status), ControlErrorCode::NotFound);
    let status = c
        .end_sync(wire::EndSyncRequest {
            mount: "missing".to_owned(),
            sync: 1,
        })
        .await
        .unwrap_err();
    assert_eq!(error_code(&status), ControlErrorCode::NotFound);
    runtime.shutdown().await;
}

/// The daemon answers a message of any size: no limit of a control message
/// bounds an answer, as the mounts of a large config.
#[test]
fn a_reply_of_any_size_is_an_answer_of_the_daemon() {
    let mount = |config: usize| wire::ListMountsResponse {
        mounts: vec![wire::MountRecord {
            definition: Some(wire::MountDefinition {
                config: vec![0; config].into(),
                ..wire::MountDefinition::default()
            }),
            ..wire::MountRecord::default()
        }],
    };
    assert!(super::reply(mount(1024)).is_ok());
    assert!(super::reply(mount(8 * 1024 * 1024)).is_ok());
}

#[test]
fn manager_recovery_required_maps_to_recovery_required() {
    let error = crate::manager::ManagerError::RecoveryRequired("blocked".to_owned());
    assert_eq!(
        super::manager_error(&error).code,
        ControlErrorCode::RecoveryRequired
    );
}

struct TestRuntime {
    daemon: Arc<super::Daemon>,
    control: Arc<super::ControlServer>,
    shutdown: tokio::sync::watch::Sender<bool>,
    control_task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl TestRuntime {
    async fn shutdown(self) {
        self.control.set_shutting_down();
        let _ = self.shutdown.send(true);
        self.control_task.await.unwrap().unwrap();
        self.daemon.shutdown().await.unwrap();
    }
}

async fn test_daemon(dir: &tempfile::TempDir) -> TestRuntime {
    test_daemon_with_limits(
        dir,
        CONTROL_CONNECTION_LIMIT,
        CONTROL_PREFACE_TIMEOUT,
        Duration::from_secs(30),
    )
    .await
}

/// A daemon whose mutation lease is short enough that lease-expiry tests can
/// wait it out with a real (short) sleep instead of a real 30-second one.
async fn test_daemon_with_lease(dir: &tempfile::TempDir, lease: Duration) -> TestRuntime {
    test_daemon_with_limits(
        dir,
        CONTROL_CONNECTION_LIMIT,
        CONTROL_PREFACE_TIMEOUT,
        lease,
    )
    .await
}

async fn test_daemon_with_limits(
    dir: &tempfile::TempDir,
    connection_limit: usize,
    preface_timeout: std::time::Duration,
    lease: Duration,
) -> TestRuntime {
    let runtime = test_runtime(dir, connection_limit, preface_timeout, lease).await;
    runtime.control.set_ready(Arc::clone(&runtime.daemon));
    runtime
}

/// A daemon whose control is `Starting` until the test sets it ready.
async fn test_runtime(
    dir: &tempfile::TempDir,
    connection_limit: usize,
    preface_timeout: std::time::Duration,
    lease: Duration,
) -> TestRuntime {
    test_runtime_with_bounds(
        dir,
        connection_limit,
        preface_timeout,
        lease,
        omnifs_core::SILENT_PEER_TIMEOUT,
        omnifs_core::SILENT_PEER_TIMEOUT,
    )
    .await
}

/// [`test_runtime`] whose provider upload fails after `upload_silence`
/// without bytes, and whose shutdown waits `drain_timeout` for its requests
/// and its filesystems.
async fn test_runtime_with_bounds(
    dir: &tempfile::TempDir,
    connection_limit: usize,
    preface_timeout: std::time::Duration,
    lease: Duration,
    upload_silence: Duration,
    drain_timeout: Duration,
) -> TestRuntime {
    let _ = dir;
    let context = Arc::new(crate::context::DaemonContext::resolve().unwrap());
    context.prepare_startup_dirs().unwrap();
    let listener = context.bind_control_socket().unwrap();
    let state = Arc::new(
        omnifs_state::StateStore::open(
            context.endpoint(),
            omnifs_state::StateStoreOptions::default(),
        )
        .await
        .unwrap(),
    );
    let paths = state.engine_paths();
    let host = Arc::new(
        omnifs_engine::HostOnline::open_runtime(omnifs_engine::HostRuntimeOpen {
            projection: paths.projection_cache().to_path_buf(),
            wasmtime: paths.wasmtime_cache().to_path_buf(),
            clones: paths.clone_cache().to_path_buf(),
        })
        .unwrap(),
    );
    let draft = crate::generation_builder::GenerationDraft::load(&state)
        .await
        .unwrap();
    let parts = draft.prepare(&state, &host).await.unwrap().into_parts();
    let serving =
        omnifs_engine::ServingCell::new(context.namespace_epoch().daemon_instance(), parts.ready);
    parts.pending_refreshes.activate(&state).await.unwrap();
    state.mark_serving(parts.revision).await.unwrap();
    let manager = crate::manager::MutationManager::spawn_with_bounds(
        Arc::clone(&state),
        host,
        Arc::clone(&serving),
        crate::manager::ManagerBounds {
            lease,
            ..crate::manager::ManagerBounds::SILENT_PEER
        },
    );
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let embedded = Arc::new(super::EmbeddedProviders::default());
    let daemon = Arc::new(super::Daemon::new(DaemonParts {
        context: Arc::clone(&context),
        embedded: Arc::clone(&embedded),
        state,
        serving,
        manager,
        inspector: None,
        shutdown_tx: shutdown.clone(),
        drain_timeout,
    }));
    let (control, _repairs) = super::ControlServer::new_with_limits(
        Arc::clone(&context),
        embedded,
        shutdown.clone(),
        connection_limit,
        preface_timeout,
        upload_silence,
    );
    let control_task = tokio::spawn(Arc::clone(&control).run(listener));
    TestRuntime {
        daemon,
        control,
        shutdown,
        control_task,
    }
}

#[tokio::test]
#[allow(unsafe_code)]
async fn tonic_reports_starting_state_and_invalid_requests() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let context = Arc::new(crate::context::DaemonContext::resolve().unwrap());
    context.prepare_startup_dirs().unwrap();
    let listener = context.bind_control_socket().unwrap();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let (control, _repairs) = super::ControlServer::new(
        Arc::clone(&context),
        Arc::new(super::EmbeddedProviders::default()),
        shutdown.clone(),
    );
    let task = tokio::spawn(Arc::clone(&control).run(listener));
    let mut c = client(&context.control_socket()).await;
    let recovery = c
        .get_recovery_state(wire::Empty {})
        .await
        .unwrap()
        .into_inner()
        .recovery
        .unwrap();
    assert_eq!(
        grpc::daemon_recovery(&recovery).unwrap().phase,
        DaemonPhase::Starting
    );
    let status = c.ready(wire::Empty {}).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    let invalid = c
        .repair_state(wire::RepairStateRequest::default())
        .await
        .unwrap_err();
    assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
    let _ = shutdown.send(true);
    task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn idle_control_peer_releases_connection_capacity_after_preface_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon_with_limits(
        &dir,
        1,
        std::time::Duration::from_secs(2),
        Duration::from_secs(30),
    )
    .await;
    let idle = tokio::net::UnixStream::connect(home.join("control.sock"))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while runtime.control.connection_permits.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("idle peer did not consume connection capacity");

    let mut rejected = tokio::net::UnixStream::connect(home.join("control.sock"))
        .await
        .unwrap();
    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout(std::time::Duration::from_secs(1), rejected.read(&mut byte))
        .await
        .expect("connection above the capacity limit stayed open")
        .expect("read rejected connection");
    assert_eq!(
        read, 0,
        "connection above the capacity limit was not closed"
    );

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while runtime.control.connection_permits.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("idle peer did not release connection capacity");
    let mut valid = client(&home.join("control.sock")).await;
    valid.get_status(wire::Empty {}).await.unwrap();
    drop(idle);
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn established_tonic_channel_remains_usable_after_preface_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon_with_limits(
        &dir,
        1,
        std::time::Duration::from_millis(20),
        Duration::from_secs(30),
    )
    .await;
    let mut c = client(&home.join("control.sock")).await;
    c.get_status(wire::Empty {}).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    c.get_status(wire::Empty {}).await.unwrap();
    runtime.shutdown().await;
}

#[tokio::test]
#[allow(unsafe_code)]
async fn tonic_log_stream_preserves_raw_tail() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let log = runtime.daemon.state.daemon_log_path();
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, b"one\xff\ntwo\nthree\n").unwrap();
    let mut c = client(&home.join("control.sock")).await;
    let mut stream = c
        .stream_logs(wire::StreamLogsRequest {
            follow: false,
            tail_lines: 3,
        })
        .await
        .unwrap()
        .into_inner();
    let ready = stream.next().await.unwrap().unwrap();
    assert!(matches!(
        ready.value,
        Some(wire::log_stream_item::Value::Ready(_))
    ));
    let data = stream.next().await.unwrap().unwrap();
    match data.value {
        Some(wire::log_stream_item::Value::Data(bytes)) => {
            assert_eq!(&bytes[..], b"one\xff\ntwo\nthree\n");
        },
        _ => panic!("missing log data"),
    }
    assert!(stream.next().await.is_none());
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn tonic_provider_upload_reports_inserted_then_unchanged_disposition() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let bytes = provider_wasm();
    let first = import_provider_bytes(&mut c, bytes.clone()).await;
    assert_eq!(first.disposition, ProviderImportDisposition::Inserted);
    let duplicate = import_provider_bytes(&mut c, bytes).await;
    assert_eq!(duplicate.disposition, ProviderImportDisposition::Unchanged);
    assert_eq!(duplicate.provider, first.provider);
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn tonic_followed_log_stream_ends_cleanly_when_daemon_stops() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let log = runtime.daemon.state.daemon_log_path();
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, b"tail\n").unwrap();
    let mut c = client(&home.join("control.sock")).await;
    let mut stream = c
        .stream_logs(wire::StreamLogsRequest {
            follow: true,
            tail_lines: 1,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(matches!(
        stream.next().await.unwrap().unwrap().value,
        Some(wire::log_stream_item::Value::Ready(_))
    ));
    assert!(matches!(
        stream.next().await.unwrap().unwrap().value,
        Some(wire::log_stream_item::Value::Data(data)) if &data[..] == b"tail\n"
    ));
    runtime.shutdown().await;
    assert!(stream.next().await.is_none());
}

/// The next phase of a `WatchPhase` stream.
async fn next_phase(phases: &mut tonic::Streaming<wire::PhaseEvent>) -> omnifs_api::PhaseEvent {
    grpc::phase_event(&phases.next().await.unwrap().unwrap()).unwrap()
}

/// `WatchPhase` gives a running daemon whose store requires recovery as
/// `recovery_required` with the error of its store, to a subscription at
/// the change and to one after it, and as `ready` again once its store
/// serves.
#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn watch_phase_gives_a_running_daemon_with_the_recovery_of_its_store() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let mut phases = c.watch_phase(wire::Empty {}).await.unwrap().into_inner();
    assert_eq!(next_phase(&mut phases).await, omnifs_api::PhaseEvent::Ready);

    let state = Arc::clone(&runtime.daemon.state);
    state
        .mark_recovery_required(None, "a publish failed".to_owned())
        .await
        .unwrap();
    let recovery = omnifs_api::PhaseEvent::RecoveryRequired {
        error: "a publish failed".to_owned(),
    };
    assert_eq!(next_phase(&mut phases).await, recovery);
    let mut later = c.watch_phase(wire::Empty {}).await.unwrap().into_inner();
    assert_eq!(next_phase(&mut later).await, recovery);

    let revision = state.mount_revision().await.unwrap();
    state.mark_serving(revision).await.unwrap();
    assert_eq!(next_phase(&mut phases).await, omnifs_api::PhaseEvent::Ready);
    assert_eq!(next_phase(&mut later).await, omnifs_api::PhaseEvent::Ready);
    runtime.shutdown().await;
}

/// `WatchPhase` gives a starting daemon as `starting`, then its readiness
/// once, however often the daemon sets it, and ends with the daemon.
#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn watch_phase_gives_a_starting_daemon_then_its_readiness_once() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_runtime(
        &dir,
        CONTROL_CONNECTION_LIMIT,
        CONTROL_PREFACE_TIMEOUT,
        Duration::from_secs(30),
    )
    .await;
    let mut c = client(&home.join("control.sock")).await;
    let mut phases = c.watch_phase(wire::Empty {}).await.unwrap().into_inner();
    assert_eq!(
        next_phase(&mut phases).await,
        omnifs_api::PhaseEvent::Starting
    );
    runtime.control.set_ready(Arc::clone(&runtime.daemon));
    assert_eq!(next_phase(&mut phases).await, omnifs_api::PhaseEvent::Ready);
    runtime.control.set_ready(Arc::clone(&runtime.daemon));
    runtime.shutdown().await;
    assert!(phases.next().await.is_none());
}

/// `WatchPhase` gives a ready daemon as `ready` at the subscription.
#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn watch_phase_gives_a_ready_daemon_at_the_subscription() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let mut phases = c.watch_phase(wire::Empty {}).await.unwrap().into_inner();
    assert_eq!(next_phase(&mut phases).await, omnifs_api::PhaseEvent::Ready);
    runtime.shutdown().await;
    assert!(phases.next().await.is_none());
}

/// `WatchPhase` gives a daemon that requires recovery with the error that
/// the store health of its recovery reports.
#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn watch_phase_gives_a_recovering_daemon_with_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let context = Arc::new(crate::context::DaemonContext::resolve().unwrap());
    context.prepare_startup_dirs().unwrap();
    let listener = context.bind_control_socket().unwrap();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let (control, _repairs) = super::ControlServer::new(
        Arc::clone(&context),
        Arc::new(super::EmbeddedProviders::default()),
        shutdown.clone(),
    );
    let task = tokio::spawn(Arc::clone(&control).run(listener));
    let mut c = client(&context.control_socket()).await;
    let mut phases = c.watch_phase(wire::Empty {}).await.unwrap().into_inner();
    assert_eq!(
        next_phase(&mut phases).await,
        omnifs_api::PhaseEvent::Starting
    );
    let error = "control store could not be opened: file is not a database";
    control.set_recovery(DaemonRecovery {
        phase: DaemonPhase::RecoveryRequired,
        durable_revision: None,
        serving_revision: None,
        failed_mutation: None,
        store_health: HealthReport::new(HealthState::Unhealthy, error),
        repair: None,
    });
    assert_eq!(
        next_phase(&mut phases).await,
        omnifs_api::PhaseEvent::RecoveryRequired {
            error: error.to_owned()
        }
    );
    control.set_shutting_down();
    let _ = shutdown.send(true);
    task.await.unwrap().unwrap();
    assert!(phases.next().await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn tonic_lists_and_deletes_credentials_without_returning_material() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let provider = import_test_provider(&mut c, provider_wasm()).await;
    let key = omnifs_api::CredentialKey {
        provider_name: "demo".to_owned(),
        scheme: "pat".to_owned(),
        account_label: "work".to_owned(),
    };
    let submission = omnifs_api::CredentialSubmission {
        provider,
        scheme: key.scheme.clone(),
        account_label: key.account_label.clone(),
        material: omnifs_api::CredentialMaterial::StaticToken {
            token: omnifs_api::SecretBytes::new(b"never-return-this".to_vec()),
        },
        overrides: omnifs_api::CredentialClientOverrides {
            client_id: None,
            client_secret: None,
            redirect_uri: None,
            scopes: None,
        },
    };

    let submit_id = omnifs_core::MutationId::from_bytes([9; 16]);
    c.begin_mutation(begin_request(submit_id)).await.unwrap();
    let response = c
        .apply_mutation(apply_request(
            submit_id,
            &[omnifs_api::MutationOp::SubmitCredential(submission)],
        ))
        .await
        .unwrap()
        .into_inner();
    let submitted = match grpc::mutation_op_result(&response.results[0]).unwrap() {
        omnifs_api::MutationOpResult::Credential(status) => status,
        other @ omnifs_api::MutationOpResult::Mount(_) => {
            panic!("expected a credential result, got {other:?}")
        },
    };
    assert_eq!(submitted.last_mutation_id, submit_id);

    let recovery = c
        .get_recovery_state(wire::Empty {})
        .await
        .unwrap()
        .into_inner()
        .recovery
        .unwrap();
    assert_eq!(
        grpc::daemon_recovery(&recovery).unwrap().phase,
        DaemonPhase::Ready,
        "an unreferenced credential still activates a (no-op) serving generation"
    );

    let listed = c
        .list_credentials(wire::Empty {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.credentials.len(), 1);
    assert_eq!(
        grpc::credential_status(&listed.credentials[0]).unwrap(),
        submitted
    );
    assert!(!format!("{listed:?}").contains("never-return-this"));

    let delete_id = omnifs_core::MutationId::from_bytes([10; 16]);
    c.begin_mutation(begin_request(delete_id)).await.unwrap();
    let response = c
        .apply_mutation(apply_request(
            delete_id,
            &[omnifs_api::MutationOp::DeleteCredential(key)],
        ))
        .await
        .unwrap()
        .into_inner();
    let deleted = match grpc::mutation_op_result(&response.results[0]).unwrap() {
        omnifs_api::MutationOpResult::Credential(status) => status,
        other @ omnifs_api::MutationOpResult::Mount(_) => {
            panic!("expected a credential result, got {other:?}")
        },
    };
    assert_eq!(deleted.status, CredentialStatusKind::Deleted);
    assert_eq!(deleted.last_mutation_id, delete_id);
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn tonic_inventory_and_status_use_generated_messages() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let _ = runtime.daemon.start().await.unwrap();
    c.ready(wire::Empty {}).await.unwrap();
    let inventory = c
        .get_inventory(wire::Empty {})
        .await
        .unwrap()
        .into_inner()
        .inventory
        .unwrap();
    assert_eq!(
        grpc::daemon_inventory(&inventory).unwrap().phase,
        DaemonPhase::Ready
    );
    let inventory = grpc::daemon_inventory(&inventory).unwrap();
    assert_eq!(inventory.durable_revision, inventory.serving_revision);
    assert!(inventory.mounts.is_empty());
    assert!(inventory.credentials.is_empty());
    assert!(inventory.attachments.is_empty());
    let status = c
        .get_status(wire::Empty {})
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    let status = grpc::daemon_status(&status).unwrap();
    assert!(status.mounts.is_empty());
    assert!(status.active_mutation.is_none());

    // A store that requires recovery under a running daemon reports the
    // error of the recovery, in its recovery state and in its inventory.
    runtime
        .daemon
        .state
        .mark_recovery_required(None, "a publish failed".to_owned())
        .await
        .unwrap();
    let recovery = c
        .get_recovery_state(wire::Empty {})
        .await
        .unwrap()
        .into_inner()
        .recovery
        .unwrap();
    let recovery = grpc::daemon_recovery(&recovery).unwrap();
    assert_eq!(recovery.phase, DaemonPhase::RecoveryRequired);
    assert_eq!(recovery.store_health.state, HealthState::Unhealthy);
    assert_eq!(recovery.store_health.message, "a publish failed");
    let inventory = c
        .get_inventory(wire::Empty {})
        .await
        .unwrap()
        .into_inner()
        .inventory
        .unwrap();
    let inventory = grpc::daemon_inventory(&inventory).unwrap();
    assert_eq!(inventory.phase, DaemonPhase::RecoveryRequired);
    assert_eq!(inventory.health.mounts.state, HealthState::Unhealthy);
    assert_eq!(inventory.health.mounts.message, "a publish failed");
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn begin_while_held_rejects_with_mutation_in_progress() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let first = omnifs_core::MutationId::from_bytes([1; 16]);
    let second = omnifs_core::MutationId::from_bytes([2; 16]);
    c.begin_mutation(begin_request(first)).await.unwrap();
    let error = c.begin_mutation(begin_request(second)).await.unwrap_err();
    assert_eq!(error.code(), tonic::Code::Aborted);
    assert_eq!(
        error_code(&error),
        omnifs_api::ControlErrorCode::MutationInProgress
    );
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn expired_lease_is_stolen_by_next_begin() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon_with_lease(&dir, Duration::from_millis(30)).await;
    let mut c = client(&home.join("control.sock")).await;
    let first = omnifs_core::MutationId::from_bytes([1; 16]);
    c.begin_mutation(begin_request(first)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    let second = omnifs_core::MutationId::from_bytes([2; 16]);
    c.begin_mutation(begin_request(second)).await.unwrap();
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn apply_after_lease_expiry_rejects_with_lease_expired() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon_with_lease(&dir, Duration::from_millis(30)).await;
    let mut c = client(&home.join("control.sock")).await;
    let id = omnifs_core::MutationId::from_bytes([1; 16]);
    c.begin_mutation(begin_request(id)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    let error = c.apply_mutation(apply_request(id, &[])).await.unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error_code(&error),
        omnifs_api::ControlErrorCode::LeaseExpired
    );
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn apply_without_begin_rejects_with_lease_not_held() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let id = omnifs_core::MutationId::from_bytes([1; 16]);
    let error = c.apply_mutation(apply_request(id, &[])).await.unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error_code(&error),
        omnifs_api::ControlErrorCode::LeaseNotHeld
    );
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn drop_mutation_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let id = omnifs_core::MutationId::from_bytes([1; 16]);
    // Dropping a mutation that never began succeeds vacuously.
    c.drop_mutation(drop_request(id)).await.unwrap();
    c.begin_mutation(begin_request(id)).await.unwrap();
    c.drop_mutation(drop_request(id)).await.unwrap();
    c.drop_mutation(drop_request(id)).await.unwrap();
    // The slot is free again: another id can begin immediately.
    let other = omnifs_core::MutationId::from_bytes([2; 16]);
    c.begin_mutation(begin_request(other)).await.unwrap();
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn status_shows_active_mutation_while_held() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let id = omnifs_core::MutationId::from_bytes([1; 16]);
    c.begin_mutation(begin_request(id)).await.unwrap();
    let status = c
        .get_status(wire::Empty {})
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    let status = grpc::daemon_status(&status).unwrap();
    assert_eq!(status.active_mutation.unwrap().mutation_id, id);
    c.drop_mutation(drop_request(id)).await.unwrap();
    let status = c
        .get_status(wire::Empty {})
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    let status = grpc::daemon_status(&status).unwrap();
    assert!(status.active_mutation.is_none());
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn two_op_batch_applies_atomically_and_reports_the_batch_id() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let provider = import_test_provider(&mut c, provider_wasm()).await;

    let submission = omnifs_api::CredentialSubmission {
        provider,
        scheme: "pat".to_owned(),
        account_label: "work".to_owned(),
        material: omnifs_api::CredentialMaterial::StaticToken {
            token: omnifs_api::SecretBytes::new(b"batched-secret".to_vec()),
        },
        overrides: omnifs_api::CredentialClientOverrides {
            client_id: None,
            client_secret: None,
            redirect_uri: None,
            scopes: None,
        },
    };
    let definition = omnifs_api::MountDefinition {
        name: omnifs_core::MountName::new("demo").unwrap(),
        provider,
        auth: Some(omnifs_api::MountCredential {
            scheme: "pat".to_owned(),
            account_label: "work".to_owned(),
        }),
        limits: None,
        config: b"{}".to_vec(),
    };

    let id = omnifs_core::MutationId::from_bytes([42; 16]);
    c.begin_mutation(begin_request(id)).await.unwrap();
    let response = c
        .apply_mutation(apply_request(
            id,
            &[
                omnifs_api::MutationOp::SubmitCredential(submission),
                omnifs_api::MutationOp::CreateMount(definition),
            ],
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.results.len(), 2);
    // The batch's durable commit is unconditional and happens before
    // generation activation is attempted; this test's provider fixture is a
    // metadata-only stub, not a real WASM component, so activation itself is
    // expected to fail here. That is an engine-level concern, not the batch
    // atomicity and provenance this test exists to check.

    let mounts = c.list_mounts(wire::Empty {}).await.unwrap().into_inner();
    assert_eq!(mounts.mounts.len(), 1);
    assert_eq!(
        grpc::mount_record(&mounts.mounts[0])
            .unwrap()
            .last_mutation_id,
        id
    );
    let credentials = c
        .list_credentials(wire::Empty {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(credentials.credentials.len(), 1);
    assert_eq!(
        grpc::credential_status(&credentials.credentials[0])
            .unwrap()
            .last_mutation_id,
        id
    );
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn batch_second_op_failure_leaves_no_first_op_row_behind() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let provider = import_test_provider(&mut c, provider_wasm()).await;
    let definition = omnifs_api::MountDefinition {
        name: omnifs_core::MountName::new("demo").unwrap(),
        provider,
        auth: None,
        limits: None,
        config: b"{}".to_vec(),
    };

    let id = omnifs_core::MutationId::from_bytes([7; 16]);
    c.begin_mutation(begin_request(id)).await.unwrap();
    let error = c
        .apply_mutation(apply_request(
            id,
            &[
                omnifs_api::MutationOp::CreateMount(definition),
                omnifs_api::MutationOp::RemoveMount {
                    name: omnifs_core::MountName::new("missing").unwrap(),
                },
            ],
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::NotFound);

    let mounts = c.list_mounts(wire::Empty {}).await.unwrap().into_inner();
    assert!(
        mounts.mounts.is_empty(),
        "the first op's row must not survive the second op's failure"
    );

    // The lease released on failure too: a fresh mutation can begin.
    let next = omnifs_core::MutationId::from_bytes([8; 16]);
    c.begin_mutation(begin_request(next)).await.unwrap();
    runtime.shutdown().await;
}

/// A provider upload whose client sends its start and then holds its stream
/// open without bytes fails after the silence of an upload, with the error
/// that it sent no bytes.
#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn a_provider_upload_without_bytes_fails_after_its_silence() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_runtime_with_bounds(
        &dir,
        CONTROL_CONNECTION_LIMIT,
        CONTROL_PREFACE_TIMEOUT,
        Duration::from_secs(30),
        Duration::from_millis(100),
        omnifs_core::SILENT_PEER_TIMEOUT,
    )
    .await;
    runtime.control.set_ready(Arc::clone(&runtime.daemon));
    let mut c = client(&home.join("control.sock")).await;
    let bytes = provider_wasm();
    let digest = omnifs_core::ProviderId::from_wasm_bytes(&bytes);
    let start = grpc::to_provider_upload_start("demo.wasm", bytes.len() as u64, &digest);
    let first = wire::ImportProviderRequest {
        value: Some(wire::import_provider_request::Value::Start(start)),
    };
    // The start, then a stream that stays open and sends nothing more.
    let held = tokio_stream::StreamExt::chain(
        tokio_stream::iter([first]),
        tokio_stream::pending::<wire::ImportProviderRequest>(),
    );
    let status = c.import_provider(held).await.unwrap_err();
    assert!(
        status.message().contains("sent no bytes for"),
        "{}",
        status.message()
    );
    runtime.shutdown().await;
}

/// A daemon in a shutdown waits for a request that a silent peer holds for
/// its drain timeout, and then cancels it.
#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn a_shutdown_cancels_a_silent_request_after_its_drain_timeout() {
    use omnifs_vfs::ServingNamespace as _;
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let drain = Duration::from_millis(200);
    let runtime = test_runtime_with_bounds(
        &dir,
        CONTROL_CONNECTION_LIMIT,
        CONTROL_PREFACE_TIMEOUT,
        omnifs_core::SILENT_PEER_TIMEOUT,
        omnifs_core::SILENT_PEER_TIMEOUT,
        drain,
    )
    .await;
    runtime.control.set_ready(Arc::clone(&runtime.daemon));
    let lease = runtime.daemon.serving.acquire().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let request = tokio::spawn(async move {
        lease
            .run(async move {
                let _ = started_tx.send(());
                std::future::pending::<Result<(), omnifs_vfs::NsError>>().await
            })
            .await
    });
    started_rx.await.unwrap();
    let started = tokio::time::Instant::now();
    tokio::time::timeout(drain * 25, runtime.shutdown())
        .await
        .expect("the silent request held the shutdown");
    assert!(started.elapsed() >= drain);
    assert_eq!(request.await.unwrap(), Err(omnifs_vfs::NsError::Network));
}

/// A shutdown that stops the filesystems waits for a busy one for its drain
/// timeout, and then answers with it still attached.
#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn a_shutdown_answers_with_a_busy_filesystem_after_its_drain_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let drain = Duration::from_millis(200);
    let runtime = test_runtime_with_bounds(
        &dir,
        CONTROL_CONNECTION_LIMIT,
        CONTROL_PREFACE_TIMEOUT,
        omnifs_core::SILENT_PEER_TIMEOUT,
        omnifs_core::SILENT_PEER_TIMEOUT,
        drain,
    )
    .await;
    runtime.control.set_ready(Arc::clone(&runtime.daemon));
    let socket = home.join("attach-test.sock");
    runtime.daemon.vfs.serve_unix(&socket).unwrap();
    let identity = omnifs_core::fs::Spec::new(
        "test".parse().unwrap(),
        omnifs_core::fs::Protocol::Fuse,
        omnifs_core::fs::Runtime::Host,
        std::path::PathBuf::from("/mnt/test"),
    )
    .unwrap();
    let (teardown_tx, mut teardown_rx) = tokio::sync::mpsc::channel(1);
    let namespace = omnifs_vfs::WireNamespace::attach_with_teardown(
        omnifs_vfs::AttachTarget::Unix(socket),
        "0123456789abcdef0123456789abcdef".parse().unwrap(),
        identity.clone(),
        tokio::runtime::Handle::current(),
        teardown_tx,
    )
    .await
    .unwrap();
    let busy = tokio::spawn(async move {
        let request = teardown_rx.recv().await.unwrap();
        request.complete(omnifs_vfs::TeardownOutcome::Busy);
    });
    let mut c = client(&home.join("control.sock")).await;
    let started = tokio::time::Instant::now();
    let answer = tokio::time::timeout(
        drain * 25,
        c.shutdown(wire::ShutdownRequest {
            stop_filesystems: true,
        }),
    )
    .await
    .expect("the busy filesystem held the shutdown")
    .unwrap()
    .into_inner();
    assert!(started.elapsed() >= drain);
    assert_eq!(answer.still_attached, vec![identity.to_string()]);
    busy.await.unwrap();
    drop(namespace);
    runtime.shutdown().await;
}

/// A control connection whose peer sends no preface ends once it is silent
/// for the bound of a silent peer, and not before.
#[tokio::test(start_paused = true)]
async fn a_control_connection_without_a_preface_ends_after_the_bound_of_a_silent_peer() {
    let (inner, _peer) = tokio::net::UnixStream::pair().unwrap();
    let permit = Arc::new(tokio::sync::Semaphore::new(1))
        .try_acquire_owned()
        .unwrap();
    let started = tokio::time::Instant::now();
    let prepared = tokio::time::timeout(
        omnifs_core::SILENT_PEER_TIMEOUT * 2,
        super::prepare_control_connection(inner, permit, CONTROL_PREFACE_TIMEOUT),
    )
    .await
    .expect("the silent peer held its control connection");
    assert!(prepared.is_err());
    let waited = started.elapsed();
    assert!(
        waited >= omnifs_core::SILENT_PEER_TIMEOUT
            && waited < omnifs_core::SILENT_PEER_TIMEOUT + Duration::from_secs(1),
        "{waited:?}"
    );
}

/// The attachments stream of a daemon gives an attachment at the attach.
#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn the_attachments_stream_gives_an_attachment_at_the_attach() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let socket = home.join("attach-test.sock");
    runtime.daemon.vfs.serve_unix(&socket).unwrap();
    let mut c = client(&home.join("control.sock")).await;
    let mut attachments = c
        .watch_attachments(wire::Empty {})
        .await
        .unwrap()
        .into_inner();
    let first = attachments.next().await.unwrap().unwrap();
    assert!(first.attachments.is_empty());
    let identity = omnifs_core::fs::Spec::new(
        "test".parse().unwrap(),
        omnifs_core::fs::Protocol::Fuse,
        omnifs_core::fs::Runtime::Host,
        std::path::PathBuf::from("/mnt/test"),
    )
    .unwrap();
    let namespace = omnifs_vfs::WireNamespace::attach(
        omnifs_vfs::AttachTarget::Unix(socket),
        "0123456789abcdef0123456789abcdef".parse().unwrap(),
        identity.clone(),
        tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    let attached = tokio::time::timeout(Duration::from_secs(5), attachments.next())
        .await
        .expect("the stream outlived the attach")
        .unwrap()
        .unwrap();
    assert_eq!(
        omnifs_api::grpc::attachments_event(&attached).unwrap(),
        vec![identity]
    );
    drop(attachments);
    drop(namespace);
    runtime.shutdown().await;
}
