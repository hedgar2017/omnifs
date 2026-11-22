//! The JWT bearer grant of RFC 7523: an access token for an assertion that
//! the client signs with its own key, without a sign-in and without a
//! refresh token. The client asserts its subject: the subject of its
//! credential, or itself.

use crate::CredentialEntry;
use crate::OAuthFlow;
use crate::client::OAuthClient;
use crate::error::AuthError;
use crate::flows::{credential_entry_from_refresh_token, credential_entry_from_token};
use crate::request::{JwtBearerLoginRequest, OAuthRequest};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use oauth2::TokenResponse as _;
use oauth2::basic::{BasicErrorResponse, BasicTokenResponse};
use secrecy::ExposeSecret;
use serde_json::{Map, Value};
use time::{Duration, OffsetDateTime};

const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// The time an assertion is valid for after its signature, in which the
/// token endpoint answers it.
const ASSERTION_LIFETIME: Duration = Duration::HOUR;

impl OAuthClient {
    /// The first access token of a client by its assertion, of the scopes of
    /// the answer, and of the request for an answer without them.
    pub async fn login_jwt_bearer(
        &self,
        request: JwtBearerLoginRequest,
    ) -> Result<CredentialEntry, AuthError> {
        let token = self.assert(&request.oauth).await?;
        let granted = credential_entry_from_token(&token)?;
        Ok(match token.scopes() {
            Some(_) => granted,
            None => CredentialEntry::oauth(
                granted.access_token().clone(),
                None,
                granted.expires_at(),
                granted.token_type(),
                request.oauth.scheme.default_scopes.clone(),
            ),
        })
    }

    /// The next access token of `current` by a new assertion of the client.
    pub(crate) async fn assert_again(
        &self,
        request: &OAuthRequest,
        current: &CredentialEntry,
    ) -> Result<CredentialEntry, AuthError> {
        let token = self.assert(request).await?;
        credential_entry_from_refresh_token(&token, current)
    }

    async fn assert(&self, request: &OAuthRequest) -> Result<BasicTokenResponse, AuthError> {
        let assertion = assertion(request, OffsetDateTime::now_utc())?;
        let scope = request.scheme.default_scopes.join(" ");
        let mut form = vec![
            ("grant_type", GRANT_TYPE),
            ("assertion", assertion.as_str()),
        ];
        if matches!(request.scheme.flow, OAuthFlow::JwtBearer) && !scope.is_empty() {
            form.push(("scope", &scope));
        }
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
            return Err(match serde_json::from_slice::<BasicErrorResponse>(&body) {
                Ok(failure) => AuthError::TokenEndpoint {
                    error: failure.error().to_string(),
                    description: failure.error_description().cloned(),
                },
                Err(error) => AuthError::TokenAnswer(format!("HTTP {status}: {error}")),
            });
        }
        serde_json::from_slice(&body).map_err(|error| AuthError::TokenAnswer(error.to_string()))
    }
}

/// The assertion of the client at `now`, signed with RS256: its ID as the
/// issuer; the subject of its credential, or itself, as the subject; the
/// token endpoint as the audience; and its scopes in the claim of a flow
/// that declares one.
fn assertion(request: &OAuthRequest, now: OffsetDateTime) -> Result<String, AuthError> {
    let issuer = request.effective_client_id()?;
    let key = request
        .client_secret()
        .ok_or(AuthError::MissingClientSecret)?;
    let key = EncodingKey::from_rsa_pem(key.expose_secret().as_bytes())
        .map_err(|error| AuthError::AssertionKey(error.to_string()))?;
    let mut claims = Map::new();
    if let OAuthFlow::JwtBearerScopeClaim(flow) = &request.scheme.flow {
        claims.insert(
            flow.claim.clone(),
            Value::from(request.scheme.default_scopes.join(" ")),
        );
    }
    claims.insert(
        "sub".to_owned(),
        Value::from(request.subject().unwrap_or(&issuer)),
    );
    claims.insert("iss".to_owned(), Value::from(issuer));
    claims.insert(
        "aud".to_owned(),
        Value::from(request.scheme.token_endpoint.as_str()),
    );
    claims.insert("iat".to_owned(), Value::from(now.unix_timestamp()));
    claims.insert(
        "exp".to_owned(),
        Value::from((now + ASSERTION_LIFETIME).unix_timestamp()),
    );
    jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key)
        .map_err(|error| AuthError::AssertionKey(error.to_string()))
}
