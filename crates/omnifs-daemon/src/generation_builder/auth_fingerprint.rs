//! Derives `omnifs_core::AuthRuntimeFingerprint` from the provider's pinned
//! auth scheme and a credential's client overrides, so a generation build can
//! detect when a stored credential no longer matches the provider it was
//! issued against.

use anyhow::Context as _;
use omnifs_api::CredentialClientOverrides;
use omnifs_auth::AuthScheme;
use omnifs_core::{AuthRuntimeFingerprint, ProviderId};
use serde::Serialize;

const AUTH_FINGERPRINT_DOMAIN: &str = "omnifs auth runtime fingerprint v1";

#[derive(Serialize)]
struct FingerprintInput<'a> {
    provider: &'a [u8; 32],
    scheme: &'a AuthScheme,
    overrides: FingerprintOverrides<'a>,
}

/// The client overrides of a credential for its fingerprint, which statuses
/// show: all but the client secret, in whose place every credential has
/// none, as a credential without one.
#[derive(Serialize)]
struct FingerprintOverrides<'a> {
    client_id: &'a Option<String>,
    client_secret: Option<()>,
    redirect_uri: &'a Option<String>,
    scopes: &'a Option<Vec<String>>,
}

pub(super) fn auth_fingerprint(
    provider: ProviderId,
    scheme: &AuthScheme,
    overrides: &CredentialClientOverrides,
) -> anyhow::Result<AuthRuntimeFingerprint> {
    let input = postcard::to_allocvec(&FingerprintInput {
        provider: provider.as_bytes(),
        scheme,
        overrides: FingerprintOverrides {
            client_id: &overrides.client_id,
            client_secret: None,
            redirect_uri: &overrides.redirect_uri,
            scopes: &overrides.scopes,
        },
    })
    .context("encode auth runtime fingerprint")?;
    let mut hasher = blake3::Hasher::new_derive_key(AUTH_FINGERPRINT_DOMAIN);
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

        // A credential without a client secret has the fingerprint that the
        // overrides themselves gave it.
        let input =
            postcard::to_allocvec(&(provider.as_bytes(), &AuthScheme::None, &overrides(None)))
                .unwrap();
        let mut hasher = blake3::Hasher::new_derive_key(AUTH_FINGERPRINT_DOMAIN);
        hasher.update(&input);
        assert_eq!(
            without,
            AuthRuntimeFingerprint::from_digest(*hasher.finalize().as_bytes())
        );
    }
}
