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

pub(super) async fn validate_static_token(
    validation: &TokenValidation,
    header_name: &str,
    header_prefix: &str,
    token: &str,
    output: &crate::ui::output::Output,
) -> anyhow::Result<ValidationOutcome> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
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
    if u32::from(status.as_u16()) != u32::from(validation.expect_status) {
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
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if let Some(pointer) = validation.json_pointer.as_deref()
        && parsed.pointer(pointer).is_none()
    {
        anyhow::bail!(
            "validation failed: response did not contain `{}`. Response: {}",
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
    use super::validate_static_token;
    use crate::ui::output::{Output, OutputMode};
    use omnifs_auth::TokenValidation;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The probe sends each header of the validation beside the token.
    #[tokio::test]
    async fn the_probe_sends_the_headers_of_the_validation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let listen = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0; 1];
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
                .await
                .unwrap();
            String::from_utf8(request).unwrap().to_ascii_lowercase()
        });
        let validation = TokenValidation {
            method: "GET".to_string(),
            url: format!("http://{address}/v1/users/me"),
            headers: [("Notion-Version".to_string(), "2026-03-11".to_string())].into(),
            body: None,
            expect_status: 200,
            json_pointer: None,
            extract: std::collections::BTreeMap::new(),
        };
        validate_static_token(
            &validation,
            "Authorization",
            "Bearer ",
            "secret",
            &Output::new(OutputMode::Json, true),
        )
        .await
        .unwrap();
        let request = listen.await.unwrap();
        assert!(request.starts_with("get /v1/users/me "), "{request}");
        assert!(
            request.contains("\r\nnotion-version: 2026-03-11\r\n"),
            "{request}"
        );
        assert!(
            request.contains("\r\nauthorization: bearer secret\r\n"),
            "{request}"
        );
    }
}
