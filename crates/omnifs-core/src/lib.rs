//! Core omnifs protocol types.

mod auth_fingerprint;
mod client_owner_id;
mod content_type;
mod file;
pub mod fs;
mod mount_name;
mod mutation;
pub mod path;
mod program;
mod provider;
mod provider_id;
mod state_version;

/// How long a peer that is alive and silent may make no progress before
/// its wait fails: a daemon that holds a control request and never answers,
/// a client that holds a provider upload open and sends no bytes, a
/// revocation endpoint that holds its connection, and an NFS client that
/// reads none of a reply that the server writes. It is measured from the
/// last progress of the wait, never as a total time, and it bounds these
/// waits alone (`docs/contracts/10-system.md`).
pub const SILENT_PEER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub use auth_fingerprint::{AuthRuntimeFingerprint, AuthRuntimeFingerprintParseError};
pub use client_owner_id::{ClientOwnerId, ClientOwnerIdError};
pub use content_type::ContentType;
pub use file::{FileSize, ReadMode, Stability};
pub use mount_name::{MountName, MountNameError};
pub use mutation::{MutationId, MutationIdError};
pub use path::{ParseError, Path, Segment};
pub use program::executable_program;
pub use provider::{
    IdError, ProviderMeta, ProviderName, ProviderRef, ProviderVersion, validate_account,
    validate_key_part,
};
pub use provider_id::{ProviderId, ProviderIdHexError};
pub use state_version::{
    CredentialGeneration, CredentialVersion, MountRevision, MountVersion, MountVersionParseError,
};
