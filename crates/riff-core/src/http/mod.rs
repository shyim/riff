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
