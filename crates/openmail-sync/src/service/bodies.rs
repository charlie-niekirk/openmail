//! Message bodies: fetched when asked for, and prefetched for recent mail.

use std::collections::HashMap;
use std::sync::Arc;

use openmail_core::{LabelId, MessageId, SystemLabel};
use openmail_gmail::GmailClient;
use openmail_gmail::api::{self, MAX_BATCH_SIZE, MessageFormat};
use openmail_store::{MessageBody, Store};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{Events, MailEvent};
use crate::initial::{cancellable, parse_body};
use crate::{Error, Result};

/// Bodies per prefetch request: small, so a body the user asks for does not
/// wait long behind a prefetch.
const PREFETCH_CHUNK: usize = 10;

/// Where a requested body goes.
pub(super) type Reply = oneshot::Sender<Result<Option<MessageBody>>>;

/// What the body fetcher is asked to do.
pub(super) enum BodyRequest {
    /// Fetch this message's body (unless it is stored) and answer.
    Fetch { id: MessageId, reply: Reply },
    /// Fetch the bodies of the newest Inbox messages that have none.
    Prefetch,
}

/// Fetches bodies one batch at a time, bodies someone is waiting for first,
/// and stores them.
pub(super) struct BodyFetcher {
    pub(super) client: GmailClient,
    pub(super) store: Store,
    /// How many of the newest Inbox messages to prefetch; 0 turns it off.
    pub(super) prefetch: usize,
    pub(super) events: Events,
    pub(super) requests: mpsc::UnboundedReceiver<BodyRequest>,
}

