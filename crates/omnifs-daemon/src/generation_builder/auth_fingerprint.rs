//! Derives `omnifs_core::AuthRuntimeFingerprint` from the provider's pinned
//! auth scheme and the facts of the client of a credential that are not
//! secrets, so a generation build can detect when a stored credential no
//! longer matches the provider it was issued against.

use anyhow::Context as _;
use omnifs_api::CredentialClientOverrides;
use omnifs_auth::AuthScheme;
use omnifs_core::{AuthRuntimeFingerprint, ProviderId};
use serde::Serialize;

/// The domain of the fingerprint of each credential that this omnifs
/// stores.
const AUTH_FINGERPRINT_DOMAIN: &str = "omnifs auth runtime fingerprint v2";

/// The domain of the fingerprint of a credential that an earlier omnifs
/// stored.
const EARLIER_AUTH_FINGERPRINT_DOMAIN: &str = "omnifs auth runtime fingerprint v1";

#[derive(Serialize)]
struct FingerprintInput<'a, C> {
    provider: &'a [u8; 32],
    scheme: &'a AuthScheme,
    client: C,
}

/// The client of a credential for its fingerprint, which statuses show:
/// each of its overrides but the client secret.
#[derive(Serialize)]
struct FingerprintClient<'a> {
    client_id: &'a Option<String>,
    redirect_uri: &'a Option<String>,
    scopes: &'a Option<Vec<String>>,
    subject: &'a Option<String>,
}

/// The client of a credential in the fingerprint of an earlier omnifs: a
/// none in the place of the client secret, and the subject of a credential
/// with one alone.
#[derive(Serialize)]
struct EarlierFingerprintClient<'a> {
    client_id: &'a Option<String>,
    no_client_secret: Option<()>,
    redirect_uri: &'a Option<String>,
    scopes: &'a Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: &'a Option<String>,
}

/// The fingerprint of a credential of `provider` with `overrides`, issued
/// against `scheme`.
pub(super) fn auth_fingerprint(
    provider: ProviderId,
    scheme: &AuthScheme,
    overrides: &CredentialClientOverrides,
) -> anyhow::Result<AuthRuntimeFingerprint> {
    digest(
        AUTH_FINGERPRINT_DOMAIN,
        &FingerprintInput {
            provider: provider.as_bytes(),
            scheme,
            client: FingerprintClient {
                client_id: &overrides.client_id,
                redirect_uri: &overrides.redirect_uri,
                scopes: &overrides.scopes,
                subject: &overrides.subject,
            },
        },
    )
}

/// The fingerprint that an earlier omnifs gave the credential of `provider`
/// with `overrides`, issued against `scheme`. A stored credential with it is
/// issued against `scheme` as one with [`auth_fingerprint`], and keeps it
/// until its next submission.
pub(super) fn earlier_auth_fingerprint(
    provider: ProviderId,
    scheme: &AuthScheme,
    overrides: &CredentialClientOverrides,
) -> anyhow::Result<AuthRuntimeFingerprint> {
    digest(
        EARLIER_AUTH_FINGERPRINT_DOMAIN,
        &FingerprintInput {
            provider: provider.as_bytes(),
            scheme,
            client: EarlierFingerprintClient {
                client_id: &overrides.client_id,
                no_client_secret: None,
                redirect_uri: &overrides.redirect_uri,
                scopes: &overrides.scopes,
                subject: &overrides.subject,
            },
        },
    )
}

fn digest(domain: &str, input: &impl Serialize) -> anyhow::Result<AuthRuntimeFingerprint> {
    let input = postcard::to_allocvec(input).context("encode auth runtime fingerprint")?;
    let mut hasher = blake3::Hasher::new_derive_key(domain);
    hasher.update(&input);
    Ok(AuthRuntimeFingerprint::from_digest(
        *hasher.finalize().as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::SecretBytes;

    fn overrides(client_secret: Option<&[u8]>) -> CredentialClientOverrides {
        CredentialClientOverrides {
            client_id: Some("client".to_owned()),
            client_secret: client_secret.map(|secret| SecretBytes::new(secret.to_vec())),
            redirect_uri: None,
            scopes: None,
            subject: None,
        }
    }

    fn with_subject(subject: &str) -> CredentialClientOverrides {
        CredentialClientOverrides {
            subject: Some(subject.to_owned()),
            ..overrides(None)
        }
    }

    /// The subject of a credential is of its fingerprint.
    #[test]
    fn the_fingerprint_has_the_subject_of_a_credential() {
        let provider = ProviderId::from_digest([7; 32]);
        let of = |overrides: &CredentialClientOverrides| {
            auth_fingerprint(provider, &AuthScheme::None, overrides).unwrap()
        };
        assert_ne!(of(&with_subject("ada@example.test")), of(&overrides(None)));
        assert_ne!(
            of(&with_subject("ada@example.test")),
            of(&with_subject("bob@example.test"))
        );
    }

    #[test]
    fn the_fingerprint_has_nothing_of_the_client_secret() {
        let provider = ProviderId::from_digest([7; 32]);
        let of = |overrides: &CredentialClientOverrides| {
            auth_fingerprint(provider, &AuthScheme::None, overrides).unwrap()
        };
        let without = of(&overrides(None));
        assert_eq!(of(&overrides(Some(b"one"))), without);
        assert_eq!(of(&overrides(Some(b"another"))), without);

        // The fingerprint is of the provider, the scheme, and the four facts
        // of the client alone, without a place for the client secret.
        let none: Option<String> = None;
        let scopes: Option<Vec<String>> = None;
        let input = postcard::to_allocvec(&(
            provider.as_bytes(),
            &AuthScheme::None,
            (Some("client"), &none, &scopes, &none),
        ))
        .unwrap();
        let mut hasher = blake3::Hasher::new_derive_key("omnifs auth runtime fingerprint v2");
        hasher.update(&input);
        assert_eq!(
            without,
            AuthRuntimeFingerprint::from_digest(*hasher.finalize().as_bytes())
        );
    }

    /// The fingerprint of an earlier omnifs is the one that it stored, by
    /// the digests of two credentials of it, and is not the one of this
    /// omnifs.
    #[test]
    fn the_earlier_fingerprint_is_the_one_that_an_earlier_omnifs_stored() {
        let provider = ProviderId::from_digest([7; 32]);
        let earlier = |overrides: &CredentialClientOverrides| {
            hex::encode(
                earlier_auth_fingerprint(provider, &AuthScheme::None, overrides)
                    .unwrap()
                    .as_bytes(),
            )
        };
        assert_eq!(
            earlier(&overrides(None)),
            "8a27c87c3ebc8475e94b91ee5be769bb88357439edb44401eb30723dbf8137a0"
        );
        assert_eq!(
            earlier(&overrides(Some(b"key"))),
            "8a27c87c3ebc8475e94b91ee5be769bb88357439edb44401eb30723dbf8137a0"
        );
        assert_eq!(
            earlier(&with_subject("ada@example.test")),
            "67b61a001283834f25452955e214e5b58f87c4836882f3a82315338fa28b1378"
        );
        assert_ne!(
            earlier_auth_fingerprint(provider, &AuthScheme::None, &overrides(None)).unwrap(),
            auth_fingerprint(provider, &AuthScheme::None, &overrides(None)).unwrap()
        );
    }
}
