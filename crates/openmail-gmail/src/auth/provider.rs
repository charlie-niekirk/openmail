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

/// The future returned by [`TokenProvider::access_token`].
pub type TokenFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AccessToken, AuthError>> + Send + 'a>>;

/// Supplies access tokens to the Gmail REST client.
///
/// The trait is object safe, so a client can hold an `Arc<dyn TokenProvider>`.
pub trait TokenProvider: Send + Sync {
    /// Returns an access token that is valid now, refreshing it first if needed.
    fn access_token(&self) -> TokenFuture<'_>;

    /// Reports that the API rejected `rejected` (HTTP 401), so the next call to
    /// [`access_token`](Self::access_token) fetches a new one. Has no effect if a
    /// newer token has already replaced it.
    fn invalidate(&self, rejected: &AccessToken);
}

/// A provider that always returns the same token. Useful in tests.
#[derive(Debug, Clone)]
pub struct StaticTokenProvider {
    token: AccessToken,
}

impl StaticTokenProvider {
    /// A provider returning `secret`, which never expires.
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            token: AccessToken::new(secret, None),
        }
    }
}

impl TokenProvider for StaticTokenProvider {
    fn access_token(&self) -> TokenFuture<'_> {
        Box::pin(ready(Ok(self.token.clone())))
    }

    fn invalidate(&self, _rejected: &AccessToken) {}
}

/// Keeps one account's access token fresh using its stored refresh token.
///
/// The access token is cached in memory and refreshed when it is within
/// [`REFRESH_MARGIN`] of expiring or after [`invalidate`](TokenProvider::invalidate).
/// Concurrent callers share a single refresh. The refresh token is read from the
/// [`TokenStore`] on each refresh, and replaced there if Google rotates it.
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

    async fn current(&self) -> Result<AccessToken, AuthError> {
        if let Some(token) = self.cached_fresh() {
            return Ok(token);
        }
        let _refreshing = self.refreshing.lock().await;
        // Another caller may have refreshed while this one waited.
        if let Some(token) = self.cached_fresh() {
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

    fn cached_fresh(&self) -> Option<AccessToken> {
        self.lock_cached()
            .as_ref()
            .filter(|token| !token.expires_within(REFRESH_MARGIN))
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
        Box::pin(self.current())
    }

    fn invalidate(&self, rejected: &AccessToken) {
        let mut cached = self.lock_cached();
        if cached
            .as_ref()
            .is_some_and(|token| token.secret() == rejected.secret())
        {
            *cached = None;
        }
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

        let first = provider.access_token().await.unwrap();
        let second = provider.access_token().await.unwrap();
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

        provider.access_token().await.unwrap();
        provider.access_token().await.unwrap();
    }

    #[tokio::test]
    async fn invalidate_forces_a_refresh() {
        let server = MockServer::start().await;
        mock_refresh(&server, token("ya29.token", 3599), 2).await;
        let provider = provider(&server, signed_in_store());

        let rejected = provider.access_token().await.unwrap();
        provider.invalidate(&rejected);
        provider.access_token().await.unwrap();
    }

    #[tokio::test]
    async fn invalidating_an_old_token_keeps_the_current_one() {
        let server = MockServer::start().await;
        mock_refresh(&server, token("ya29.current", 3599), 1).await;
        let provider = provider(&server, signed_in_store());

        provider.access_token().await.unwrap();
        provider.invalidate(&AccessToken::new("ya29.old", None));
        assert_eq!(
            provider.access_token().await.unwrap().secret(),
            "ya29.current"
        );
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
                tokio::spawn(async move { provider.access_token().await })
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

        provider.access_token().await.unwrap();
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

        let error = provider.access_token().await.unwrap_err();
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

        let error = provider.access_token().await.unwrap_err();
        assert!(matches!(error, AuthError::TokenEndpoint(_)), "{error:?}");
        assert!(!error.requires_sign_in());
    }

    #[tokio::test]
    async fn an_account_without_a_stored_token_is_not_signed_in() {
        let server = MockServer::start().await;
        mock_refresh(&server, token("ya29.token", 3599), 0).await;
        let provider = provider(&server, Arc::new(MemoryTokenStore::new()));

        let error = provider.access_token().await.unwrap_err();
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
        assert_eq!(
            provider.access_token().await.unwrap().secret(),
            "ya29.signed-in"
        );
    }

    #[tokio::test]
    async fn providers_work_as_trait_objects() {
        let providers: Vec<Arc<dyn TokenProvider>> =
            vec![Arc::new(StaticTokenProvider::new("static"))];
        for provider in providers {
            let token = provider.access_token().await.unwrap();
            assert_eq!(token.secret(), "static");
            provider.invalidate(&token);
            assert_eq!(provider.access_token().await.unwrap().secret(), "static");
        }
    }
}
