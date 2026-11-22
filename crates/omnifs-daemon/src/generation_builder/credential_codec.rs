//! Byte encoding for `omnifs_state::SecretMaterial`: the versioned prefix and
//! postcard payload that carry a credential's material and client overrides
//! between the daemon and durable storage, and after the payload the subject
//! of a credential with one.

use anyhow::Context as _;
use omnifs_api::{CredentialClientOverrides, CredentialMaterial, SecretBytes};
use omnifs_auth::{AuthKind, CredentialEntry};
use secrecy::ExposeSecret as _;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const CREDENTIAL_PAYLOAD_PREFIX: &[u8] = b"omnifs.credential.v1\0";

#[derive(Serialize, Deserialize)]
pub(super) struct CredentialPayload {
    pub(super) material: CredentialMaterial,
    pub(super) overrides: CredentialClientOverrides,
}

#[derive(Serialize)]
struct RefreshedCredentialPayload<'a> {
    material: CredentialMaterial,
    overrides: &'a CredentialClientOverrides,
}

pub(super) fn encode_payload(payload: &CredentialPayload) -> anyhow::Result<Vec<u8>> {
    encode_serialized_payload(payload, payload.overrides.subject.as_ref())
}

pub(super) fn encode_refreshed_payload(
    entry: &CredentialEntry,
    overrides: &CredentialClientOverrides,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        entry.kind() == AuthKind::OAuth,
        "only OAuth credentials can refresh"
    );
    // Zeroizing here only protects these transient copies; `SecretBytes`
    // itself does not zeroize on drop, and the final serialized payload
    // handed back is the persisted plaintext blob by design.
    let access_token = Zeroizing::new(entry.access_token().expose_secret().as_bytes().to_vec());
    let refresh_token = entry
        .refresh_token()
        .map(|token| Zeroizing::new(token.expose_secret().as_bytes().to_vec()));
    let material = CredentialMaterial::OAuth {
        access_token: SecretBytes::new(access_token.to_vec()),
        refresh_token: refresh_token.map(|token| SecretBytes::new(token.to_vec())),
        expires_at_unix: entry.expires_at().map(time::OffsetDateTime::unix_timestamp),
        token_type: entry.token_type().to_owned(),
        scopes: entry.scopes().to_vec(),
        upstream_identity: entry.upstream_identity().map(str::to_owned),
    };
    encode_serialized_payload(
        &RefreshedCredentialPayload {
            material,
            overrides,
        },
        overrides.subject.as_ref(),
    )
}

fn encode_serialized_payload(
    payload: &impl Serialize,
    subject: Option<&String>,
) -> anyhow::Result<Vec<u8>> {
    let encoded =
        Zeroizing::new(postcard::to_allocvec(payload).context("encode credential runtime")?);
    let mut bytes = Vec::with_capacity(CREDENTIAL_PAYLOAD_PREFIX.len() + encoded.len());
    bytes.extend_from_slice(CREDENTIAL_PAYLOAD_PREFIX);
    bytes.extend_from_slice(&encoded);
    if let Some(subject) = subject {
        bytes.extend(postcard::to_allocvec(subject).context("encode credential subject")?);
    }
    Ok(bytes)
}

pub(super) fn decode_payload(bytes: &[u8]) -> anyhow::Result<CredentialPayload> {
    let payload = bytes
        .strip_prefix(CREDENTIAL_PAYLOAD_PREFIX)
        .context("credential material has an unknown encoding")?;
    let (mut payload, subject) = postcard::take_from_bytes::<CredentialPayload>(payload)
        .context("decode credential runtime")?;
    if !subject.is_empty() {
        payload.overrides.subject =
            Some(postcard::from_bytes(subject).context("decode credential subject")?);
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(subject: Option<&str>) -> CredentialPayload {
        CredentialPayload {
            material: CredentialMaterial::OAuth {
                access_token: SecretBytes::new(b"access".to_vec()),
                refresh_token: None,
                expires_at_unix: Some(42),
                token_type: "Bearer".to_owned(),
                scopes: vec!["read".to_owned()],
                upstream_identity: None,
            },
            overrides: CredentialClientOverrides {
                client_id: Some("client".to_owned()),
                client_secret: Some(SecretBytes::new(b"key".to_vec())),
                redirect_uri: None,
                scopes: None,
                subject: subject.map(str::to_owned),
            },
        }
    }

    /// The subject of a credential is after its payload, which a credential
    /// without one has alone; its next token keeps it.
    #[test]
    fn the_stored_credential_has_its_subject_after_its_payload() {
        let without = encode_payload(&payload(None)).unwrap();
        let with = encode_payload(&payload(Some("ada@example.test"))).unwrap();
        assert!(with.starts_with(&without));
        assert_eq!(decode_payload(&without).unwrap().overrides.subject, None);
        let decoded = decode_payload(&with).unwrap();
        assert_eq!(
            decoded.overrides.subject.as_deref(),
            Some("ada@example.test")
        );
        assert_eq!(decoded.overrides.client_id.as_deref(), Some("client"));

        let entry = CredentialEntry::oauth(
            secrecy::SecretString::from("next".to_owned()),
            None,
            None,
            "Bearer",
            vec!["read".to_owned()],
        );
        let refreshed = encode_refreshed_payload(&entry, &decoded.overrides).unwrap();
        assert_eq!(
            decode_payload(&refreshed).unwrap().overrides.subject,
            decoded.overrides.subject
        );
    }
}
