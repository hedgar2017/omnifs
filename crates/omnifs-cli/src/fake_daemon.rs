//! A daemon of the tests of the control client: a control service on the
//! control socket of a profile that answers the info and the inventory of a
//! daemon as its test sets them, counts the mutations that begin, and
//! answers each other request as unimplemented.

use omnifs_api::grpc::{self, wire};
use omnifs_api::{ControlError, DaemonInfo, DaemonInventory};
use omnifs_bootstrap::{Bootstrap, Client};
use prost::Message as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::{Request, Response, Status};

/// What the fake daemon answers.
pub(crate) struct Answers {
    pub(crate) info: DaemonInfo,
    /// The inventory, or the error of the control protocol in its place.
    pub(crate) inventory: Result<DaemonInventory, ControlError>,
}

pub(crate) struct FakeDaemon {
    begun: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
    socket: std::path::PathBuf,
}

impl FakeDaemon {
    /// Serves `answers` on the control socket of `endpoint`.
    pub(crate) fn start(endpoint: &Bootstrap<Client>, answers: Answers) -> Self {
        let socket = endpoint.control_socket();
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the control socket");
        let begun = Arc::new(AtomicUsize::new(0));
        let service = wire::control_server::ControlServer::new(Service {
            answers,
            begun: Arc::clone(&begun),
        });
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(UnixListenerStream::new(listener))
                .await
                .expect("serve the control socket");
        });
        Self {
            begun,
            server,
            socket,
        }
    }

    /// The number of mutations that began.
    pub(crate) fn begun(&self) -> usize {
        self.begun.load(Ordering::SeqCst)
    }

    /// Ends the daemon, as one that removes its control socket at its end.
    pub(crate) async fn end(self) {
        self.server.abort();
        let _ = self.server.await;
        std::fs::remove_file(&self.socket).expect("remove the control socket");
    }
}

/// The info of a daemon of the control protocol `protocol`.
pub(crate) fn info(protocol: u32) -> DaemonInfo {
    DaemonInfo {
        version: "test".to_owned(),
        pid: 1,
        instance_id: "test-instance".to_owned(),
        executable: "/bin/omnifs".into(),
        attach_unix: None,
        attach_tcp: None,
        program: None,
        protocol,
    }
}

struct Service {
    answers: Answers,
    begun: Arc<AtomicUsize>,
}

fn control_status(error: &ControlError) -> Status {
    Status::with_details(
        tonic::Code::FailedPrecondition,
        error.message.clone(),
        grpc::to_error_detail(error).encode_to_vec().into(),
    )
}

/// Each named request as one that the daemon does not implement, in the
/// form that `tonic::async_trait` gives a method of the service.
macro_rules! unimplemented_requests {
    ($($method:ident($request:ty) -> $response:ty;)*) => {
        $(
            fn $method<'life, 'async_trait>(
                &'life self,
                _request: Request<$request>,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<Response<$response>, Status>>
                        + Send
                        + 'async_trait,
                >,
            >
            where
                'life: 'async_trait,
                Self: 'async_trait,
            {
                Box::pin(async move { Err(Status::unimplemented(stringify!($method))) })
            }
        )*
    };
}

#[tonic::async_trait]
impl wire::control_server::Control for Service {
    type SubscribeInspectorStream = ReceiverStream<Result<wire::InspectorStreamItem, Status>>;
    type StreamLogsStream = ReceiverStream<Result<wire::LogStreamItem, Status>>;
    type WatchPhaseStream = ReceiverStream<Result<wire::PhaseEvent, Status>>;

    async fn get_daemon_info(
        &self,
        _request: Request<wire::Empty>,
    ) -> Result<Response<wire::DaemonInfoResponse>, Status> {
        Ok(Response::new(wire::DaemonInfoResponse {
            info: Some(grpc::to_daemon_info(&self.answers.info)),
        }))
    }

    async fn get_inventory(
        &self,
        _request: Request<wire::Empty>,
    ) -> Result<Response<wire::InventoryResponse>, Status> {
        match &self.answers.inventory {
            Ok(inventory) => Ok(Response::new(wire::InventoryResponse {
                inventory: Some(grpc::to_daemon_inventory(inventory)),
            })),
            Err(error) => Err(control_status(error)),
        }
    }

    async fn begin_mutation(
        &self,
        _request: Request<wire::BeginMutationRequest>,
    ) -> Result<Response<wire::BeginMutationResponse>, Status> {
        self.begun.fetch_add(1, Ordering::SeqCst);
        Err(Status::unimplemented("begin_mutation"))
    }

    unimplemented_requests! {
        ready(wire::Empty) -> wire::Empty;
        get_status(wire::Empty) -> wire::StatusResponse;
        get_recovery_state(wire::Empty) -> wire::RecoveryResponse;
        repair_state(wire::RepairStateRequest) -> wire::RepairStateResponse;
        shutdown(wire::ShutdownRequest) -> wire::ShutdownResponse;
        list_providers(wire::Empty) -> wire::ListProvidersResponse;
        get_provider_metadata(wire::GetProviderMetadataRequest) -> wire::GetProviderMetadataResponse;
        import_provider(tonic::Streaming<wire::ImportProviderRequest>) -> wire::ImportProviderResponse;
        import_embedded_provider(wire::ImportEmbeddedProviderRequest) -> wire::ImportProviderResponse;
        apply_mutation(wire::ApplyMutationRequest) -> wire::ApplyMutationResponse;
        drop_mutation(wire::DropMutationRequest) -> wire::DropMutationResponse;
        list_mounts(wire::Empty) -> wire::ListMountsResponse;
        get_mount(wire::GetMountRequest) -> wire::GetMountResponse;
        list_credentials(wire::Empty) -> wire::ListCredentialsResponse;
        get_credential_status(wire::GetCredentialStatusRequest) -> wire::GetCredentialStatusResponse;
        subscribe_inspector(wire::InspectorRequest) -> Self::SubscribeInspectorStream;
        stream_logs(wire::StreamLogsRequest) -> Self::StreamLogsStream;
        watch_phase(wire::Empty) -> Self::WatchPhaseStream;
    }
}
