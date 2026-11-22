use anyhow::Context;
use omnifs_auth::TokenValidation;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Default, Clone)]
pub(super) struct ValidationOutcome {
    pub(super) identity: Option<String>,
    pub(super) workspace: Option<String>,
    pub(super) extras: BTreeMap<String, String>,
}

/// Probes `token` as the validation of its scheme declares: one request of
/// its method to its URL with the token in its header and each declared
/// header beside it, which follows no redirect, so that the token reaches the
/// declared URL alone. The token is valid with the declared status and with
/// a value at the declared JSON pointer that [`holds`].
pub(super) async fn validate_static_token(
    validation: &TokenValidation,
    header_name: &str,
    header_prefix: &str,
    token: &str,
    output: &crate::ui::output::Output,
) -> anyhow::Result<ValidationOutcome> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("omnifs-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build HTTP client")?;
    let method = reqwest::Method::from_bytes(validation.method.as_bytes())
        .with_context(|| format!("invalid HTTP method `{}`", validation.method))?;
    let header_value = format!("{header_prefix}{token}");

    let mut req = client
        .request(method, &validation.url)
        .header(header_name, header_value);
    for (name, value) in &validation.headers {
        req = req.header(name, value);
    }
    if let Some(body) = validation.body.as_deref() {
        req = req
            .header("Content-Type", "application/json")
            .body(body.to_string());
    }

    output.narrate(format!("validating against {}", validation.url));
    let response = req.send().await.context("validation request failed")?;
    let status = response.status();
    if status.is_redirection() {
        anyhow::bail!(
            "validation failed: {} answered the redirect {}, and the probe of a token follows none",
            validation.url,
            status.as_u16()
        );
    }
    if status.as_u16() != validation.expect_status {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "validation failed: expected status {}, got {} ({}). Response: {}",
            validation.expect_status,
            status.as_u16(),
            status.canonical_reason().unwrap_or("unknown"),
            crate::ui::truncate(&body, 300)
        );
    }
    let body = response.text().await.context("read response body")?;
    if validation.json_pointer.is_none() && validation.extract.is_empty() {
        return Ok(ValidationOutcome::default());
    }
    let parsed: Value = serde_json::from_str(&body).map_err(|error| {
        anyhow::anyhow!(
            "validation failed: the response is not JSON ({error}). Response: {}",
            crate::ui::truncate(&body, 300)
        )
    })?;
    if let Some(pointer) = validation.json_pointer.as_deref()
        && !parsed.pointer(pointer).is_some_and(holds)
    {
        anyhow::bail!(
            "validation failed: `{}` of the response is absent, null, false, or an empty string. Response: {}",
            pointer,
            crate::ui::truncate(&body, 300)
        );
    }
    Ok(validation.extract.iter().fold(
        ValidationOutcome::default(),
        |mut outcome, (key, pointer)| {
            if let Some(val) = parsed.pointer(pointer).and_then(json_to_string) {
                match key.as_str() {
                    "identity" => outcome.identity = Some(val),
                    "workspace" => outcome.workspace = Some(val),
                    _ => {
                        outcome.extras.insert(key.clone(), val);
                    },
                }
            }
            outcome
        },
    ))
}

/// Whether the value at the JSON pointer of a validation holds: each value
/// but `null`, `false`, and the empty string, with which an upstream answers
/// for a token that it does not take.
fn holds(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Bool(true) | Value::Number(_) | Value::Array(_) | Value::Object(_) => true,
    }
}

