//! The JWT bearer grant of RFC 7523: an access token for an assertion that
//! the client signs with its own key, without a sign-in and without a
//! refresh token. The assertion names the account that the client acts as
//! as its subject, and none for the default account.

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

/// The time an assertion is valid for after the time of its issue: RFC 7523
/// section 3 requires an `exp`, and the authorization server of Google takes
/// no later one.
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
                Err(_) => AuthError::TokenEndpointStatus(status.as_u16()),
            });
        }
        serde_json::from_slice(&body).map_err(|error| AuthError::TokenAnswer(error.to_string()))
    }
}

/// The assertion of the client at `now`, signed with RS256: its ID as the
/// issuer; the account that it acts as as the subject, and no subject for
/// the default account; the token endpoint as the audience; `now` as the
/// time of its issue, and its expiry [`ASSERTION_LIFETIME`] after that; and
/// its scopes in the claim of a flow that declares one.
fn assertion(request: &OAuthRequest, now: OffsetDateTime) -> Result<String, AuthError> {
    let issuer = request.effective_client_id()?;
    let key = request
        .client_secret()
        .ok_or(AuthError::MissingAssertionKey)?;
    let key = EncodingKey::from_rsa_pem(key.expose_secret().as_bytes())
        .map_err(|error| AuthError::AssertionKey(error.to_string()))?;
    let mut claims = Map::new();
    if let OAuthFlow::JwtBearerScopeClaim(flow) = &request.scheme.flow {
        claims.insert(
            flow.claim.clone(),
            Value::from(request.scheme.default_scopes.join(" ")),
        );
    }
    if let Some(subject) = request.subject() {
        claims.insert("sub".to_owned(), Value::from(subject));
    }
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
        .map_err(|error| AuthError::AssertionSignature(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::OAuthRuntimeOverrides;
    use crate::{OauthScheme, TokenEndpointAuthMethod};
    use secrecy::SecretString;

    /// The request of a client with the key of the tests for the JWT bearer
    /// grant of `token_endpoint`.
    fn request(token_endpoint: &str) -> OAuthRequest {
        OAuthRequest::from_runtime(
            OauthScheme {
                key: "service-account".to_owned(),
                display_name: "Service account".to_owned(),
                authorization_endpoint: String::new(),
                token_endpoint: token_endpoint.to_owned(),
                revocation_endpoint: None,
                default_client_id: Some("client-id".to_owned()),
                default_scopes: Vec::new(),
                flow: OAuthFlow::JwtBearer,
                token_endpoint_auth: TokenEndpointAuthMethod::None,
                refresh_token_rotates: false,
                extra_authorize_params: Vec::new(),
                extra_token_params: Vec::new(),
                inject_domains: Vec::new(),
                inject_header_name: None,
                inject_value_prefix: "Bearer ".to_owned(),
            },
            OAuthRuntimeOverrides {
                client_secret: Some(SecretString::from(
                    crate::test_support::ASSERTION_KEY.to_owned(),
                )),
                ..OAuthRuntimeOverrides::default()
            },
        )
        .unwrap()
    }

    /// An assertion names the time of its signature as the time of its
    /// issue, expires its lifetime after it, and names no subject for the
    /// default account.
    #[test]
    fn an_assertion_is_issued_at_its_signature_without_a_subject_for_the_default_account() {
        #[derive(serde::Deserialize)]
        struct Times {
            iat: i64,
            exp: i64,
            sub: Option<String>,
        }
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let assertion = assertion(&request("https://auth.example.test/token"), now).unwrap();

        let mut validation = jsonwebtoken::Validation::new(Algorithm::RS256);
        validation.insecure_disable_signature_validation();
        validation.validate_exp = false;
        validation.validate_aud = false;
        let times = jsonwebtoken::decode::<Times>(
            &assertion,
            &jsonwebtoken::DecodingKey::from_secret(&[]),
            &validation,
        )
        .unwrap()
        .claims;
        assert_eq!(times.iat, 1_800_000_000);
        assert_eq!(times.exp, times.iat + 3600);
        assert_eq!(times.sub, None);
    }

    /// The error of a token endpoint that fails without an OAuth error, as
    /// of a gateway before it, names the status of its answer.
    #[tokio::test]
    async fn a_failure_of_the_token_endpoint_without_an_oauth_error_names_its_status() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 6\r\nconnection: close\r\n\r\n<html>",
                )
                .await
                .unwrap();
        });
        let login = JwtBearerLoginRequest {
            oauth: request(&format!("http://{address}/token")),
        };
        let error = OAuthClient::new()
            .unwrap()
            .login_jwt_bearer(login)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "oauth token endpoint answered HTTP 502 without an OAuth error"
        );
    }
}
