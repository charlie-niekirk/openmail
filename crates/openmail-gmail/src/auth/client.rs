//! Signing in with Google's installed-app flow (PKCE and a loopback redirect).

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use oauth2::basic::{
    BasicErrorResponse, BasicErrorResponseType, BasicRevocationErrorResponse,
    BasicTokenIntrospectionResponse, BasicTokenType,
};
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet,
    EndpointSet, ExtraTokenFields, HttpRequest, HttpResponse, PkceCodeChallenge, RedirectUrl,
    RefreshToken, RequestTokenError, Scope, StandardRevocableToken, StandardTokenResponse,
    TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use url::Url;

use super::loopback::LoopbackServer;
use super::{AccessToken, AuthError, OAuthConfig, SCOPES};

/// How long a token request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Fields Google adds to the standard token response.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct GoogleTokenFields {
    /// Present when the `openid` scope was granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
}

impl ExtraTokenFields for GoogleTokenFields {}

type GoogleTokenResponse = StandardTokenResponse<GoogleTokenFields, BasicTokenType>;

/// An `oauth2` client with the authorization and token endpoints set.
type GoogleClient = oauth2::Client<
    BasicErrorResponse,
    GoogleTokenResponse,
    BasicTokenIntrospectionResponse,
    StandardRevocableToken,
    BasicRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointSet,
>;

/// The result of a successful sign-in.
#[derive(Clone)]
pub struct Authorization {
    /// The signed-in account's email address.
    pub email: String,
    /// The long-lived refresh token, to keep in a token store.
    pub refresh_token: String,
    /// An access token that is valid now.
    pub access_token: AccessToken,
}

impl fmt::Debug for Authorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authorization")
            .field("email", &self.email)
            .field("refresh_token", &"<redacted>")
            .field("access_token", &self.access_token)
            .finish()
    }
}

/// A refreshed access token, and the new refresh token if Google rotated it.
pub(crate) struct Refreshed {
    pub(crate) access_token: AccessToken,
    pub(crate) refresh_token: Option<String>,
}

/// An OAuth client for one Google OAuth client ID: signs users in and refreshes
/// their access tokens.
#[derive(Clone)]
pub struct OAuthClient {
    oauth: GoogleClient,
    http: reqwest::Client,
}

impl OAuthClient {
    /// Builds a client from `config`.
    ///
    /// # Errors
    ///
    /// [`AuthError::InvalidEndpoint`] if an endpoint URL is invalid, or
    /// [`AuthError::Http`] if the HTTP client cannot be created.
    pub fn new(config: &OAuthConfig) -> Result<Self, AuthError> {
        let invalid = |url: &str| {
            let url = url.to_owned();
            move |source| AuthError::InvalidEndpoint { url, source }
        };
        let auth_url = AuthUrl::new(config.auth_url.clone()).map_err(invalid(&config.auth_url))?;
        let token_url =
            TokenUrl::new(config.token_url.clone()).map_err(invalid(&config.token_url))?;

        let mut oauth = oauth2::Client::new(ClientId::new(config.client_id.clone()))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            // Google documents client credentials in the form body.
            .set_auth_type(AuthType::RequestBody);
        if let Some(secret) = &config.client_secret {
            oauth = oauth.set_client_secret(ClientSecret::new(secret.clone()));
        }

        // reqwest is built without a rustls crypto provider; use ring, which the
        // rest of the tree already links. An error means one is already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();

        // The token endpoint never redirects; following one would be a way to leak
        // the authorization code or refresh token.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()?;

        Ok(Self { oauth, http })
    }

    /// Signs in through the system browser.
    ///
    /// Opens Google's consent page, waits for the redirect, and exchanges the code
    /// for tokens. This waits as long as the user takes; wrap it in a timeout or
    /// cancel it to give up.
    ///
    /// # Errors
    ///
    /// See [`sign_in_with`](Self::sign_in_with).
    pub async fn sign_in(&self) -> Result<Authorization, AuthError> {
        self.sign_in_with(|url| open::that_detached(url.as_str()))
            .await
    }

