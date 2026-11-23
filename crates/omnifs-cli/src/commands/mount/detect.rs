//! Ambient credential detection: a generic interpreter over a provider's
//! declared `authn::AmbientSource`s. The host never hardcodes a provider's
//! probe here; it only knows how to read an env var or run a declared
//! command and take its trimmed stdout as a token. The init flow can offer
//! to import a detected credential instead of starting a fresh auth flow.

use omnifs_auth::{AmbientKind, AmbientSource, AuthManifest};
use secrecy::SecretString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct DetectedCredential {
    source: String,
    value: SecretString,
}

impl DetectedCredential {
    pub(super) fn source(&self) -> &str {
        &self.source
    }

    pub(super) fn value(&self) -> SecretString {
        self.value.clone()
    }
}

/// Reads every ambient source declared across `manifest`'s static-token
/// schemes and returns the credentials found, in declaration order.
pub fn detect(manifest: Option<&AuthManifest>) -> Vec<DetectedCredential> {
    let Some(manifest) = manifest else {
        return Vec::new();
    };
    manifest.ambient_sources().filter_map(read_source).collect()
}

fn read_source(source: &AmbientSource) -> Option<DetectedCredential> {
    match &source.kind {
        AmbientKind::EnvVar { name } => {
            let value = std::env::var_os(name)?;
            // A whitespace-only value is not a usable token; treat it as absent
            // rather than importing blank credentials.
            let value = value.to_string_lossy();
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return None;
            }
            Some(DetectedCredential {
                source: format!("${name}"),
                value: SecretString::from(trimmed.to_string()),
            })
        },
        AmbientKind::Command { argv } => {
            let token = run_command(argv, omnifs_core::SILENT_PEER_TIMEOUT)?;
            Some(DetectedCredential {
                source: source.note.clone(),
                value: SecretString::from(token),
            })
        },
    }
}

/// Runs `argv` as a plain argv exec (never a shell, no string interpolation)
/// and returns its trimmed stdout if it exits successfully. The end of its
/// output, at its exit, ends the wait; a command that holds its output open
/// and writes nothing for `silence` is a silent peer, killed and treated as
/// not found.
fn run_command(argv: &[String], silence: Duration) -> Option<String> {
    let (binary, rest) = argv.split_first()?;
    let mut child = Command::new(binary)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (chunks, output) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) | Err(_) => {
                    let _ = chunks.send(None);
                    return;
                },
                Ok(read) => {
                    if chunks.send(Some(buffer[..read].to_vec())).is_err() {
                        return;
                    }
                },
            }
        }
    });
    let mut bytes = Vec::new();
    loop {
        match output.recv_timeout(silence) {
            Ok(Some(chunk)) => bytes.extend(chunk),
            Ok(None) => break,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            },
        }
    }
    let succeeded = child.wait().ok()?.success();
    let token = String::from_utf8_lossy(&bytes).trim().to_string();
    (succeeded && !token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_auth::{AuthScheme, StaticTokenScheme};
    use secrecy::ExposeSecret;

    fn manifest_with(sources: Vec<AmbientSource>) -> AuthManifest {
        AuthManifest {
            schemes: vec![AuthScheme::StaticToken(StaticTokenScheme {
                key: "pat".to_string(),
                header_name: None,
                value_prefix: "Bearer ".to_string(),
                description: "test".to_string(),
                inject_domains: vec![],
                creation_url: None,
                validation: None,
                ambient_sources: sources,
            })],
        }
    }

    #[test]
    #[allow(unsafe_code)]
    fn detect_env_var_source() {
        // SAFETY: this is the only test in this module and no other test in
        // the crate mutates DETECT_TEST_ENV_VAR, so no concurrent env write
        // can race here.
        unsafe {
            std::env::set_var("DETECT_TEST_ENV_VAR", "token-value");
        }
        let manifest = manifest_with(vec![AmbientSource {
            kind: AmbientKind::EnvVar {
                name: "DETECT_TEST_ENV_VAR".into(),
            },
            note: String::new(),
        }]);
        let found = detect(Some(&manifest));
        assert_eq!(found[0].source(), "$DETECT_TEST_ENV_VAR");
        // SAFETY: same rationale as the set_var above.
        unsafe {
            std::env::remove_var("DETECT_TEST_ENV_VAR");
        }
    }

    #[test]
    fn detect_command_source() {
        let manifest = manifest_with(vec![AmbientSource {
            kind: AmbientKind::Command {
                argv: vec!["echo".into(), "  command-token  ".into()],
            },
            note: "echo probe".into(),
        }]);
        let found = detect(Some(&manifest));
        let credential = found.first().expect("command credential detected");
        assert_eq!(credential.source(), "echo probe");
        assert_eq!(credential.value().expose_secret(), "command-token");
    }

    /// A command that holds its output open and writes nothing is killed
    /// after the silence of the wait, and gives no credential.
    #[test]
    fn a_silent_detect_command_is_killed_after_its_silence() {
        let argv = vec!["sleep".to_owned(), "1000".to_owned()];
        assert_eq!(run_command(&argv, Duration::from_millis(100)), None);
        let argv = vec!["echo".to_owned(), "token".to_owned()];
        assert_eq!(
            run_command(&argv, Duration::from_millis(100)).as_deref(),
            Some("token")
        );
    }
}
