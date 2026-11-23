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
        }
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

        // The fingerprint is of the provider, the scheme, and the three facts
        // of the client alone, without a place for the client secret.
        let none: Option<String> = None;
        let scopes: Option<Vec<String>> = None;
        let input = postcard::to_allocvec(&(
            provider.as_bytes(),
            &AuthScheme::None,
            (Some("client"), &none, &scopes),
        ))
        .unwrap();
        let mut hasher = blake3::Hasher::new_derive_key("omnifs auth runtime fingerprint v2");
        hasher.update(&input);
        assert_eq!(
            without,
            AuthRuntimeFingerprint::from_digest(*hasher.finalize().as_bytes())
        );
    }
}
