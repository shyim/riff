mod auth;
mod client;
mod forge_auth;
mod no_proxy;
mod proxy;
mod transport;

pub use auth::{
    authentication_options, authentication_retry_decision, is_public_bitbucket_download,
    AuthenticationHeader, AuthenticationOptions, AuthenticationPolicyError,
    AuthenticationRetryDecision, ClientCertificateOptions,
};
pub use client::{HttpClient, HttpClientConfig, HttpError};
pub use forge_auth::{
    BitbucketOAuthSession, BitbucketStoredOAuth, BitbucketTokenOutcome, BitbucketTokenPlan,
    BitbucketTokenRequest, ForgeAuthOutcome, ForgeAuthRequest, ForgeAuthSession,
    ForgeAuthenticationError, ForgeProvider, StoredForgeCredential,
};
pub use no_proxy::NoProxyPattern;
pub use proxy::{
    ProxyCurlOptions, ProxyEnvironment, ProxyItem, ProxyParseError, ProxyRequest,
    ProxyStreamOptions,
};
pub use transport::{
    applicable_http_notices, redirect_is_allowed, HttpNotice, HttpNoticeLevel, HttpRequestOptions,
    HttpTransportPolicyError, HttpWarningMetadata, PreparedHttpUrl, TlsOptions, TransferProgress,
    UrlAuthentication, VersionedHttpNotice,
};

/// Custom token headers must never follow a redirect into another origin.
pub(crate) fn authenticated_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        if attempt
            .previous()
            .last()
            .is_some_and(|previous| previous.origin() != attempt.url().origin())
        {
            attempt.stop()
        } else {
            attempt.follow()
        }
    })
}
