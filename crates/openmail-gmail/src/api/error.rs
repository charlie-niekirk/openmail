//! Errors from the Gmail API client, and how HTTP failures map onto them.

use std::fmt;
use std::time::Duration;

use openmail_core::HistoryId;
use reqwest::StatusCode;
use serde::Deserialize;

use super::TokenError;

/// A `Result` whose error defaults to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Which Gmail usage limit a request ran into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum QuotaKind {
    /// A short-window rate limit (per user or per project), or too many
    /// concurrent requests. Retrying later succeeds.
    RateLimit,
    /// The project's daily quota is used up. Retrying today will not help.
    Daily,
}

impl fmt::Display for QuotaKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::RateLimit => "rate limit",
            Self::Daily => "daily quota",
        })
    }
}

/// An error from a Gmail API call.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Gmail rejected the access token, and again after one refresh.
    #[error("Gmail rejected the access token, even after a refresh")]
    Unauthorized,
    /// The [`TokenProvider`](super::TokenProvider) could not supply a token.
    #[error("could not get an access token")]
    Token(#[source] TokenError),
    /// The requested resource does not exist (HTTP 404).
    #[error("not found: {message}")]
    NotFound { message: String },
    /// `history.list` was called with a history ID Gmail no longer keeps.
    /// The caller must do a full sync.
    #[error("history ID {start} has expired; a full sync is needed")]
    HistoryExpired { start: HistoryId },
    /// A usage limit was hit, and retries (if allowed) did not get past it.
    #[error("Gmail {kind} exceeded: {message}")]
    Quota {
        kind: QuotaKind,
        message: String,
        /// How long Gmail asked us to wait, from `Retry-After`.
        retry_after: Option<Duration>,
    },
    /// Any other error response from Gmail.
    #[error("Gmail API error {status}: {message}")]
    Api {
        status: StatusCode,
        /// The machine-readable reason, such as `"invalidArgument"`.
        reason: Option<String>,
        message: String,
    },
    /// The request could not be sent or the response could not be read.
    #[error("HTTP request to Gmail failed")]
    Http(#[from] reqwest::Error),
    /// A response body was not the JSON we expected.
    #[error("could not decode the Gmail response")]
    Decode(#[from] serde_json::Error),
    /// A response decoded but lacked data a conversion needs.
    #[error("unexpected Gmail response: {0}")]
    InvalidResponse(String),
    /// A value from Gmail was not a valid domain value.
    #[error(transparent)]
    Domain(#[from] openmail_core::Error),
    /// The client was configured with invalid settings.
    #[error("invalid Gmail client configuration: {0}")]
    InvalidConfig(String),
}

/// An error response, classified for the retry loop.
#[derive(Debug)]
pub(crate) struct Failure {
    /// The error to report if the request is not (or no longer) retried.
    pub(crate) error: Error,
    /// Whether retrying after a backoff may succeed.
    pub(crate) retryable: bool,
}

/// Reasons Gmail gives (in `error.errors[].reason`) for short-window limits.
const RATE_LIMIT_REASONS: &[&str] = &[
    "rateLimitExceeded",
    "userRateLimitExceeded",
    "concurrentLimitExceeded",
];

/// Reasons Gmail gives for an exhausted daily quota.
const DAILY_LIMIT_REASONS: &[&str] = &["dailyLimitExceeded", "quotaExceeded"];

/// Classifies a non-success response (other than a first 401, which the client
/// handles by refreshing the token).
pub(crate) fn classify(status: StatusCode, body: &[u8], retry_after: Option<Duration>) -> Failure {
    let body = ErrorBody::parse(body);
    let reason = body.reason();
    let message = if body.message.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("unknown error")
            .to_owned()
    } else {
        body.message.clone()
    };

    let quota = |kind| Error::Quota {
        kind,
        message: message.clone(),
        retry_after,
    };
    let is = |reasons: &[&str]| reason.as_deref().is_some_and(|r| reasons.contains(&r));

    if is(DAILY_LIMIT_REASONS) && matches!(status.as_u16(), 403 | 429) {
        return Failure {
            error: quota(QuotaKind::Daily),
            retryable: false,
        };
    }
    if status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN && is(RATE_LIMIT_REASONS))
    {
        return Failure {
            error: quota(QuotaKind::RateLimit),
            retryable: true,
        };
    }

    let error = match status {
        StatusCode::UNAUTHORIZED => Error::Unauthorized,
        StatusCode::NOT_FOUND => Error::NotFound { message },
        _ => Error::Api {
            status,
            reason,
            message,
        },
    };
    Failure {
        error,
        retryable: status.is_server_error(),
    }
}

/// Google's JSON error envelope: `{"error": {"code", "message", "errors", "details"}}`.
#[derive(Debug, Default, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: String,
    #[serde(default)]
    errors: Vec<ErrorItem>,
    #[serde(default)]
    details: Vec<ErrorItem>,
}