fn json_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::auth::Auth;
    use crate::ui::output::{Output, OutputMode};
    use omnifs_core::ProviderId;
    use omnifs_provider::ProviderManifest;
    use secrecy::SecretString;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A local server that answers its one request with `answer` and gives
    /// the head of that request in lowercase.
    async fn serve(answer: String) -> (std::net::SocketAddr, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0; 1];
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            stream.write_all(answer.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap().to_ascii_lowercase()
        });
        (address, request)
    }

    fn json_answer(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// The manifest of a provider whose static token has `validation`, the
    /// one of Slack for none: `auth.test` with the ID of the bot.
    fn manifest(
        address: std::net::SocketAddr,
        validation: Option<serde_json::Value>,
    ) -> ProviderManifest {
        let validation = validation.unwrap_or_else(|| {
            serde_json::json!({
                "method": "POST",
                "url": format!("http://{address}/api/auth.test"),
                "headers": { "Demo-Version": "2026-03-11" },
                "expectStatus": 200,
                "jsonPointer": "/bot_id"
            })
        });
        let manifest = serde_json::json!({
            "id": "demo",
            "displayName": "Demo",
            "provider": "demo.wasm",
            "defaultMount": "demo",
            "projection": 1,
            "refreshIntervalSecs": 0,
            "capabilities": [{
                "kind": "domain",
                "value": "api.demo.test",
                "why": "Fetch the objects of the demo."
            }],
            "auth": {
                "default": "token",
                "schemes": [{
                    "staticToken": {
                        "key": "token",
                        "valuePrefix": "Bearer ",
                        "description": "Demo bot token",
                        "injectDomains": ["api.demo.test"],
                        "validation": validation
                    }
                }]
            }
        });
        ProviderManifest::from_bytes(&serde_json::to_vec(&manifest).unwrap()).unwrap()
    }

    /// The submission of `mount add` for the token `xoxb-1` of `manifest`,
    /// after its validation.
    async fn submit(
        manifest: &ProviderManifest,
    ) -> anyhow::Result<omnifs_api::CredentialSubmission> {
        let auth = Auth::from_provider_default(manifest).unwrap();
        crate::commands::mount::run_static_token_init(
            ProviderId::from_digest([7; 32]),
            manifest,
            &auth,
            SecretString::from("xoxb-1".to_owned()),
            true,
            &Output::new(OutputMode::Json, true),
        )
        .await
    }

    /// The probe of a token is the request that the manifest of its provider
    /// declares: its method, its URL, the token with its prefix in the
    /// header of the scheme, and each declared header beside it.
    #[tokio::test]
    async fn the_probe_of_a_token_is_the_request_that_its_manifest_declares() {
        let (address, request) = serve(json_answer(r#"{"ok":true,"bot_id":"B1"}"#)).await;
        let submission = submit(&manifest(address, None)).await.unwrap();
        assert_eq!(submission.scheme, "token");
        assert_eq!(submission.account_label, "default");
        let omnifs_api::CredentialMaterial::StaticToken { token } = &submission.material else {
            panic!("no static token");
        };
        assert_eq!(token.expose(), b"xoxb-1");

        let request = request.await.unwrap();
        assert!(
            request.starts_with("post /api/auth.test http/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains("\r\nauthorization: bearer xoxb-1\r\n"),
            "{request}"
        );
        assert_eq!(
            request.matches("\r\nauthorization:").count(),
            1,
            "{request}"
        );
        assert!(
            request.contains("\r\ndemo-version: 2026-03-11\r\n"),
            "{request}"
        );
    }

    /// The declared header of a scheme carries the token in place of
    /// `Authorization`, and a declared body goes with the request.
    #[tokio::test]
    async fn the_probe_sends_the_token_in_the_header_of_its_scheme() {
        let (address, request) = serve(json_answer(r#"{"data":{"viewer":{"id":"u1"}}}"#)).await;
        let body = r#"{"query":"query { viewer { id } }"}"#;
        let mut value = serde_json::to_value(manifest(
            address,
            Some(serde_json::json!({
                "method": "POST",
                "url": format!("http://{address}/graphql"),
                "body": body,
                "expectStatus": 200,
                "jsonPointer": "/data/viewer/id"
            })),
        ))
        .unwrap();
        value["auth"]["schemes"][0]["staticToken"]["headerName"] = "X-Api-Key".into();
        value["auth"]["schemes"][0]["staticToken"]["valuePrefix"] = "".into();
        let manifest = ProviderManifest::from_bytes(&serde_json::to_vec(&value).unwrap()).unwrap();
        submit(&manifest).await.unwrap();
        let request = request.await.unwrap();
        assert!(
            request.starts_with("post /graphql http/1.1\r\n"),
            "{request}"
        );
        assert!(request.contains("\r\nx-api-key: xoxb-1\r\n"), "{request}");
        assert!(!request.contains("\r\nauthorization:"), "{request}");
        assert!(
            request.contains("\r\ncontent-type: application/json\r\n"),
            "{request}"
        );
        assert!(
            request.contains(&format!("\r\ncontent-length: {}\r\n", body.len())),
            "{request}"
        );
    }

    /// The value at the declared JSON pointer holds for a valid token: an
    /// answer without it, or with `null`, `false` or an empty string at it,
    /// is of a token that the upstream does not take.
    #[tokio::test]
    async fn a_token_is_valid_with_a_value_at_the_declared_json_pointer() {
        for held in [r#""B1""#, "true", "0", "[]", "{}"] {
            let (address, _) =
                serve(json_answer(&format!(r#"{{"ok":true,"bot_id":{held}}}"#))).await;
            submit(&manifest(address, None))
                .await
                .unwrap_or_else(|error| panic!("{held}: {error:#}"));
        }
        for body in [
            r#"{"ok":false,"error":"invalid_auth"}"#,
            r#"{"ok":true,"bot_id":null}"#,
            r#"{"ok":true,"bot_id":false}"#,
            r#"{"ok":true,"bot_id":""}"#,
        ] {
            let (address, _) = serve(json_answer(body)).await;
            let error = submit(&manifest(address, None)).await.unwrap_err();
            assert!(
                error.to_string().contains(
                    "`/bot_id` of the response is absent, null, false, or an empty string"
                ),
                "{body}: {error:#}"
            );
        }
        let (address, _) = serve(json_answer("<html>")).await;
        let error = submit(&manifest(address, None)).await.unwrap_err();
        assert!(
            error.to_string().contains("the response is not JSON"),
            "{error:#}"
        );
    }

    /// The probe follows no redirect: the token reaches the declared URL
    /// alone, and a redirect is a failed validation.
    #[tokio::test]
    async fn the_probe_of_a_token_follows_no_redirect() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let location = target.local_addr().unwrap();
        for status in [
            "301 Moved Permanently",
            "302 Found",
            "307 Temporary Redirect",
        ] {
            let (address, request) = serve(format!(
                "HTTP/1.1 {status}\r\nlocation: http://{location}/api/auth.test\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            ))
            .await;
            let error = submit(&manifest(address, None)).await.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("the probe of a token follows none"),
                "{status}: {error:#}"
            );
            request.await.unwrap();
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), target.accept())
                .await
                .is_err(),
            "the target of a redirect takes no request"
        );
    }

    /// Another status than the declared one is a failed validation.
    #[tokio::test]
    async fn a_token_is_valid_with_the_declared_status_alone() {
        let (address, _) = serve(
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"
                .to_owned(),
        )
        .await;
        let error = submit(&manifest(address, None)).await.unwrap_err();
        assert!(
            error.to_string().contains("expected status 200, got 401"),
            "{error:#}"
        );
    }
}
