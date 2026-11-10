//! A typed client for the Gmail REST API.
//!
//! [`GmailClient`] wraps `reqwest` and returns serde models that mirror Gmail's
//! JSON resources. Those convert into `openmail-core` domain types.

mod batch;
mod client;
mod convert;
mod endpoints;
mod error;
mod models;
mod mutations;
mod pagination;
mod retry;

pub use crate::auth::{TokenError, TokenFuture, TokenProvider};
pub use batch::MAX_BATCH_SIZE;
pub use client::{GmailClient, GmailClientBuilder, default_http_client};
pub use error::{Error, QuotaKind, Result};
pub use models::{
    ApiLabel, ApiLabelColor, ApiMessage, ApiThread, Header, History, HistoryLabelChange,
    HistoryMessage, HistoryRequest, HistoryType, LabelRequest, ListHistoryResponse,
    ListLabelsResponse, ListMessagesResponse, ListRequest, ListThreadsResponse, METADATA_HEADERS,
    MessageFormat, MessagePart, MessagePartBody, ModifyLabels, Profile, ThreadFormat,
};
pub use mutations::MAX_BATCH_MODIFY_IDS;
pub use pagination::{Paginated, paginate};
pub use retry::RetryPolicy;