#[derive(Debug, Deserialize)]
struct ErrorItem {
    reason: Option<String>,
}

impl ErrorBody {
    fn parse(body: &[u8]) -> Self {
        #[derive(Deserialize)]
        struct Envelope {
            error: ErrorBody,
        }
        serde_json::from_slice::<Envelope>(body)
            .map(|e| e.error)
            .unwrap_or_default()
    }

    /// The first reason in `errors`, else the first in `details` (`ErrorInfo`),
    /// whose reasons are `SCREAMING_CASE` versions of the same names.
    fn reason(&self) -> Option<String> {
        if let Some(reason) = self.errors.iter().find_map(|e| e.reason.clone()) {
            return Some(reason);
        }
        self.details
            .iter()
            .find_map(|d| d.reason.as_deref())
            .map(|reason| match reason {
                "RATE_LIMIT_EXCEEDED" => "rateLimitExceeded".to_owned(),
                "RESOURCE_EXHAUSTED" | "QUOTA_EXCEEDED" => "quotaExceeded".to_owned(),
                other => other.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(code: u16, reason: &str) -> Vec<u8> {
        serde_json::json!({
            "error": {
                "code": code,
                "message": "Something happened",
                "errors": [{"domain": "global", "reason": reason, "message": "x"}]
            }
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn rate_limits_are_retryable_quota_errors() {
        let f = classify(
            StatusCode::TOO_MANY_REQUESTS,
            &body(429, "rateLimitExceeded"),
            None,
        );
        assert!(f.retryable);
        assert!(matches!(
            f.error,
            Error::Quota {
                kind: QuotaKind::RateLimit,
                ..
            }
        ));

        let f = classify(
            StatusCode::FORBIDDEN,
            &body(403, "userRateLimitExceeded"),
            None,
        );
        assert!(f.retryable);
        assert!(matches!(
            f.error,
            Error::Quota {
                kind: QuotaKind::RateLimit,
                ..
            }
        ));
    }

    #[test]
    fn daily_quota_is_not_retried() {
        let f = classify(
            StatusCode::FORBIDDEN,
            &body(403, "dailyLimitExceeded"),
            None,
        );
        assert!(!f.retryable);
        assert!(matches!(
            f.error,
            Error::Quota {
                kind: QuotaKind::Daily,
                ..
            }
        ));
    }

    #[test]
    fn other_forbidden_errors_are_api_errors() {
        let f = classify(
            StatusCode::FORBIDDEN,
            &body(403, "insufficientPermissions"),
            None,
        );
        assert!(!f.retryable);
        match f.error {
            Error::Api {
                status,
                reason,
                message,
            } => {
                assert_eq!(status, StatusCode::FORBIDDEN);
                assert_eq!(reason.as_deref(), Some("insufficientPermissions"));
                assert_eq!(message, "Something happened");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn server_errors_are_retryable_and_tolerate_non_json_bodies() {
        let f = classify(StatusCode::BAD_GATEWAY, b"<html>Bad Gateway</html>", None);
        assert!(f.retryable);
        match f.error {
            Error::Api {
                status, message, ..
            } => {
                assert_eq!(status, StatusCode::BAD_GATEWAY);
                assert_eq!(message, "Bad Gateway");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn not_found_is_typed() {
        let f = classify(StatusCode::NOT_FOUND, &body(404, "notFound"), None);
        assert!(!f.retryable);
        assert!(matches!(f.error, Error::NotFound { .. }));
    }

    #[test]
    fn error_info_details_supply_a_reason() {
        let body = serde_json::json!({
            "error": {
                "code": 429,
                "message": "Quota exceeded",
                "status": "RESOURCE_EXHAUSTED",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                    "reason": "RATE_LIMIT_EXCEEDED"
                }]
            }
        });
        let f = classify(
            StatusCode::TOO_MANY_REQUESTS,
            body.to_string().as_bytes(),
            Some(Duration::from_secs(3)),
        );
        assert!(f.retryable);
        match f.error {
            Error::Quota {
                kind, retry_after, ..
            } => {
                assert_eq!(kind, QuotaKind::RateLimit);
                assert_eq!(retry_after, Some(Duration::from_secs(3)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
