//! Byte encoding for `omnifs_state::SecretMaterial`: a prefix that names the
//! version of the encoding, and the postcard payload of that version with
//! the material and the client overrides of a credential. The daemon writes
//! the second version and reads both.

use anyhow::Context as _;
use omnifs_api::{CredentialClientOverrides, CredentialMaterial, SecretBytes};
use omnifs_auth::{AuthKind, CredentialEntry};
use secrecy::ExposeSecret as _;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The prefix of the first version, which an earlier omnifs wrote: the
/// material, the client overrides without the subject, and after them the
/// subject of a credential with one alone.
const CREDENTIAL_PAYLOAD_V1: &[u8] = b"omnifs.credential.v1\0";

/// The prefix of the second version: the material and the client overrides
/// with the subject.
const CREDENTIAL_PAYLOAD_V2: &[u8] = b"omnifs.credential.v2\0";

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

/// The payload of the first version, whose overrides have no subject.
#[derive(Deserialize)]
struct CredentialPayloadV1 {
    material: CredentialMaterial,
    overrides: CredentialClientOverridesV1,
}

#[derive(Deserialize)]
struct CredentialClientOverridesV1 {
    client_id: Option<String>,
    client_secret: Option<SecretBytes>,
    redirect_uri: Option<String>,
    scopes: Option<Vec<String>>,
}

pub(super) fn encode_payload(payload: &CredentialPayload) -> anyhow::Result<Vec<u8>> {
    encode_serialized_payload(payload)
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
    encode_serialized_payload(&RefreshedCredentialPayload {
        material,
        overrides,
    })
}

fn encode_serialized_payload(payload: &impl Serialize) -> anyhow::Result<Vec<u8>> {
    let encoded =
        Zeroizing::new(postcard::to_allocvec(payload).context("encode credential runtime")?);
    let mut bytes = Vec::with_capacity(CREDENTIAL_PAYLOAD_V2.len() + encoded.len());
    bytes.extend_from_slice(CREDENTIAL_PAYLOAD_V2);
    bytes.extend_from_slice(&encoded);
    Ok(bytes)
}

pub(super) fn decode_payload(bytes: &[u8]) -> anyhow::Result<CredentialPayload> {
    if let Some(payload) = bytes.strip_prefix(CREDENTIAL_PAYLOAD_V2) {
        return postcard::from_bytes(payload).context("decode credential runtime");
    }
    let payload = bytes
        .strip_prefix(CREDENTIAL_PAYLOAD_V1)
        .context("credential material has an unknown encoding")?;
    let (payload, subject) = postcard::take_from_bytes::<CredentialPayloadV1>(payload)
        .context("decode credential runtime of the first version")?;
    let subject = if subject.is_empty() {
        None
    } else {
        Some(postcard::from_bytes(subject).context("decode credential subject")?)
    };
    Ok(CredentialPayload {
        material: payload.material,
        overrides: CredentialClientOverrides {
            client_id: payload.overrides.client_id,
            client_secret: payload.overrides.client_secret,
            redirect_uri: payload.overrides.redirect_uri,
            scopes: payload.overrides.scopes,
            subject,
        },
    })
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

    /// The facts of a payload that the tests compare, with its secrets.
    fn facts(payload: &CredentialPayload) -> String {
        let material = match &payload.material {
            CredentialMaterial::StaticToken { token } => {
                format!("static {:?}", token.expose())
            },
            CredentialMaterial::OAuth {
                access_token,
                refresh_token,
                expires_at_unix,
                token_type,
                scopes,
                upstream_identity,
            } => format!(
                "oauth {:?} {:?} {expires_at_unix:?} {token_type} {scopes:?} {upstream_identity:?}",
                access_token.expose(),
                refresh_token.as_ref().map(SecretBytes::expose)
            ),
        };
        format!(
            "{material} | {:?} {:?} {:?} {:?} {:?}",
            payload.overrides.client_id,
            payload
                .overrides
                .client_secret
                .as_ref()
                .map(SecretBytes::expose),
            payload.overrides.redirect_uri,
            payload.overrides.scopes,
            payload.overrides.subject
        )
    }

    /// A stored credential has the prefix of the second version, and each
    /// fact of its payload with its subject, which its next token keeps.
    #[test]
    fn a_stored_credential_is_of_the_second_version_with_its_subject() {
        for subject in [None, Some("ada@example.test")] {
            let encoded = encode_payload(&payload(subject)).unwrap();
            assert!(encoded.starts_with(b"omnifs.credential.v2\0"));
            let decoded = decode_payload(&encoded).unwrap();
            assert_eq!(facts(&decoded), facts(&payload(subject)));

            let entry = CredentialEntry::oauth(
                secrecy::SecretString::from("next".to_owned()),
                None,
                None,
                "Bearer",
                vec!["read".to_owned()],
            );
            let refreshed = encode_refreshed_payload(&entry, &decoded.overrides).unwrap();
            assert!(refreshed.starts_with(b"omnifs.credential.v2\0"));
            let refreshed = decode_payload(&refreshed).unwrap();
            assert_eq!(refreshed.overrides.subject.as_deref(), subject);
            assert_eq!(refreshed.overrides.client_id.as_deref(), Some("client"));
        }
    }

    /// A credential that an earlier omnifs stored, by its bytes, is read
    /// with each fact of it: a static token, an OAuth credential without a
    /// subject, and one with the subject after its payload.
    #[test]
    fn a_credential_of_the_first_version_is_read_by_its_bytes() {
        let stored = |hex: &str| decode_payload(&hex::decode(hex).unwrap()).unwrap();

        let static_token =
            stored("6f6d6e6966732e63726564656e7469616c2e7631000005746f6b656e00000000");
        assert_eq!(
            facts(&static_token),
            facts(&CredentialPayload {
                material: CredentialMaterial::StaticToken {
                    token: SecretBytes::new(b"token".to_vec()),
                },
                overrides: CredentialClientOverrides {
                    client_id: None,
                    client_secret: None,
                    redirect_uri: None,
                    scopes: None,
                    subject: None,
                },
            })
        );

        let without = stored(
            "6f6d6e6966732e63726564656e7469616c2e763100010661636365737300015406426561726572010472656164000106636c69656e7401036b65790000",
        );
        assert_eq!(facts(&without), facts(&payload(None)));

        let with = stored(
            "6f6d6e6966732e63726564656e7469616c2e763100010661636365737300015406426561726572010472656164000106636c69656e7401036b6579000010616461406578616d706c652e74657374",
        );
        assert_eq!(facts(&with), facts(&payload(Some("ada@example.test"))));
    }

    #[test]
    fn a_credential_of_an_unknown_version_is_an_error() {
        let error = decode_payload(b"omnifs.credential.v3\0").err().unwrap();
        assert_eq!(
            error.to_string(),
            "credential material has an unknown encoding"
        );
    }
}
