//! Authentication, error mapping and retries, exercised through `users.getProfile`.

mod common;

use std::time::Duration;

use common::{USER, json, setup, setup_with};
use openmail_core::HistoryId;
use openmail_gmail::api::{Error, QuotaKind, RetryPolicy};
use reqwest::StatusCode;
use wiremock::matchers::{bearer_token, method, path};
use wiremock::{Mock, ResponseTemplate};

fn profile_path() -> String {
    format!("{USER}/profile")
}

#[tokio::test]
async fn get_profile_sends_the_access_token() {
    let (server, client, tokens) = setup().await;
    Mock::given(method("GET"))
        .and(path(profile_path()))
        .and(bearer_token("token-0"))
        .respond_with(json(200, "profile.json"))
        .expect(1)
        .mount(&server)
        .await;

    let profile = client.get_profile().await.unwrap();
    assert_eq!(profile.email_address, "ada@example.com");
    assert_eq!(profile.messages_total, 18342);
    assert_eq!(profile.threads_total, 9120);
    assert_eq!(profile.history_id, HistoryId::new(1_029_384));
    assert_eq!(tokens.refreshes(), 0);
}

#[tokio::test]
async fn unauthorized_refreshes_the_token_once_and_retries() {
    let (server, client, tokens) = setup().await;
    Mock::given(path(profile_path()))
        .and(bearer_token("token-0"))
        .respond_with(json(401, "error_401.json"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path(profile_path()))
        .and(bearer_token("token-1"))
        .respond_with(json(200, "profile.json"))
        .expect(1)
        .mount(&server)
        .await;

    client.get_profile().await.unwrap();
    assert_eq!(tokens.refreshes(), 1);
}

#[tokio::test]
async fn a_second_unauthorized_is_an_error() {
    let (server, client, tokens) = setup().await;
    Mock::given(path(profile_path()))
        .respond_with(json(401, "error_401.json"))
        .expect(2)
        .mount(&server)
        .await;

    let err = client.get_profile().await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized), "{err:?}");
    assert_eq!(tokens.refreshes(), 1);
}

#[tokio::test]
async fn server_errors_are_retried() {
    let (server, client, _) = setup().await;
    Mock::given(path(profile_path()))
        .respond_with(json(500, "error_500.json"))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(path(profile_path()))
        .respond_with(json(200, "profile.json"))
        .expect(1)
        .mount(&server)
        .await;

    client.get_profile().await.unwrap();
}

#[tokio::test]
async fn server_errors_surface_after_the_last_retry() {
    let (server, client, _) = setup_with(common::fast_retries(2)).await;
    Mock::given(path(profile_path()))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;

    match client.get_profile().await.unwrap_err() {
        Error::Api { status, .. } => assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn rate_limits_are_retried() {
    let (server, client, _) = setup().await;
    Mock::given(path(profile_path()))
        .respond_with(json(429, "error_429_rate_limit.json"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path(profile_path()))
        .respond_with(json(200, "profile.json"))
        .expect(1)
        .mount(&server)
        .await;

    client.get_profile().await.unwrap();
    assert_eq!(client.stats().retries, 1);
    assert_eq!(client.stats().rate_limited, 1);
}

#[tokio::test]
async fn rate_limits_outlast_max_retries_within_their_wait_budget() {
    let (server, client, _) = setup_with(RetryPolicy {
        max_retries: 1,
        rate_limit_max_wait: Duration::from_secs(1),
        ..common::fast_retries(1)
    })
    .await;
    Mock::given(path(profile_path()))
        .respond_with(json(429, "error_429_rate_limit.json"))
        .up_to_n_times(4)
        .expect(4)
        .mount(&server)
        .await;
    Mock::given(path(profile_path()))
        .respond_with(json(200, "profile.json"))
        .expect(1)
        .mount(&server)
        .await;

    client.get_profile().await.unwrap();
}

#[tokio::test]
async fn rate_limit_retries_stop_when_their_wait_budget_is_spent() {
    let (server, client, _) = setup_with(RetryPolicy {
        max_retries: 10,
        rate_limit_max_wait: Duration::ZERO,
        ..common::fast_retries(10)
    })
    .await;
    Mock::given(path(profile_path()))
        .respond_with(json(429, "error_429_rate_limit.json"))
        .expect(1)
        .mount(&server)
        .await;

    let err = client.get_profile().await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Quota {
                kind: QuotaKind::RateLimit,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn persistent_rate_limits_are_typed_quota_errors() {
    let (server, client, _) = setup_with(RetryPolicy::none()).await;
    Mock::given(path(profile_path()))
        .respond_with(json(429, "error_429_rate_limit.json").insert_header("Retry-After", "7"))
        .expect(1)
        .mount(&server)
        .await;

    match client.get_profile().await.unwrap_err() {
        Error::Quota {
            kind,
            retry_after,
            message,
        } => {
            assert_eq!(kind, QuotaKind::RateLimit);
            assert_eq!(retry_after, Some(Duration::from_secs(7)));
            assert!(message.starts_with("User-rate limit exceeded"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn daily_quota_is_not_retried() {
    let (server, client, _) = setup().await;
    Mock::given(path(profile_path()))
        .respond_with(json(403, "error_403_daily_limit.json"))
        .expect(1)
        .mount(&server)
        .await;

    let err = client.get_profile().await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Quota {
                kind: QuotaKind::Daily,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn client_errors_are_not_retried() {
    let (server, client, _) = setup().await;
    Mock::given(path(profile_path()))
        .respond_with(json(400, "error_400.json"))
        .expect(1)
        .mount(&server)
        .await;

    match client.get_profile().await.unwrap_err() {
        Error::Api {
            status,
            reason,
            message,
        } => {
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(reason.as_deref(), Some("invalidArgument"));
            assert_eq!(message, "Invalid id value");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn token_provider_failures_are_reported() {
    use openmail_gmail::api::{GmailClient, TokenFuture, TokenProvider};
    use std::sync::Arc;

    struct Revoked;
    impl TokenProvider for Revoked {
        fn access_token(&self) -> TokenFuture<'_> {
            Box::pin(async { Err("refresh token revoked".into()) })
        }
        fn refresh_access_token<'a>(&'a self, _: &'a str) -> TokenFuture<'a> {
            self.access_token()
        }
    }

    let client = GmailClient::builder(Arc::new(Revoked))
        .base_url("http://127.0.0.1:9/".parse().unwrap())
        .build()
        .unwrap();
    let err = client.get_profile().await.unwrap_err();
    assert!(matches!(err, Error::Token(_)), "{err:?}");
}

#[tokio::test]
async fn base_url_with_a_path_prefix_is_respected() {
    let server = wiremock::MockServer::start().await;
    let tokens = std::sync::Arc::new(common::TestTokens::default());
    let client = openmail_gmail::GmailClient::builder(tokens)
        .base_url(format!("{}/proxy", server.uri()).parse().unwrap())
        .build()
        .unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/proxy{}", profile_path())))
        .respond_with(json(200, "profile.json"))
        .expect(1)
        .mount(&server)
        .await;

    client.get_profile().await.unwrap();
}

#[test]
fn non_http_base_urls_are_rejected() {
    let tokens = std::sync::Arc::new(common::TestTokens::default());
    let err = openmail_gmail::GmailClient::builder(tokens)
        .base_url("mailto:ada@example.com".parse().unwrap())
        .build()
        .unwrap_err();
    assert!(matches!(err, Error::InvalidConfig(_)), "{err:?}");
}