impl BodyFetcher {
    /// Serves requests until `cancel` fires.
    pub(super) async fn run(mut self, cancel: CancellationToken) {
        let mut waiting = Waiting::default();
        let mut prefetch_due = false;
        loop {
            if waiting.is_empty() && !prefetch_due {
                let request = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    request = self.requests.recv() => request,
                };
                let Some(request) = request else { return };
                self.take(request, &mut waiting, &mut prefetch_due);
            }
            self.take_pending(&mut waiting, &mut prefetch_due);
            let result = if waiting.is_empty() {
                prefetch_due = false;
                self.prefetch(&mut waiting, &cancel).await
            } else {
                self.fetch_waiting(&mut waiting, &cancel).await
            };
            match result {
                Ok(()) => {}
                Err(Error::Cancelled) => return,
                Err(err) => tracing::warn!(%err, "fetching message bodies failed"),
            }
        }
    }

    /// Takes a request: answers a fetch at once if the body is stored, and
    /// otherwise adds it to `waiting`.
    fn take(&self, request: BodyRequest, waiting: &mut Waiting, prefetch_due: &mut bool) {
        match request {
            BodyRequest::Prefetch => *prefetch_due = true,
            BodyRequest::Fetch { id, reply } => match self.store.body(&id) {
                Ok(None) => waiting.add(id, reply),
                stored => {
                    let _ = reply.send(stored.map_err(Error::from));
                }
            },
        }
    }

    /// Takes every request that has arrived, without waiting.
    fn take_pending(&mut self, waiting: &mut Waiting, prefetch_due: &mut bool) {
        while let Ok(request) = self.requests.try_recv() {
            self.take(request, waiting, prefetch_due);
        }
    }

    /// Fetches, stores and hands out every body in `waiting`.
    async fn fetch_waiting(
        &mut self,
        waiting: &mut Waiting,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let mut all = std::mem::take(waiting);
        let ids: Vec<MessageId> = all.0.keys().cloned().collect();
        for chunk in ids.chunks(MAX_BATCH_SIZE) {
            let results = match cancellable(
                cancel,
                self.client.get_messages(chunk, MessageFormat::Raw, &[]),
            )
            .await
            {
                Ok(results) => results,
                Err(err) => {
                    let cancelled = matches!(err, Error::Cancelled);
                    let err = Arc::new(err);
                    for id in chunk {
                        all.answer(id, Err(Error::Shared(err.clone())));
                    }
                    if cancelled {
                        return Err(Error::Cancelled);
                    }
                    continue;
                }
            };
            let fetched_at = now();
            for (id, result) in chunk.iter().zip(results) {
                let answer = match result {
                    Ok(message) => match parse_body(id, message.raw.as_deref(), fetched_at) {
                        Some(body) => self.save(id, &body).map(|()| Some(body)),
                        None => Ok(None),
                    },
                    Err(api::Error::NotFound { .. }) => Ok(None),
                    Err(err) => Err(err.into()),
                };
                all.answer(id, answer);
            }
        }
        Ok(())
    }

    /// Fetches and stores the bodies of the newest Inbox messages that have
    /// none, a few at a time, fetching bodies someone asks for meanwhile
    /// first. Stops quietly if Gmail fails (offline or rate-limited): the
    /// next poll asks again.
    async fn prefetch(&mut self, waiting: &mut Waiting, cancel: &CancellationToken) -> Result<()> {
        if self.prefetch == 0 {
            return Ok(());
        }
        let ids = self
            .store
            .recent_messages_without_body(&LabelId::from(SystemLabel::Inbox), self.prefetch)?;
        let mut fetched = 0;
        for chunk in ids.chunks(PREFETCH_CHUNK) {
            let mut ignored = false;
            self.take_pending(waiting, &mut ignored);
            if !waiting.is_empty() {
                self.fetch_waiting(waiting, cancel).await?;
            }
            // Some may have been fetched for someone meanwhile.
            let mut chunk = chunk.to_vec();
            chunk.retain(|id| matches!(self.store.has_body(id), Ok(false)));
            if chunk.is_empty() {
                continue;
            }
            let results = match cancellable(
                cancel,
                self.client.get_messages(&chunk, MessageFormat::Raw, &[]),
            )
            .await
            {
                Ok(results) => results,
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(err) => {
                    tracing::debug!(%err, "body prefetch stopped");
                    return Ok(());
                }
            };
            let fetched_at = now();
            for (id, result) in chunk.iter().zip(results) {
                match result {
                    Ok(message) => {
                        if let Some(body) = parse_body(id, message.raw.as_deref(), fetched_at) {
                            self.save(id, &body)?;
                            fetched += 1;
                        }
                    }
                    Err(api::Error::NotFound { .. }) => {}
                    Err(err) => {
                        tracing::debug!(%err, message = %id, "body prefetch stopped");
                        return Ok(());
                    }
                }
            }
        }
        if fetched > 0 {
            tracing::debug!(fetched, "prefetched message bodies");
        }
        Ok(())
    }

    /// Stores a body, if its message is stored (it may have been deleted
    /// meanwhile, or never synced), and reports it.
    fn save(&self, id: &MessageId, body: &MessageBody) -> Result<()> {
        let stored = self.store.transaction(|store| {
            if store.message(id)?.is_none() {
                return Ok(false);
            }
            store.upsert_body(id, body)?;
            Ok::<_, openmail_store::Error>(true)
        })?;
        if stored {
            self.events.send(MailEvent::BodyFetched(id.clone()));
        }
        Ok(())
    }
}

/// The time now, to the millisecond as the store keeps it, so a body handed
/// out equals its stored copy.
fn now() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    now.replace_millisecond(now.millisecond()).unwrap_or(now)
}

/// Bodies someone is waiting for, with everyone waiting for each.
#[derive(Default)]
struct Waiting(HashMap<MessageId, Vec<Reply>>);

impl Waiting {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn add(&mut self, id: MessageId, reply: Reply) {
        self.0.entry(id).or_default().push(reply);
    }

    /// Answers everyone waiting for `id`.
    fn answer(&mut self, id: &MessageId, answer: Result<Option<MessageBody>>) {
        let replies = self.0.remove(id).unwrap_or_default();
        match answer {
            Ok(body) => {
                for reply in replies {
                    let _ = reply.send(Ok(body.clone()));
                }
            }
            Err(err) if replies.len() == 1 => {
                let _ = replies.into_iter().next().map(|reply| reply.send(Err(err)));
            }
            Err(err) => {
                let err = Arc::new(err);
                for reply in replies {
                    let _ = reply.send(Err(Error::Shared(err.clone())));
                }
            }
        }
    }
}