    /// Signs in, calling `open_browser` with the consent page URL.
    ///
    /// Use this to also print the URL, for when no browser can be opened.
    ///
    /// # Errors
    ///
    /// [`AuthError::Browser`] if `open_browser` fails; the redirect errors from
    /// the loopback server ([`AuthError::StateMismatch`],
    /// [`AuthError::AuthorizationDenied`], [`AuthError::InvalidRedirect`]);
    /// token endpoint errors; [`AuthError::MissingRefreshToken`]; and
    /// [`AuthError::InvalidIdToken`] if the account email cannot be read.
    pub async fn sign_in_with(
        &self,
        open_browser: impl FnOnce(&Url) -> io::Result<()>,
    ) -> Result<Authorization, AuthError> {
        let server = LoopbackServer::bind().await?;
        let redirect = RedirectUrl::new(server.redirect_uri().to_owned()).map_err(|source| {
            AuthError::InvalidEndpoint {
                url: server.redirect_uri().to_owned(),
                source,
            }
        })?;

        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state) = self
            .oauth
            .authorize_url(CsrfToken::new_random)
            .add_scopes(SCOPES.iter().map(|s| Scope::new((*s).to_owned())))
            .set_pkce_challenge(challenge)
            .set_redirect_uri(Cow::Borrowed(&redirect))
            // Ask for a refresh token, and for consent every time so Google issues
            // one even if this client was authorized before.
            .add_extra_param("access_type", "offline")
            .add_extra_param("prompt", "consent")
            .url();

        open_browser(&url).map_err(AuthError::Browser)?;
        let code = server.receive(state.secret()).await?;

        let http = self.http_client();
        let response = self
            .oauth
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(verifier)
            .set_redirect_uri(Cow::Borrowed(&redirect))
            .request_async(&http)
            .await
            .map_err(token_error)?;

        let id_token =
            response.extra_fields().id_token.as_deref().ok_or_else(|| {
                AuthError::InvalidIdToken("no ID token in the response".to_owned())
            })?;
        let email = email_from_id_token(id_token)?;
        let refresh_token = response
            .refresh_token()
            .ok_or(AuthError::MissingRefreshToken)?
            .secret()
            .clone();

        Ok(Authorization {
            email,
            refresh_token,
            access_token: access_token(&response),
        })
    }

    /// Exchanges a refresh token for a new access token.
    ///
    /// # Errors
    ///
    /// [`AuthError::InvalidGrant`] if the refresh token was revoked or expired,
    /// otherwise a token endpoint error.
    pub(crate) async fn refresh(&self, refresh_token: &str) -> Result<Refreshed, AuthError> {
        let refresh_token = RefreshToken::new(refresh_token.to_owned());
        let http = self.http_client();
        let response = self
            .oauth
            .exchange_refresh_token(&refresh_token)
            .request_async(&http)
            .await
            .map_err(token_error)?;
        Ok(Refreshed {
            access_token: access_token(&response),
            refresh_token: response.refresh_token().map(|t| t.secret().clone()),
        })
    }

    /// Adapts the `reqwest` client to the `oauth2` crate's HTTP interface.
    fn http_client(
        &self,
    ) -> impl Fn(
        HttpRequest,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<HttpResponse, reqwest::Error>> + Send>,
    > + use<> {
        let http = self.http.clone();
        move |request| Box::pin(execute(http.clone(), request))
    }
}

impl fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClient")
            .field("client_id", self.oauth.client_id())
            .finish_non_exhaustive()
    }
}

async fn execute(
    http: reqwest::Client,
    request: HttpRequest,
) -> Result<HttpResponse, reqwest::Error> {
    let (parts, body) = request.into_parts();
    let response = http
        .request(parts.method, parts.uri.to_string())
        .headers(parts.headers)
        .body(body)
        .send()
        .await?;

    let status = response.status();
    let headers = response.headers().clone();
    let body = response.bytes().await?.to_vec();

    let mut converted = HttpResponse::new(body);
    *converted.status_mut() = status;
    *converted.headers_mut() = headers;
    Ok(converted)
}

fn access_token(response: &GoogleTokenResponse) -> AccessToken {
    AccessToken::new(
        response.access_token().secret().clone(),
        response.expires_in(),
    )
}

fn token_error(error: RequestTokenError<reqwest::Error, BasicErrorResponse>) -> AuthError {
    match error {
        RequestTokenError::ServerResponse(response)
            if *response.error() == BasicErrorResponseType::InvalidGrant =>
        {
            AuthError::InvalidGrant(
                response
                    .error_description()
                    .cloned()
                    .unwrap_or_else(|| "invalid_grant".to_owned()),
            )
        }
        RequestTokenError::ServerResponse(response) => {
            AuthError::TokenEndpoint(response.to_string())
        }
        RequestTokenError::Request(error) => AuthError::Http(error),
        RequestTokenError::Parse(error, _) => {
            AuthError::TokenEndpoint(format!("unexpected response: {error}"))
        }
        RequestTokenError::Other(message) => AuthError::TokenEndpoint(message),
    }
}

