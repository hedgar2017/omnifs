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
    pub(crate) client_secret_env: Option<String>,
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
    fn is_empty(&self) -> bool {
        self.client_id.is_none() && self.client_secret_file.is_none() && self.subject.is_none()
    }

    /// Refuses a client of the invocation, which an OAuth scheme alone
    /// takes.
    pub(crate) fn refuse(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.is_empty(),
            "`--client-id`, `--client-secret-file` and `--subject` need an OAuth auth scheme"
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
            client_secret_env: self.client_secret_env.clone(),
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
    /// alone takes.
    pub(crate) fn with_client(self, client: OAuthClientArgs) -> anyhow::Result<Self> {
        match self {
            Self::OAuth(oauth) => Ok(Self::OAuth(OAuth {
                client_id: client.client_id,
                client_secret_file: client.client_secret_file,
                subject: client.subject,
                ..oauth
            })),
            Self::StaticToken(_) => {
                client.refuse()?;
                Ok(self)
            },
        }
    }

    pub(crate) fn as_oauth(&self) -> Option<&OAuth> {
        match self {
            Self::OAuth(config) => Some(config),
            Self::StaticToken(_) => None,
        }
    }
}
