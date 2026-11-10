//! Valid access tokens for API calls.

use std::fmt;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::{AccessToken, AuthError, Authorization, OAuthClient, TokenStore};

/// Refresh an access token this long before it expires, to allow for clock skew
/// and the time a request spends in flight.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// An error from a [`TokenProvider`], such as a failed refresh or a revoked grant.
///
/// For the providers in this module it is an [`AuthError`]; downcast it to ask
/// [`AuthError::requires_sign_in`].
pub type TokenError = Box<dyn std::error::Error + Send + Sync>;

/// The future a [`TokenProvider`] returns: an access token or an error.
pub type TokenFuture<'a> = Pin<Box<dyn Future<Output = Result<String, TokenError>> + Send + 'a>>;

/// Supplies OAuth access tokens to the Gmail REST client.
///
/// The client asks for a token before each request, and asks for a fresh one once
/// when Gmail rejects a token with `401 Unauthorized`. The trait is object safe, so
/// the client can hold an `Arc<dyn TokenProvider>`.
pub trait TokenProvider: Send + Sync {
    /// Returns a valid access token, refreshing it first if it has expired.
    fn access_token(&self) -> TokenFuture<'_>;

    /// Returns a new access token after Gmail rejected `rejected`.
    ///
    /// Implementations should refresh unless the current token already differs
    /// from `rejected` (another request refreshed it first), in which case they
    /// can return the current one.
    fn refresh_access_token<'a>(&'a self, rejected: &'a str) -> TokenFuture<'a>;
}

/// A provider that always returns the same token. Useful in tests.
#[derive(Debug, Clone)]
pub struct StaticTokenProvider {
    token: String,
}

impl StaticTokenProvider {
    /// A provider that always returns `token`.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

impl TokenProvider for StaticTokenProvider {
    fn access_token(&self) -> TokenFuture<'_> {
        Box::pin(ready(Ok(self.token.clone())))
    }

    fn refresh_access_token<'a>(&'a self, _rejected: &'a str) -> TokenFuture<'a> {
        self.access_token()
    }
}

/// Keeps one account's access token fresh using its stored refresh token.
///
/// The access token is cached in memory and refreshed when it is within
/// [`REFRESH_MARGIN`] of expiring, or when the API rejects it. Concurrent callers
/// share a single refresh. The refresh token is read from the [`TokenStore`] on
/// each refresh, and replaced there if Google rotates it.
pub struct RefreshingTokenProvider {
    client: OAuthClient,
    store: Arc<dyn TokenStore>,
    account: String,
    cached: Mutex<Option<AccessToken>>,
    refreshing: tokio::sync::Mutex<()>,
}