/// Reads the verified email address from an OpenID Connect ID token.
///
/// The signature is not checked: the token came straight from Google's token
/// endpoint over TLS, which Google's OpenID Connect guide says is sufficient.
fn email_from_id_token(id_token: &str) -> Result<String, AuthError> {
    #[derive(Deserialize)]
    struct Claims {
        email: Option<String>,
        email_verified: Option<bool>,
    }

    let invalid = |reason: &str| AuthError::InvalidIdToken(reason.to_owned());
    let mut segments = id_token.split('.');
    let (Some(_), Some(payload), Some(_), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(invalid("not a JWT"));
    };
    let json = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| invalid("payload is not base64url"))?;
    let claims: Claims =
        serde_json::from_slice(&json).map_err(|_| invalid("payload is not a JSON claim set"))?;

    if claims.email_verified == Some(false) {
        return Err(invalid("email address is not verified"));
    }
    claims
        .email
        .filter(|email| !email.is_empty())
        .ok_or_else(|| invalid("no email claim; was the `email` scope granted?"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const EMAIL: &str = "someone@example.com";

    fn id_token(claims: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("{header}.{payload}.signature")
    }

    fn token_response() -> serde_json::Value {
        json!({
            "access_token": "ya29.access",
            "expires_in": 3599,
            "refresh_token": "1//refresh",
            "scope": SCOPES.join(" "),
            "token_type": "Bearer",
            "id_token": id_token(&json!({ "email": EMAIL, "email_verified": true })),
        })
    }

    fn config(server: &MockServer) -> OAuthConfig {
        OAuthConfig::new("client-id.apps.googleusercontent.com")
            .with_client_secret("client-secret")
            .with_endpoints(
                format!("{}/auth", server.uri()),
                format!("{}/token", server.uri()),
            )
    }

    fn query(url: &Url) -> HashMap<String, String> {
        url.query_pairs().into_owned().collect()
    }

    fn form(body: &[u8]) -> HashMap<String, String> {
        url::form_urlencoded::parse(body).into_owned().collect()
    }

    /// Plays the browser: follows the consent URL straight to the redirect,
    /// with `state` replaced if `forged_state` is set. Records the consent URL.
    fn browser(
        seen: Arc<Mutex<Option<Url>>>,
        forged_state: Option<&'static str>,
    ) -> impl FnOnce(&Url) -> io::Result<()> {
        move |url| {
            *seen.lock().unwrap() = Some(url.clone());
            let params = query(url);
            let state = forged_state.unwrap_or(&params["state"]).to_owned();
            let mut redirect = Url::parse(&params["redirect_uri"]).unwrap();
            redirect
                .query_pairs_mut()
                .append_pair("state", &state)
                .append_pair("code", "4/the-code");
            tokio::spawn(async move { reqwest::get(redirect).await });
            Ok(())
        }
    }

    #[tokio::test]
    async fn sign_in_exchanges_the_code_with_pkce() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = OAuthClient::new(&config(&server)).unwrap();
        let seen = Arc::new(Mutex::new(None));
        let auth = client
            .sign_in_with(browser(seen.clone(), None))
            .await
            .unwrap();

        assert_eq!(auth.email, EMAIL);
        assert_eq!(auth.refresh_token, "1//refresh");
        assert_eq!(auth.access_token.secret(), "ya29.access");
        assert!(!auth.access_token.expires_within(Duration::from_secs(3000)));

        // The consent URL asks for the right things.
        let consent_url = seen.lock().unwrap().clone().unwrap();
        assert!(
            consent_url
                .as_str()
                .starts_with(&format!("{}/auth?", server.uri()))
        );
        let consent = query(&consent_url);
        assert_eq!(consent["response_type"], "code");
        assert_eq!(consent["client_id"], "client-id.apps.googleusercontent.com");
        assert_eq!(consent["scope"], SCOPES.join(" "));
        assert_eq!(consent["code_challenge_method"], "S256");
        assert_eq!(consent["access_type"], "offline");
        assert_eq!(consent["prompt"], "consent");
        assert!(consent["redirect_uri"].starts_with("http://127.0.0.1:"));
        assert!(consent["state"].len() >= 16);

        // The token request proves possession of the PKCE verifier.
        let requests = server.received_requests().await.unwrap();
        let token_request = form(&requests[0].body);
        assert_eq!(token_request["code"], "4/the-code");
        assert_eq!(token_request["redirect_uri"], consent["redirect_uri"]);
        assert_eq!(
            token_request["client_id"],
            "client-id.apps.googleusercontent.com"
        );
        assert_eq!(token_request["client_secret"], "client-secret");
        let verifier = &token_request["code_verifier"];
        assert!(verifier.len() >= 43);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, consent["code_challenge"]);
    }

    #[tokio::test]
    async fn each_sign_in_uses_fresh_state_and_verifier() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
            .mount(&server)
            .await;
        let client = OAuthClient::new(&config(&server)).unwrap();

        let (first, second) = (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)));
        client
            .sign_in_with(browser(first.clone(), None))
            .await
            .unwrap();
        client
            .sign_in_with(browser(second.clone(), None))
            .await
            .unwrap();

        let first = query(first.lock().unwrap().as_ref().unwrap());
        let second = query(second.lock().unwrap().as_ref().unwrap());
        assert_ne!(first["state"], second["state"]);
        assert_ne!(first["code_challenge"], second["code_challenge"]);
    }

    #[tokio::test]
    async fn a_forged_redirect_never_reaches_the_token_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
            .expect(0)
            .mount(&server)
            .await;

        let client = OAuthClient::new(&config(&server)).unwrap();
        let seen = Arc::new(Mutex::new(None));
        let result = client.sign_in_with(browser(seen, Some("forged"))).await;
        assert!(
            matches!(result, Err(AuthError::StateMismatch)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_client_without_a_secret_sends_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
            .mount(&server)
            .await;

        let mut config = config(&server);
        config.client_secret = None;
        let client = OAuthClient::new(&config).unwrap();
        client
            .sign_in_with(browser(Arc::new(Mutex::new(None)), None))
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let token_request = form(&requests[0].body);
        assert_eq!(
            token_request["client_id"],
            "client-id.apps.googleusercontent.com"
        );
        assert!(!token_request.contains_key("client_secret"));
    }

    #[tokio::test]
    async fn a_missing_refresh_token_is_an_error() {
        let server = MockServer::start().await;
        let mut response = token_response();
        response.as_object_mut().unwrap().remove("refresh_token");
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;

        let client = OAuthClient::new(&config(&server)).unwrap();
        let result = client
            .sign_in_with(browser(Arc::new(Mutex::new(None)), None))
            .await;
        assert!(
            matches!(result, Err(AuthError::MissingRefreshToken)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn an_expired_code_is_an_invalid_grant() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
                "error_description": "Bad Request",
            })))
            .mount(&server)
            .await;

        let client = OAuthClient::new(&config(&server)).unwrap();
        let result = client
            .sign_in_with(browser(Arc::new(Mutex::new(None)), None))
            .await;
        match result {
            Err(error @ AuthError::InvalidGrant(_)) => assert!(error.requires_sign_in()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_browser_that_fails_to_open_is_reported() {
        let server = MockServer::start().await;
        let client = OAuthClient::new(&config(&server)).unwrap();
        let result = client
            .sign_in_with(|_| Err(io::Error::other("no browser")))
            .await;
        assert!(matches!(result, Err(AuthError::Browser(_))), "{result:?}");
    }

    #[test]
    fn rejects_invalid_endpoints() {
        let config =
            OAuthConfig::new("id").with_endpoints("not a url", "https://example.com/token");
        assert!(matches!(
            OAuthClient::new(&config),
            Err(AuthError::InvalidEndpoint { url, .. }) if url == "not a url"
        ));
    }

    #[test]
    fn reads_the_email_from_the_id_token() {
        let token = id_token(&json!({ "email": EMAIL, "email_verified": true, "sub": "1" }));
        assert_eq!(email_from_id_token(&token).unwrap(), EMAIL);
        // Padding is tolerated.
        let padded = token.replacen(".signature", "==.signature", 1);
        assert_eq!(email_from_id_token(&padded).unwrap(), EMAIL);
    }

    #[test]
    fn rejects_unusable_id_tokens() {
        for token in [
            "not-a-jwt".to_owned(),
            "a.b".to_owned(),
            "a.!!!.c".to_owned(),
            id_token(&json!({ "sub": "1" })),
            id_token(&json!({ "email": "", "email_verified": true })),
            id_token(&json!({ "email": EMAIL, "email_verified": false })),
        ] {
            assert!(
                matches!(
                    email_from_id_token(&token),
                    Err(AuthError::InvalidIdToken(_))
                ),
                "{token}"
            );
        }
    }

    #[test]
    fn debug_redacts_tokens() {
        let auth = Authorization {
            email: EMAIL.to_owned(),
            refresh_token: "1//refresh".to_owned(),
            access_token: AccessToken::new("ya29.access", None),
        };
        let debug = format!("{auth:?}");
        assert!(!debug.contains("1//refresh") && !debug.contains("ya29"));
    }
}
