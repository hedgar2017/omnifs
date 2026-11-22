pub(crate) mod device;
pub(crate) mod implicit;
pub(crate) mod jwt_bearer;
pub(crate) mod loopback;
pub(crate) mod manual;

use crate::CredentialEntry;
use crate::OauthScheme;
use crate::error::AuthError;
use crate::request::ConfiguredClient;
use oauth2::{
    AuthorizationCode, CsrfToken, PkceCodeChallenge, PkceCodeVerifier, Scope, TokenResponse,
};
use secrecy::SecretString;
use time::OffsetDateTime;
use url::Url;

pub use device::DeviceCodePrompt;
pub use manual::ManualCode;

pub(crate) fn authorization_url(
    client: &ConfiguredClient,
    scheme: &OauthScheme,
    pkce_challenge: PkceCodeChallenge,
) -> (Url, CsrfToken) {
    let mut request = client
        .authorize_url(CsrfToken::new_random)
        .set_pkce_challenge(pkce_challenge)
        .add_scopes(scopes(scheme));
    for param in &scheme.extra_authorize_params {
        request = request.add_extra_param(&param.key, &param.value);
    }
    request.url()
}

pub(crate) fn scopes(scheme: &OauthScheme) -> impl Iterator<Item = Scope> + '_ {
    scheme.default_scopes.iter().cloned().map(Scope::new)
}

pub(crate) async fn exchange_code(
    client: &ConfiguredClient,
    http: &reqwest::Client,
    code: AuthorizationCode,
    pkce_verifier: PkceCodeVerifier,
    scheme: &OauthScheme,
) -> Result<CredentialEntry, AuthError> {
    let mut exchange = client.exchange_code(code).set_pkce_verifier(pkce_verifier);
    for param in &scheme.extra_token_params {
        exchange = exchange.add_extra_param(&param.key, &param.value);
    }
    let token = exchange.request_async(http).await?;
    credential_entry_from_token(&token)
}

/// The expiry of a token that the token endpoint answered at `now`: none for
/// an answer without `expires_in`, whose token has no known expiry, and an
/// error for one past the last time.
pub(crate) fn expiry(
    token: &oauth2::basic::BasicTokenResponse,
    now: OffsetDateTime,
) -> Result<Option<OffsetDateTime>, AuthError> {
    token
        .expires_in()
        .map(|expires_in| {
            time::Duration::try_from(expires_in)
                .ok()
                .and_then(|expires_in| now.checked_add(expires_in))
                .ok_or_else(|| {
                    AuthError::TokenAnswer(format!(
                        "expires_in of {} seconds is past the last time",
                        expires_in.as_secs()
                    ))
                })
        })
        .transpose()
}

pub(crate) fn credential_entry_from_token(
    token: &oauth2::basic::BasicTokenResponse,
) -> Result<CredentialEntry, AuthError> {
    // Stamp the real expiry; the credential service applies its freshness
    // margin at check time instead of baking it in here.
    let expires_at = expiry(token, OffsetDateTime::now_utc())?;
    let refresh_token = token
        .refresh_token()
        .map(|token| SecretString::from(token.secret().to_owned()));
    let scopes = token
        .scopes()
        .map(|s| s.iter().map(|sc| sc.as_ref().to_owned()).collect())
        .unwrap_or_default();
    let token_type = token.token_type().as_ref().to_owned();
    Ok(CredentialEntry::oauth(
        SecretString::from(token.access_token().secret().to_owned()),
        refresh_token,
        expires_at,
        token_type,
        scopes,
    ))
}

pub(crate) fn credential_entry_from_refresh_token(
    token: &oauth2::basic::BasicTokenResponse,
    current: &CredentialEntry,
) -> Result<CredentialEntry, AuthError> {
    let expires_at = expiry(token, OffsetDateTime::now_utc())?;
    let refresh_token = token
        .refresh_token()
        .map(|token| SecretString::from(token.secret().to_owned()));
    let scopes = token.scopes().map(|scopes| {
        scopes
            .iter()
            .map(|scope| scope.as_ref().to_owned())
            .filter(|scope| !scope.is_empty())
            .collect()
    });
    Ok(current.merge_oauth_refresh(
        SecretString::from(token.access_token().secret().to_owned()),
        refresh_token,
        expires_at,
        token.token_type().as_ref(),
        scopes,
    ))
}
