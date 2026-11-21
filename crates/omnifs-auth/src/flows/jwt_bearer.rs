//! The JWT bearer grant of RFC 7523: an access token for an assertion that
//! the client signs with its own key, without a user and without a refresh
//! token.

use crate::CredentialEntry;
use crate::client::OAuthClient;
use crate::error::AuthError;
use crate::request::{JwtBearerLoginRequest, OAuthRequest};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// The time an assertion is valid for, the longest that RFC 7523 servers
/// such as Google take.
const ASSERTION_LIFETIME: Duration = Duration::HOUR;

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: String,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

/// The answer of the token endpoint to an assertion.
#[derive(Deserialize)]
struct Answer {
    access_token: String,
    token_type: String,
    expires_in: i64,
}

/// The error answer of the token endpoint of RFC 6749.
#[derive(Deserialize)]
struct Failure {
    error: String,
    error_description: Option<String>,
}

/// The access token of an answer of the token endpoint, with its type and
/// its expiry.
struct Granted {
    access_token: SecretString,
    token_type: String,
    expires_at: OffsetDateTime,
}

impl OAuthClient {
    /// The first access token of a client by its assertion.
    pub async fn login_jwt_bearer(
        &self,
        request: JwtBearerLoginRequest,
    ) -> Result<CredentialEntry, AuthError> {
        let granted = self.assert(&request.oauth).await?;
        Ok(CredentialEntry::oauth(
            granted.access_token,
            None,
            Some(granted.expires_at),
            granted.token_type,
            request.oauth.scheme.default_scopes.clone(),
        ))
    }

    /// The next access token of `current` by a new assertion of the client.
    pub(crate) async fn assert_again(
        &self,
        request: &OAuthRequest,
        current: &CredentialEntry,
    ) -> Result<CredentialEntry, AuthError> {
        let granted = self.assert(request).await?;
        Ok(current.merge_oauth_refresh(
            granted.access_token,
            None,
            Some(granted.expires_at),
            granted.token_type,
            None,
        ))
    }

    async fn assert(&self, request: &OAuthRequest) -> Result<Granted, AuthError> {
        let now = OffsetDateTime::now_utc();
        let assertion = assertion(request, now)?;
        let mut form = vec![
            ("grant_type", GRANT_TYPE),
            ("assertion", assertion.as_str()),
        ];
        for param in &request.scheme.extra_token_params {
            form.push((param.key.as_str(), param.value.as_str()));
        }
        let response = self
            .http
            .post(&request.scheme.token_endpoint)
            .form(&form)
            .send()
            .await?;
        let status = response.status();
        let body = response.bytes().await?;
        if !status.is_success() {
            return Err(match serde_json::from_slice::<Failure>(&body) {
                Ok(failure) => AuthError::TokenEndpoint {
                    error: failure.error,
                    description: failure.error_description,
                },
                Err(error) => AuthError::TokenAnswer(format!("HTTP {status}: {error}")),
            });
        }
        let token: Answer = serde_json::from_slice(&body)
            .map_err(|error| AuthError::TokenAnswer(error.to_string()))?;
        Ok(Granted {
            access_token: SecretString::from(token.access_token),
            token_type: token.token_type,
            expires_at: now + Duration::seconds(token.expires_in),
        })
    }
}

/// The assertion of the client at `now`: its ID as the issuer, the scopes of
/// the request, and the token endpoint as the audience, signed with RS256.
fn assertion(request: &OAuthRequest, now: OffsetDateTime) -> Result<String, AuthError> {
    let issuer = request.effective_client_id()?;
    let key = EncodingKey::from_rsa_pem(request.assertion_key()?.expose_secret().as_bytes())
        .map_err(|error| AuthError::AssertionKey(error.to_string()))?;
    let claims = Claims {
        iss: &issuer,
        scope: request.scheme.default_scopes.join(" "),
        aud: &request.scheme.token_endpoint,
        iat: now.unix_timestamp(),
        exp: (now + ASSERTION_LIFETIME).unix_timestamp(),
    };
    jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key)
        .map_err(|error| AuthError::AssertionKey(error.to_string()))
}
