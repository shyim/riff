//! Authentication flows shared by hosted forge integrations.
//!
//! Prompting and persistence stay with the CLI. This module validates supplied
//! answers, describes the request to make, evaluates responses, and returns the
//! origin-scoped credential update which may be persisted by the caller.

use std::fmt;

use serde_json::Value;
use thiserror::Error;

const BITBUCKET_ORIGIN: &str = "bitbucket.org";
const BITBUCKET_TOKEN_ENDPOINT: &str = "https://bitbucket.org/site/oauth2/access_token";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeProvider {
    GitHub,
    GitLab,
    Forgejo,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ForgeAuthenticationError {
    #[error("unsupported authentication origin: {0}")]
    WrongOrigin(String),
    #[error("{0} must not be empty")]
    EmptyCredential(&'static str),
    #[error("authentication endpoint returned HTTP {0}")]
    Transport(u16),
    #[error("authentication endpoint returned an invalid response")]
    InvalidResponse,
    #[error("invalid {provider} credentials {attempts} times in a row, aborting")]
    AttemptsExhausted {
        provider: &'static str,
        attempts: u8,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub struct ForgeAuthRequest {
    pub endpoint: String,
    pub method: &'static str,
    pub form: Vec<(String, String)>,
    username: String,
    secret: String,
}

impl fmt::Debug for ForgeAuthRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForgeAuthRequest")
            .field("endpoint", &self.endpoint)
            .field("method", &self.method)
            .field("form", &"<redacted>")
            .field("username", &"<redacted>")
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct StoredForgeCredential {
    pub origin: String,
    pub username: String,
    pub password: String,
    pub remove_legacy_key: Option<String>,
}

impl fmt::Debug for StoredForgeCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredForgeCredential")
            .field("origin", &self.origin)
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .field("remove_legacy_key", &self.remove_legacy_key)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeAuthOutcome {
    Authorized(StoredForgeCredential),
    Rejected,
}

#[derive(Debug, Clone)]
pub struct ForgeAuthSession {
    provider: ForgeProvider,
    scheme: String,
    origin: String,
    failures: u8,
}

impl ForgeAuthSession {
    pub fn new(
        provider: ForgeProvider,
        scheme: impl Into<String>,
        origin: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            scheme: scheme.into(),
            origin: origin.into(),
            failures: 0,
        }
    }

    pub fn request(
        &self,
        username: impl Into<String>,
        secret: impl Into<String>,
    ) -> Result<ForgeAuthRequest, ForgeAuthenticationError> {
        let username = username.into();
        let secret = secret.into();
        match self.provider {
            ForgeProvider::GitHub => {
                if secret.is_empty() {
                    return Err(ForgeAuthenticationError::EmptyCredential("token"));
                }
                Ok(ForgeAuthRequest {
                    endpoint: format!("https://api.{}/", self.origin),
                    method: "GET",
                    form: Vec::new(),
                    username,
                    secret,
                })
            }
            ForgeProvider::GitLab => {
                require_pair(&username, &secret, "username", "password")?;
                Ok(ForgeAuthRequest {
                    endpoint: format!("{}://{}/oauth/token", self.scheme, self.origin),
                    method: "POST",
                    form: vec![
                        ("grant_type".to_owned(), "password".to_owned()),
                        ("username".to_owned(), username.clone()),
                        ("password".to_owned(), secret.clone()),
                    ],
                    username,
                    secret,
                })
            }
            ForgeProvider::Forgejo => {
                require_pair(&username, &secret, "username", "token")?;
                Ok(ForgeAuthRequest {
                    endpoint: format!("https://{}/api/v1/version", self.origin),
                    method: "GET",
                    form: Vec::new(),
                    username,
                    secret,
                })
            }
        }
    }

    pub fn complete(
        &mut self,
        request: &ForgeAuthRequest,
        status: u16,
        body: &str,
    ) -> Result<ForgeAuthOutcome, ForgeAuthenticationError> {
        if !(200..300).contains(&status) {
            self.failures = self.failures.saturating_add(1);
            if self.provider == ForgeProvider::GitLab && self.failures >= 5 {
                return Err(ForgeAuthenticationError::AttemptsExhausted {
                    provider: "GitLab",
                    attempts: self.failures,
                });
            }
            return Ok(ForgeAuthOutcome::Rejected);
        }
        self.failures = 0;
        let credential = match self.provider {
            ForgeProvider::GitHub => StoredForgeCredential {
                origin: self.origin.clone(),
                username: request.secret.clone(),
                password: "x-oauth-basic".to_owned(),
                remove_legacy_key: Some(format!("github-oauth.{}", self.origin)),
            },
            ForgeProvider::GitLab => {
                let response: Value = serde_json::from_str(body)
                    .map_err(|_| ForgeAuthenticationError::InvalidResponse)?;
                let token = response
                    .get("access_token")
                    .and_then(Value::as_str)
                    .filter(|token| !token.is_empty())
                    .ok_or(ForgeAuthenticationError::InvalidResponse)?;
                StoredForgeCredential {
                    origin: self.origin.clone(),
                    username: token.to_owned(),
                    password: "oauth2".to_owned(),
                    remove_legacy_key: None,
                }
            }
            ForgeProvider::Forgejo => StoredForgeCredential {
                origin: self.origin.clone(),
                username: request.username.clone(),
                password: request.secret.clone(),
                remove_legacy_key: Some(format!("forgejo-token.{}", self.origin)),
            },
        };
        Ok(ForgeAuthOutcome::Authorized(credential))
    }

    pub fn failures(&self) -> u8 {
        self.failures
    }
}

fn require_pair(
    username: &str,
    secret: &str,
    username_name: &'static str,
    secret_name: &'static str,
) -> Result<(), ForgeAuthenticationError> {
    if username.is_empty() {
        return Err(ForgeAuthenticationError::EmptyCredential(username_name));
    }
    if secret.is_empty() {
        return Err(ForgeAuthenticationError::EmptyCredential(secret_name));
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
pub struct BitbucketStoredOAuth {
    pub consumer_key: String,
    pub consumer_secret: String,
    pub access_token: String,
    pub expires_at: u64,
}

impl fmt::Debug for BitbucketStoredOAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BitbucketStoredOAuth")
            .field("consumer_key", &"<redacted>")
            .field("consumer_secret", &"<redacted>")
            .field("access_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BitbucketTokenRequest {
    pub endpoint: &'static str,
    pub method: &'static str,
    pub form: &'static str,
    pub retry_auth_failure: bool,
    pub remove_basic_auth: bool,
    consumer_key: String,
    consumer_secret: String,
}

impl fmt::Debug for BitbucketTokenRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BitbucketTokenRequest")
            .field("endpoint", &self.endpoint)
            .field("method", &self.method)
            .field("form", &self.form)
            .field("retry_auth_failure", &self.retry_auth_failure)
            .field("remove_basic_auth", &self.remove_basic_auth)
            .field("consumer_key", &"<redacted>")
            .field("consumer_secret", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BitbucketTokenPlan {
    Cached,
    Request(BitbucketTokenRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BitbucketTokenOutcome {
    Authorized(BitbucketStoredOAuth),
    InvalidConsumer,
}

#[derive(Debug, Clone)]
pub struct BitbucketOAuthSession {
    now: u64,
    token: Option<String>,
}

impl BitbucketOAuthSession {
    pub fn new(now: u64) -> Self {
        Self { now, token: None }
    }

    pub fn token(&self) -> &str {
        self.token.as_deref().unwrap_or("")
    }

    pub fn plan(
        &mut self,
        origin: &str,
        consumer_key: impl Into<String>,
        consumer_secret: impl Into<String>,
        stored: Option<&BitbucketStoredOAuth>,
    ) -> Result<BitbucketTokenPlan, ForgeAuthenticationError> {
        self.plan_with_persistence(origin, consumer_key, consumer_secret, stored, false)
    }

    pub fn interactive_plan(
        &mut self,
        origin: &str,
        consumer_key: impl Into<String>,
        consumer_secret: impl Into<String>,
    ) -> Result<BitbucketTokenRequest, ForgeAuthenticationError> {
        match self.plan_with_persistence(origin, consumer_key, consumer_secret, None, true)? {
            BitbucketTokenPlan::Request(request) => Ok(request),
            BitbucketTokenPlan::Cached => unreachable!("interactive plans do not use cache"),
        }
    }

    fn plan_with_persistence(
        &mut self,
        origin: &str,
        consumer_key: impl Into<String>,
        consumer_secret: impl Into<String>,
        stored: Option<&BitbucketStoredOAuth>,
        remove_basic_auth: bool,
    ) -> Result<BitbucketTokenPlan, ForgeAuthenticationError> {
        if origin != BITBUCKET_ORIGIN {
            return Err(ForgeAuthenticationError::WrongOrigin(origin.to_owned()));
        }
        let consumer_key = consumer_key.into();
        let consumer_secret = consumer_secret.into();
        require_pair(
            &consumer_key,
            &consumer_secret,
            "consumer key",
            "consumer secret",
        )?;
        if let Some(stored) = stored.filter(|stored| {
            stored.consumer_key == consumer_key
                && stored.consumer_secret == consumer_secret
                && stored.expires_at > self.now
        }) {
            self.token = Some(stored.access_token.clone());
            return Ok(BitbucketTokenPlan::Cached);
        }
        Ok(BitbucketTokenPlan::Request(BitbucketTokenRequest {
            endpoint: BITBUCKET_TOKEN_ENDPOINT,
            method: "POST",
            form: "grant_type=client_credentials",
            retry_auth_failure: false,
            remove_basic_auth,
            consumer_key,
            consumer_secret,
        }))
    }

    pub fn complete(
        &mut self,
        request: &BitbucketTokenRequest,
        status: u16,
        body: &str,
    ) -> Result<BitbucketTokenOutcome, ForgeAuthenticationError> {
        if matches!(status, 400 | 401) {
            self.token = None;
            return Ok(BitbucketTokenOutcome::InvalidConsumer);
        }
        if !(200..300).contains(&status) {
            return Err(ForgeAuthenticationError::Transport(status));
        }
        let response: Value =
            serde_json::from_str(body).map_err(|_| ForgeAuthenticationError::InvalidResponse)?;
        let access_token = response
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or(ForgeAuthenticationError::InvalidResponse)?;
        let expires_in = response
            .get("expires_in")
            .and_then(Value::as_u64)
            .ok_or(ForgeAuthenticationError::InvalidResponse)?;
        self.token = Some(access_token.to_owned());
        Ok(BitbucketTokenOutcome::Authorized(BitbucketStoredOAuth {
            consumer_key: request.consumer_key.clone(),
            consumer_secret: request.consumer_secret.clone(),
            access_token: access_token.to_owned(),
            expires_at: self.now.saturating_add(expires_in),
        }))
    }

    pub fn authorize_from_git_config(&mut self, origin: &str, token: Option<&str>) -> bool {
        if origin != BITBUCKET_ORIGIN {
            return false;
        }
        let Some(token) = token.filter(|token| !token.is_empty()) else {
            return false;
        };
        self.token = Some(token.to_owned());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;
    const TOKEN_RESPONSE: &str = r#"{
        "access_token": "bitbuckettoken",
        "expires_in": 3600,
        "refresh_token": "refreshtoken",
        "token_type": "bearer"
    }"#;

    fn stored(expires_at: u64) -> BitbucketStoredOAuth {
        BitbucketStoredOAuth {
            consumer_key: "consumer_key".to_owned(),
            consumer_secret: "consumer_secret".to_owned(),
            access_token: "bitbuckettoken".to_owned(),
            expires_at,
        }
    }

    fn bitbucket_request(session: &mut BitbucketOAuthSession) -> BitbucketTokenRequest {
        match session
            .plan(BITBUCKET_ORIGIN, "consumer_key", "consumer_secret", None)
            .unwrap()
        {
            BitbucketTokenPlan::Request(request) => request,
            BitbucketTokenPlan::Cached => panic!("expected token request"),
        }
    }

    #[test]
    fn composer_bitbucket_interactive_authorization_rejects_empty_secret() {
        let mut session = BitbucketOAuthSession::new(NOW);
        assert_eq!(
            session.interactive_plan(BITBUCKET_ORIGIN, "consumer_key", ""),
            Err(ForgeAuthenticationError::EmptyCredential("consumer secret"))
        );
    }

    #[test]
    fn composer_bitbucket_interactive_authorization_rejects_empty_key() {
        let mut session = BitbucketOAuthSession::new(NOW);
        assert_eq!(
            session.interactive_plan(BITBUCKET_ORIGIN, "", "consumer_secret"),
            Err(ForgeAuthenticationError::EmptyCredential("consumer key"))
        );
    }

    #[test]
    fn composer_bitbucket_interactive_authorization_reports_token_failure() {
        let mut session = BitbucketOAuthSession::new(NOW);
        let request = session
            .interactive_plan(BITBUCKET_ORIGIN, "consumer_key", "consumer_secret")
            .unwrap();
        assert_eq!(
            session.complete(&request, 400, "{}"),
            Ok(BitbucketTokenOutcome::InvalidConsumer)
        );
        assert_eq!(session.token(), "");
    }

    #[test]
    fn composer_bitbucket_authorizes_available_git_config_token() {
        let mut session = BitbucketOAuthSession::new(NOW);
        assert!(session.authorize_from_git_config(BITBUCKET_ORIGIN, Some("git-config-token")));
        assert_eq!(session.token(), "git-config-token");
    }

    #[test]
    fn composer_bitbucket_rejects_non_bitbucket_origin() {
        let mut session = BitbucketOAuthSession::new(NOW);
        assert!(!session.authorize_from_git_config("non-bitbucket.org", Some("git-config-token")));
        assert!(matches!(
            session.plan("non-bitbucket.org", "consumer_key", "consumer_secret", None),
            Err(ForgeAuthenticationError::WrongOrigin(_))
        ));
    }

    #[test]
    fn composer_bitbucket_rejects_missing_git_config_token() {
        let mut session = BitbucketOAuthSession::new(NOW);
        assert!(!session.authorize_from_git_config(BITBUCKET_ORIGIN, None));
        assert!(!session.authorize_from_git_config(BITBUCKET_ORIGIN, Some("")));
    }

    #[test]
    fn composer_bitbucket_exposes_acquired_access_token() {
        let mut session = BitbucketOAuthSession::new(NOW);
        let request = bitbucket_request(&mut session);
        session.complete(&request, 200, TOKEN_RESPONSE).unwrap();
        assert_eq!(session.token(), "bitbuckettoken");
    }

    #[test]
    fn composer_bitbucket_has_no_token_before_authorization() {
        assert_eq!(BitbucketOAuthSession::new(NOW).token(), "");
    }

    #[test]
    fn composer_bitbucket_rejects_username_password_as_oauth_consumer() {
        let mut session = BitbucketOAuthSession::new(NOW);
        let request = match session
            .plan(BITBUCKET_ORIGIN, "username", "password", None)
            .unwrap()
        {
            BitbucketTokenPlan::Request(request) => request,
            BitbucketTokenPlan::Cached => panic!("expected token request"),
        };
        assert_eq!(
            session.complete(&request, 400, "{}"),
            Ok(BitbucketTokenOutcome::InvalidConsumer)
        );
    }

    #[test]
    fn composer_bitbucket_propagates_not_found_token_response() {
        let mut session = BitbucketOAuthSession::new(NOW);
        let request = bitbucket_request(&mut session);
        assert_eq!(
            session.complete(&request, 404, "{}"),
            Err(ForgeAuthenticationError::Transport(404))
        );
    }

    #[test]
    fn composer_bitbucket_rejects_unauthorized_oauth_consumer() {
        let mut session = BitbucketOAuthSession::new(NOW);
        let request = bitbucket_request(&mut session);
        assert_eq!(
            session.complete(&request, 401, "{}"),
            Ok(BitbucketTokenOutcome::InvalidConsumer)
        );
    }

    #[test]
    fn composer_bitbucket_requests_and_stores_valid_oauth_token() {
        let mut session = BitbucketOAuthSession::new(NOW);
        let request = bitbucket_request(&mut session);
        assert_eq!(request.endpoint, BITBUCKET_TOKEN_ENDPOINT);
        assert_eq!(request.method, "POST");
        assert_eq!(request.form, "grant_type=client_credentials");
        assert!(!request.retry_auth_failure);
        assert_eq!(
            session.complete(&request, 200, TOKEN_RESPONSE).unwrap(),
            BitbucketTokenOutcome::Authorized(stored(NOW + 3600))
        );
    }

    #[test]
    fn composer_bitbucket_reuses_unexpired_stored_token() {
        let mut session = BitbucketOAuthSession::new(NOW);
        assert_eq!(
            session
                .plan(
                    BITBUCKET_ORIGIN,
                    "consumer_key",
                    "consumer_secret",
                    Some(&stored(NOW + 1800))
                )
                .unwrap(),
            BitbucketTokenPlan::Cached
        );
        assert_eq!(session.token(), "bitbuckettoken");
    }

    #[test]
    fn composer_bitbucket_refreshes_expired_stored_token() {
        let mut session = BitbucketOAuthSession::new(NOW);
        let plan = session
            .plan(
                BITBUCKET_ORIGIN,
                "consumer_key",
                "consumer_secret",
                Some(&stored(NOW - 400)),
            )
            .unwrap();
        let BitbucketTokenPlan::Request(request) = plan else {
            panic!("expired token must be refreshed");
        };
        assert_eq!(
            session.complete(&request, 200, TOKEN_RESPONSE).unwrap(),
            BitbucketTokenOutcome::Authorized(stored(NOW + 3600))
        );
    }

    #[test]
    fn composer_bitbucket_interactive_flow_removes_basic_auth_after_success() {
        let mut session = BitbucketOAuthSession::new(NOW);
        let request = session
            .interactive_plan(BITBUCKET_ORIGIN, "consumer_key", "consumer_secret")
            .unwrap();
        assert!(request.remove_basic_auth);
        assert!(matches!(
            session.complete(&request, 200, TOKEN_RESPONSE),
            Ok(BitbucketTokenOutcome::Authorized(_))
        ));
        assert_eq!(session.token(), "bitbuckettoken");
    }

    #[test]
    fn composer_github_interactive_token_flow_stores_valid_token() {
        let mut session = ForgeAuthSession::new(ForgeProvider::GitHub, "https", "github.com");
        let request = session.request("", "password").unwrap();
        assert_eq!(request.endpoint, "https://api.github.com/");
        assert_eq!(request.method, "GET");
        let ForgeAuthOutcome::Authorized(credential) =
            session.complete(&request, 200, "{}").unwrap()
        else {
            panic!("expected authorization");
        };
        assert_eq!(credential.origin, "github.com");
        assert_eq!(credential.username, "password");
        assert_eq!(credential.password, "x-oauth-basic");
        assert_eq!(
            credential.remove_legacy_key.as_deref(),
            Some("github-oauth.github.com")
        );
        assert!(!format!("{request:?}").contains("password"));
    }

    #[test]
    fn composer_github_interactive_token_flow_rejects_failed_probe() {
        let mut session = ForgeAuthSession::new(ForgeProvider::GitHub, "https", "github.com");
        let request = session.request("", "password").unwrap();
        assert_eq!(
            session.complete(&request, 401, "{}"),
            Ok(ForgeAuthOutcome::Rejected)
        );
    }

    #[test]
    fn composer_gitlab_interactive_password_flow_stores_access_token() {
        let mut session = ForgeAuthSession::new(ForgeProvider::GitLab, "http", "gitlab.com");
        let request = session.request("username", "password").unwrap();
        assert_eq!(request.endpoint, "http://gitlab.com/oauth/token");
        assert_eq!(request.method, "POST");
        let body = r#"{"access_token":"gitlabtoken","refresh_token":"gitlabrefreshtoken"}"#;
        let ForgeAuthOutcome::Authorized(credential) =
            session.complete(&request, 200, body).unwrap()
        else {
            panic!("expected authorization");
        };
        assert_eq!(credential.username, "gitlabtoken");
        assert_eq!(credential.password, "oauth2");
    }

    #[test]
    fn composer_gitlab_aborts_after_five_failed_password_attempts() {
        let mut session = ForgeAuthSession::new(ForgeProvider::GitLab, "https", "gitlab.com");
        for attempt in 1..=4 {
            let request = session.request("username", "password").unwrap();
            assert_eq!(
                session.complete(&request, 401, "{}"),
                Ok(ForgeAuthOutcome::Rejected),
                "attempt {attempt}"
            );
        }
        let request = session.request("username", "password").unwrap();
        assert_eq!(
            session.complete(&request, 401, "{}"),
            Err(ForgeAuthenticationError::AttemptsExhausted {
                provider: "GitLab",
                attempts: 5,
            })
        );
    }

    #[test]
    fn composer_forgejo_interactive_token_flow_stores_valid_token() {
        let mut session = ForgeAuthSession::new(ForgeProvider::Forgejo, "https", "codeberg.org");
        let request = session.request("username", "access-token").unwrap();
        assert_eq!(request.endpoint, "https://codeberg.org/api/v1/version");
        let ForgeAuthOutcome::Authorized(credential) =
            session.complete(&request, 200, "{}").unwrap()
        else {
            panic!("expected authorization");
        };
        assert_eq!(credential.username, "username");
        assert_eq!(credential.password, "access-token");
        assert_eq!(
            credential.remove_legacy_key.as_deref(),
            Some("forgejo-token.codeberg.org")
        );
    }

    #[test]
    fn composer_forgejo_interactive_token_flow_rejects_failed_probe() {
        let mut session = ForgeAuthSession::new(ForgeProvider::Forgejo, "https", "codeberg.org");
        let request = session.request("username", "access-token").unwrap();
        assert_eq!(
            session.complete(&request, 404, "{}"),
            Ok(ForgeAuthOutcome::Rejected)
        );
    }
}