impl RefreshingTokenProvider {
    /// A provider for `account`, whose refresh token is already in `store`.
    pub fn new(
        client: OAuthClient,
        store: Arc<dyn TokenStore>,
        account: impl Into<String>,
    ) -> Self {
        Self {
            client,
            store,
            account: account.into(),
            cached: Mutex::new(None),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    /// Saves a fresh sign-in's refresh token to `store` and returns a provider
    /// that starts with its access token.
    ///
    /// # Errors
    ///
    /// [`AuthError::TokenStore`] if the refresh token cannot be saved.
    pub async fn from_authorization(
        client: OAuthClient,
        store: Arc<dyn TokenStore>,
        authorization: Authorization,
    ) -> Result<Self, AuthError> {
        let provider = Self::new(client, store, authorization.email);
        let refresh_token = authorization.refresh_token;
        provider
            .with_store(move |store, account| store.save(account, &refresh_token))
            .await?;
        *provider.lock_cached() = Some(authorization.access_token);
        Ok(provider)
    }

    /// The account email this provider issues tokens for.
    pub fn account(&self) -> &str {
        &self.account
    }

    /// Returns an access token that is valid now, refreshing it first if needed.
    ///
    /// # Errors
    ///
    /// [`AuthError::NotSignedIn`] if no refresh token is stored,
    /// [`AuthError::InvalidGrant`] if Google revoked it, otherwise a token
    /// endpoint or token store error.
    pub async fn token(&self) -> Result<AccessToken, AuthError> {
        self.usable_or_refresh(None).await
    }

    /// Returns a new access token after the API rejected `rejected`, or the
    /// current one if it has already been replaced.
    ///
    /// # Errors
    ///
    /// As for [`token`](Self::token).
    pub async fn token_after_rejection(&self, rejected: &str) -> Result<AccessToken, AuthError> {
        self.usable_or_refresh(Some(rejected)).await
    }

    async fn usable_or_refresh(&self, rejected: Option<&str>) -> Result<AccessToken, AuthError> {
        if let Some(token) = self.cached_usable(rejected) {
            return Ok(token);
        }
        let _refreshing = self.refreshing.lock().await;
        // Another caller may have refreshed while this one waited.
        if let Some(token) = self.cached_usable(rejected) {
            return Ok(token);
        }

        let refresh_token = self
            .with_store(|store, account| store.load(account))
            .await?
            .ok_or_else(|| AuthError::NotSignedIn(self.account.clone()))?;
        let refreshed = self.client.refresh(&refresh_token).await?;
        if let Some(rotated) = refreshed.refresh_token.filter(|t| *t != refresh_token) {
            self.with_store(move |store, account| store.save(account, &rotated))
                .await?;
        }

        *self.lock_cached() = Some(refreshed.access_token.clone());
        Ok(refreshed.access_token)
    }

    /// The cached token, unless it is about to expire or is the rejected one.
    fn cached_usable(&self, rejected: Option<&str>) -> Option<AccessToken> {
        self.lock_cached()
            .as_ref()
            .filter(|token| !token.expires_within(REFRESH_MARGIN))
            .filter(|token| rejected != Some(token.secret()))
            .cloned()
    }

    fn lock_cached(&self) -> std::sync::MutexGuard<'_, Option<AccessToken>> {
        self.cached.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs a token store call on a blocking thread, since keychains can block.
    async fn with_store<T: Send + 'static>(
        &self,
        call: impl FnOnce(&dyn TokenStore, &str) -> Result<T, AuthError> + Send + 'static,
    ) -> Result<T, AuthError> {
        let store = Arc::clone(&self.store);
        let account = self.account.clone();
        tokio::task::spawn_blocking(move || call(store.as_ref(), &account))
            .await
            .map_err(|error| AuthError::TokenStore(Box::new(error)))?
    }
}

impl TokenProvider for RefreshingTokenProvider {
    fn access_token(&self) -> TokenFuture<'_> {
        Box::pin(async move {
            let token = self.token().await?;
            Ok(token.secret().to_owned())
        })
    }

    fn refresh_access_token<'a>(&'a self, rejected: &'a str) -> TokenFuture<'a> {
        Box::pin(async move {
            let token = self.token_after_rejection(rejected).await?;
            Ok(token.secret().to_owned())
        })
    }
}

