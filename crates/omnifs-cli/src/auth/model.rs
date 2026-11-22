//! CLI-owned auth selections used while collecting credential submissions.
//!
//! Mount definitions and credential storage belong to the daemon. The CLI
//! only keeps the small set of auth options needed to drive its OAuth and
//! static-token prompts.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum Auth {
    StaticToken(StaticToken),
    #[serde(rename = "oauth")]
    OAuth(OAuth),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct StaticToken {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct OAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_secret_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) redirect_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scopes: Option<Vec<String>>,
    /// The subject of the assertions of a JWT bearer grant, in place of the
    /// client itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) subject: Option<String>,
}

/// The OAuth client of an invocation: its ID, the file of its secret, and
/// the subject that it asserts.
pub(crate) struct OAuthClientArgs {
    pub(crate) client_id: Option<String>,
    pub(crate) client_secret_file: Option<String>,
    pub(crate) subject: Option<String>,
}

impl OAuthClientArgs {
    /// Refuses each flag of a client of the invocation, which an OAuth
    /// scheme alone takes, for `auth`, which is none.
    pub(crate) fn refuse(&self, auth: &str) -> anyhow::Result<()> {
        let given: Vec<&str> = [
            ("`--client-id`", self.client_id.is_some()),
            ("`--client-secret-file`", self.client_secret_file.is_some()),
            ("`--subject`", self.subject.is_some()),
        ]
        .into_iter()
        .filter_map(|(flag, given)| given.then_some(flag))
        .collect();
        anyhow::ensure!(
            given.is_empty(),
            "{} {} an OAuth auth scheme, and {auth}",
            given.join(", "),
            if given.len() == 1 { "needs" } else { "need" }
        );
        Ok(())
    }
}

impl OAuth {
    pub(crate) fn request_config(&self) -> omnifs_auth::OAuthRequestConfig {
        omnifs_auth::OAuthRequestConfig {
            scopes: self.scopes.clone(),
            domain: self.domain.clone(),
            header: self.header.clone(),
            redirect_uri: self.redirect_uri.clone(),
            client_id: self.client_id.clone(),
            client_secret_file: self.client_secret_file.clone(),
            subject: self.subject.clone(),
        }
    }
}

impl Auth {
    pub(crate) fn scheme(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.scheme.as_deref(),
            Self::OAuth(config) => config.scheme.as_deref(),
        }
    }

    pub(crate) fn account(&self) -> Option<&str> {
        match self {
            Self::StaticToken(config) => config.account.as_deref(),
            Self::OAuth(config) => config.account.as_deref(),
        }
    }

    /// This auth's account label, or the fixed default every unlabeled
    /// account uses.
    pub(crate) fn account_or_default(&self) -> &str {
        self.account().unwrap_or("default")
    }

    pub(crate) fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }

    /// This auth for the credential of `account`.
    pub(crate) fn with_account(mut self, account: Option<String>) -> Self {
        match &mut self {
            Self::StaticToken(config) => config.account = account,
            Self::OAuth(config) => config.account = account,
        }
        self
    }

    /// This auth with the client of an invocation, which an OAuth scheme
    /// alone takes, and whose subject a flow without a sign-in alone
    /// asserts: the user of a sign-in is the subject of its own grant.
    pub(crate) fn with_client(
        self,
        client: OAuthClientArgs,
        auth_manifest: Option<&omnifs_auth::AuthManifest>,
    ) -> anyhow::Result<Self> {
        match self {
            Self::OAuth(oauth) => {
                if client.subject.is_some()
                    && let Some(scheme) = auth_manifest.and_then(|manifest| {
                        manifest.resolve_oauth_scheme(oauth.scheme.as_deref()).ok()
                    })
                    && scheme.flow.needs_user()
                {
                    anyhow::bail!(
                        "`--subject` needs the auth scheme of a JWT bearer grant, whose client asserts its subject, and the auth scheme `{}` has a sign-in, whose user is its subject",
                        scheme.key
                    );
                }
                Ok(Self::OAuth(OAuth {
                    client_id: client.client_id,
                    client_secret_file: client.client_secret_file,
                    subject: client.subject,
                    ..oauth
                }))
            },
            Self::StaticToken(token) => {
                client.refuse(&format!(
                    "the auth scheme `{}` takes a static token",
                    token.scheme.as_deref().unwrap_or_default()
                ))?;
                Ok(Self::StaticToken(token))
            },
        }
    }

    /// This auth with the scopes of an invocation, which an OAuth scheme
    /// alone requests; none of the invocation leave the ones of the auth.
    pub(crate) fn with_scopes(self, scopes: &[String]) -> anyhow::Result<Self> {
        if scopes.is_empty() {
            return Ok(self);
        }
        match self {
            Self::OAuth(oauth) => Ok(Self::OAuth(OAuth {
                scopes: Some(scopes.to_vec()),
                ..oauth
            })),
            Self::StaticToken(token) => anyhow::bail!(
                "`--scope` needs an OAuth auth scheme, and the auth scheme `{}` takes a static token",
                token.scheme.as_deref().unwrap_or_default()
            ),
        }
    }

    pub(crate) fn as_oauth(&self) -> Option<&OAuth> {
        match self {
            Self::OAuth(config) => Some(config),
            Self::StaticToken(_) => None,
        }
    }
}
