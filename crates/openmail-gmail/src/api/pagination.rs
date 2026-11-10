//! Following `nextPageToken` across list calls as a stream of pages.

use std::future::Future;

use futures_util::Stream;
use futures_util::stream::try_unfold;

use super::{
    GmailClient, HistoryRequest, ListHistoryResponse, ListMessagesResponse, ListRequest,
    ListThreadsResponse, Result,
};

/// A list response that may continue on another page.
pub trait Paginated {
    /// The token for the next page, or `None` on the last page.
    fn next_page_token(&self) -> Option<&str>;
}

impl Paginated for ListThreadsResponse {
    fn next_page_token(&self) -> Option<&str> {
        self.next_page_token.as_deref()
    }
}

impl Paginated for ListMessagesResponse {
    fn next_page_token(&self) -> Option<&str> {
        self.next_page_token.as_deref()
    }
}

impl Paginated for ListHistoryResponse {
    fn next_page_token(&self) -> Option<&str> {
        self.next_page_token.as_deref()
    }
}

/// Streams pages by calling `fetch` with each page token, starting from
/// `page_token` (`None` for the first page), until a page has no next token.
///
/// The stream ends after yielding the first error. Pages are fetched lazily, one
/// at a time, as the stream is polled.
pub fn paginate<P, F, Fut>(page_token: Option<String>, fetch: F) -> impl Stream<Item = Result<P>>
where
    P: Paginated,
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<P>>,
{
    // `None` once the last page has been yielded.
    let state = Some(page_token);
    try_unfold((fetch, state), |(mut fetch, state)| async move {
        let Some(token) = state else {
            return Ok(None);
        };
        let page = fetch(token).await?;
        let next = page
            .next_page_token()
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        Ok(Some((page, (fetch, next.map(Some)))))
    })
}

impl GmailClient {
    /// Every page of `threads.list` for `request`, starting at
    /// `request.page_token`.
    pub fn thread_pages(
        &self,
        request: ListRequest,
    ) -> impl Stream<Item = Result<ListThreadsResponse>> + Send + 'static {
        let client = self.clone();
        paginate(request.page_token.clone(), move |page_token| {
            let client = client.clone();
            let request = ListRequest {
                page_token,
                ..request.clone()
            };
            async move { client.list_threads(&request).await }
        })
    }

    /// Every page of `messages.list` for `request`, starting at
    /// `request.page_token`.
    pub fn message_pages(
        &self,
        request: ListRequest,
    ) -> impl Stream<Item = Result<ListMessagesResponse>> + Send + 'static {
        let client = self.clone();
        paginate(request.page_token.clone(), move |page_token| {
            let client = client.clone();
            let request = ListRequest {
                page_token,
                ..request.clone()
            };
            async move { client.list_messages(&request).await }
        })
    }

    /// Every page of `history.list` for `request`, starting at
    /// `request.page_token`.
    ///
    /// Store the last page's `history_id` only after applying every page, so an
    /// interrupted sync starts again from the old one.
    pub fn history_pages(
        &self,
        request: HistoryRequest,
    ) -> impl Stream<Item = Result<ListHistoryResponse>> + Send + 'static {
        let client = self.clone();
        paginate(request.page_token.clone(), move |page_token| {
            let client = client.clone();
            let request = HistoryRequest {
                page_token,
                ..request.clone()
            };
            async move { client.list_history(&request).await }
        })
    }
}