impl fmt::Debug for RefreshingTokenProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshingTokenProvider")
            .field("client", &self.client)
            .field("account", &self.account)
            .field("cached", &*self.lock_cached())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::{MemoryTokenStore, OAuthConfig};

    const ACCOUNT: &str = "someone@example.com";
    const STORED: &str = "1//stored";

    fn token(access_token: &str, expires_in: u64) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "access_token": access_token,
            "expires_in": expires_in,
            "scope": "https://www.googleapis.com/auth/gmail.modify",
            "token_type": "Bearer",
        }))
    }

    async fn mock_refresh(server: &MockServer, response: ResponseTemplate, times: u64) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(response)
            .expect(times)
            .mount(server)
            .await;
    }

    fn client(server: &MockServer) -> OAuthClient {
        let config = OAuthConfig::new("client-id")
            .with_client_secret("client-secret")
            .with_endpoints(
                format!("{}/auth", server.uri()),
                format!("{}/token", server.uri()),
            );
        OAuthClient::new(&config).unwrap()
    }

    fn signed_in_store() -> Arc<MemoryTokenStore> {
        let store = Arc::new(MemoryTokenStore::new());
        store.save(ACCOUNT, STORED).unwrap();
        store
    }

    fn provider(server: &MockServer, store: Arc<MemoryTokenStore>) -> RefreshingTokenProvider {
        RefreshingTokenProvider::new(client(server), store, ACCOUNT)
    }

    #[tokio::test]
    async fn refreshes_once_then_serves_the_cached_token() {
        let server = MockServer::start().await;
        mock_refresh(&server, token("ya29.first", 3599), 1).await;
        let provider = provider(&server, signed_in_store());

        let first = provider.token().await.unwrap();
        let second = provider.token().await.unwrap();
        assert_eq!(first.secret(), "ya29.first");
        assert_eq!(second.secret(), "ya29.first");

        let requests = server.received_requests().await.unwrap();
        let form: HashMap<String, String> = url::form_urlencoded::parse(&requests[0].body)
            .into_owned()
            .collect();
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["refresh_token"], STORED);
        assert_eq!(form["client_id"], "client-id");
        assert_eq!(form["client_secret"], "client-secret");
    }

    #[tokio::test]
    async fn refreshes_a_token_about_to_expire() {
        let server = MockServer::start().await;
        // Expires inside the refresh margin, so it is never served from the cache.
        mock_refresh(&server, token("ya29.short", 30), 2).await;
        let provider = provider(&server, signed_in_store());

        provider.token().await.unwrap();
        provider.token().await.unwrap();
    }

    #[tokio::test]
    async fn a_rejected_token_is_refreshed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(token("ya29.first", 3599))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        mock_refresh(&server, token("ya29.second", 3599), 1).await;
        let provider = provider(&server, signed_in_store());

        let rejected = provider.token().await.unwrap();
        let fresh = provider
            .token_after_rejection(rejected.secret())
            .await
            .unwrap();
        assert_eq!(fresh.secret(), "ya29.second");
        assert_eq!(provider.token().await.unwrap().secret(), "ya29.second");
    }

    #[tokio::test]
    async fn rejecting_an_old_token_returns_the_current_one() {
        let server = MockServer::start().await;
        mock_refresh(&server, token("ya29.current", 3599), 1).await;
        let provider = provider(&server, signed_in_store());

        provider.token().await.unwrap();
        let current = provider.token_after_rejection("ya29.old").await.unwrap();
        assert_eq!(current.secret(), "ya29.current");
    }

    #[tokio::test]
    async fn concurrent_rejections_share_one_refresh() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(token("ya29.first", 3599))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        let slow = token("ya29.second", 3599).set_delay(Duration::from_millis(200));
        mock_refresh(&server, slow, 1).await;
        let provider = Arc::new(provider(&server, signed_in_store()));

        let rejected = provider.token().await.unwrap().secret().to_owned();
        let calls: Vec<_> = (0..8)
            .map(|_| {
                let provider = Arc::clone(&provider);
                let rejected = rejected.clone();
                tokio::spawn(async move { provider.token_after_rejection(&rejected).await })
            })
            .collect();
        for call in calls {
            assert_eq!(call.await.unwrap().unwrap().secret(), "ya29.second");
        }
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let server = MockServer::start().await;
        let slow = token("ya29.shared", 3599).set_delay(Duration::from_millis(200));
        mock_refresh(&server, slow, 1).await;
        let provider = Arc::new(provider(&server, signed_in_store()));

        let calls: Vec<_> = (0..8)
            .map(|_| {
                let provider = Arc::clone(&provider);
                tokio::spawn(async move { provider.token().await })
            })
            .collect();
        for call in calls {
            assert_eq!(call.await.unwrap().unwrap().secret(), "ya29.shared");
        }
    }

    #[tokio::test]
    async fn saves_a_rotated_refresh_token() {
        let server = MockServer::start().await;
        let rotated = ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.token",
            "expires_in": 3599,
            "refresh_token": "1//rotated",
            "token_type": "Bearer",
        }));
        mock_refresh(&server, rotated, 1).await;
        let store = signed_in_store();
        let provider = provider(&server, Arc::clone(&store));

        provider.token().await.unwrap();
        assert_eq!(store.load(ACCOUNT).unwrap().as_deref(), Some("1//rotated"));
    }

    #[tokio::test]
    async fn a_revoked_refresh_token_requires_sign_in() {
        let server = MockServer::start().await;
        let revoked = ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant",
            "error_description": "Token has been expired or revoked.",
        }));
        mock_refresh(&server, revoked, 1).await;
        let store = signed_in_store();
        let provider = provider(&server, Arc::clone(&store));

        let error = provider.token().await.unwrap_err();
        assert!(matches!(error, AuthError::InvalidGrant(ref d) if d.contains("revoked")));
        assert!(error.requires_sign_in());
        // The token is left for the caller to remove, so a misconfigured client
        // does not wipe a working sign-in.
        assert_eq!(store.load(ACCOUNT).unwrap().as_deref(), Some(STORED));
    }

    #[tokio::test]
    async fn other_token_endpoint_errors_are_reported() {
        let server = MockServer::start().await;
        let unauthorized = ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_client",
            "error_description": "The OAuth client was not found.",
        }));
        mock_refresh(&server, unauthorized, 1).await;
        let provider = provider(&server, signed_in_store());

        let error = provider.token().await.unwrap_err();
        assert!(matches!(error, AuthError::TokenEndpoint(_)), "{error:?}");
        assert!(!error.requires_sign_in());
    }

    #[tokio::test]
    async fn an_account_without_a_stored_token_is_not_signed_in() {
        let server = MockServer::start().await;
        mock_refresh(&server, token("ya29.token", 3599), 0).await;
        let provider = provider(&server, Arc::new(MemoryTokenStore::new()));

        let error = provider.token().await.unwrap_err();
        assert!(matches!(error, AuthError::NotSignedIn(ref a) if a == ACCOUNT));
        assert!(error.requires_sign_in());
    }

    #[tokio::test]
    async fn from_authorization_stores_the_refresh_token_and_reuses_the_access_token() {
        let server = MockServer::start().await;
        mock_refresh(&server, token("ya29.refreshed", 3599), 0).await;
        let store = Arc::new(MemoryTokenStore::new());
        let authorization = Authorization {
            email: ACCOUNT.to_owned(),
            refresh_token: "1//new".to_owned(),
            access_token: AccessToken::new("ya29.signed-in", Some(Duration::from_secs(3599))),
        };

        let provider = RefreshingTokenProvider::from_authorization(
            client(&server),
            store.clone(),
            authorization,
        )
        .await
        .unwrap();
        assert_eq!(provider.account(), ACCOUNT);
        assert_eq!(store.load(ACCOUNT).unwrap().as_deref(), Some("1//new"));
        assert_eq!(provider.token().await.unwrap().secret(), "ya29.signed-in");
    }

    #[tokio::test]
    async fn providers_work_as_trait_objects() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(token("ya29.first", 3599))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mock_refresh(&server, token("ya29.second", 3599), 1).await;

        let refreshing: Arc<dyn TokenProvider> = Arc::new(provider(&server, signed_in_store()));
        let first = refreshing.access_token().await.unwrap();
        assert_eq!(first, "ya29.first");
        assert_eq!(
            refreshing.refresh_access_token(&first).await.unwrap(),
            "ya29.second"
        );

        let fixed: Arc<dyn TokenProvider> = Arc::new(StaticTokenProvider::new("static"));
        assert_eq!(fixed.access_token().await.unwrap(), "static");
        assert_eq!(
            fixed.refresh_access_token("static").await.unwrap(),
            "static"
        );
    }

    #[tokio::test]
    async fn trait_errors_downcast_to_auth_errors() {
        let server = MockServer::start().await;
        let provider: Arc<dyn TokenProvider> =
            Arc::new(provider(&server, Arc::new(MemoryTokenStore::new())));

        let error = provider.access_token().await.unwrap_err();
        let auth_error = error.downcast_ref::<AuthError>().unwrap();
        assert!(auth_error.requires_sign_in());
    }
}
