//! Incremental sync: apply `history.list` changes to the store.

use std::collections::{BTreeSet, HashSet};

use openmail_core::{HistoryId, Label, LabelId, MessageId, SystemLabel, ThreadId};
use openmail_gmail::GmailClient;
use openmail_gmail::api::{self, History, HistoryRequest, ListHistoryResponse};
use openmail_store::Store;
use tokio_util::sync::CancellationToken;

use crate::initial::{FetchedThreads, cancellable, fetch_threads};
use crate::{Error, InitialSync, Result};

/// History records per `history.list` page (Gmail allows up to 500).
pub const HISTORY_PAGE_SIZE: u32 = 500;

/// What one incremental sync changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// `history.list` pages applied.
    pub pages: u32,
    /// History records applied.
    pub records: u64,
    /// Threads fetched again because messages were added to or deleted from
    /// them (or a stored-nowhere thread was moved to the Inbox), and stored.
    pub threads_fetched: u64,
    /// Threads deleted because they no longer exist.
    pub threads_deleted: u64,
    /// Label additions and removals applied to stored messages.
    pub label_changes: u64,
    /// Whether the label list was synced again, because changes referred to
    /// user labels the store did not know.
    pub labels_synced: bool,
    /// The history ID the store is now at.
    pub history_id: Option<HistoryId>,
    /// Gmail no longer had history back to the stored history ID, so the sync
    /// state was [reset](InitialSync::reset) for a full resync.
    pub full_resync: bool,
}

impl Changes {
    /// Whether anything in the store changed (apart from the history ID).
    pub fn is_empty(&self) -> bool {
        self.threads_fetched == 0
            && self.threads_deleted == 0
            && self.label_changes == 0
            && !self.labels_synced
            && !self.full_resync
    }
}

/// Brings a fully synced store up to date with `history.list`.
///
/// Starting from the stored history ID, each page of history is applied in
/// one transaction, which also advances the stored history ID to the last
/// record applied (to the mailbox's current history ID after the last page),
/// so an interrupted sync continues where it stopped.
///
/// - Threads that gained or lost messages are fetched again (metadata, in
///   batches) and replaced, which keeps their snippet, date, message count and
///   labels right; threads that no longer exist are deleted.
/// - Label changes are applied to stored messages as additions and removals,
///   which the store's triggers roll up into thread labels and the unread and
///   starred flags. Changes to messages that are not stored (outside the
///   synced range) are ignored, except that a message moved to the Inbox has
///   its thread fetched.
/// - If a change refers to a user label the store does not know, the label
///   list is synced first.
///
/// Applying a page twice gives the same result, since threads are replaced
/// with the server's copy and label changes add or remove (never toggle).
#[derive(Debug, Clone)]
pub struct HistorySync {
    client: GmailClient,
    page_size: u32,
}

impl HistorySync {
    /// An incremental sync of the account `client` is signed in to.
    pub fn new(client: GmailClient) -> Self {
        Self {
            client,
            page_size: HISTORY_PAGE_SIZE,
        }
    }

    /// Sets the records per `history.list` page.
    #[must_use]
    pub fn page_size(mut self, records: u32) -> Self {
        self.page_size = records.clamp(1, HISTORY_PAGE_SIZE);
        self
    }

    /// Applies every change since the stored history ID.
    ///
    /// Fails with [`Error::InitialSyncIncomplete`] unless the initial sync has
    /// finished. If Gmail no longer has history that far back, resets the
    /// sync state for a full resync and returns with
    /// [`Changes::full_resync`] set: run [`InitialSync`] next.
    ///
    /// `store` is borrowed mutably only so the future is `Send` (`Store` is
    /// not `Sync`).
    pub async fn run(&self, store: &mut Store, cancel: &CancellationToken) -> Result<Changes> {
        let state = store.sync_state()?;
        let start = match state.history_id {
            Some(id) if state.backfill_complete => id,
            _ => return Err(Error::InitialSyncIncomplete),
        };
        let mut changes = Changes {
            history_id: Some(start),
            ..Changes::default()
        };
        let mut page_token = None;
        loop {
            let mut request = HistoryRequest::new(start).max_results(self.page_size);
            request.page_token = page_token.take();
            let page = match cancellable(cancel, self.client.list_history(&request)).await {
                Ok(page) => page,
                Err(Error::Gmail(api::Error::HistoryExpired { .. })) => {
                    tracing::info!(%start, "Gmail history has expired; starting a full resync");
                    InitialSync::reset(store)?;
                    changes.full_resync = true;
                    changes.history_id = None;
                    return Ok(changes);
                }
                Err(err) => return Err(err),
            };
            let next = page.next_page_token.clone().filter(|t| !t.is_empty());
            self.apply_page(store, cancel, &page, next.is_none(), &mut changes)
                .await?;
            match next {
                Some(token) => page_token = Some(token),
                None => return Ok(changes),
            }
        }
    }

    /// Applies one page in one transaction and advances the history ID.
    async fn apply_page(
        &self,
        store: &mut Store,
        cancel: &CancellationToken,
        page: &ListHistoryResponse,
        last: bool,
        changes: &mut Changes,
    ) -> Result<()> {
        let plan = Plan::new(store, &page.history)?;
        let (threads, gone): FetchedThreads =
            fetch_threads(&self.client, cancel, &plan.refetch).await?;

        let mut referenced: BTreeSet<&LabelId> =
            plan.label_changes.iter().flat_map(|c| &c.labels).collect();
        referenced.extend(
            threads
                .iter()
                .flat_map(|(_, messages)| messages.iter().flat_map(|m| &m.label_ids)),
        );
        // System labels are fixed; only user labels can be new.
        let known: HashSet<LabelId> = store.labels()?.into_iter().map(|l| l.id).collect();
        let unknown = referenced
            .iter()
            .any(|id| id.system_label().is_none() && !known.contains(*id));
        let labels = if unknown {
            tracing::debug!("history refers to unknown labels; syncing labels");
            let labels: Vec<Label> = cancellable(cancel, self.client.list_labels())
                .await?
                .into_iter()
                .map(Label::from)
                .collect();
            Some(labels)
        } else {
            None
        };

        // The last page leaves the store at the mailbox's current history ID;
        // others at their newest record.
        let current = changes.history_id;
        let next = if last {
            Some(page.history_id)
        } else {
            page.history.iter().map(|h| h.id).max()
        };
        let next = next.max(current);

        let refetched: HashSet<&ThreadId> = plan.refetch.iter().collect();
        let (deleted, label_changes) = store.transaction(|store| {
            if let Some(labels) = &labels {
                store.replace_labels(labels)?;
            }
            store.replace_threads(&threads)?;
            let mut deleted = 0;
            for id in &gone {
                deleted += u64::from(store.delete_thread(id)?);
            }
            let mut applied = 0;
            for change in &plan.label_changes {
                // A fetched thread is already up to date.
                if refetched.contains(&change.thread) {
                    continue;
                }
                let (add, remove): (&[LabelId], &[LabelId]) = if change.added {
                    (&change.labels, &[])
                } else {
                    (&[], &change.labels)
                };
                if store.modify_message_labels(&change.message, add, remove)? {
                    applied += 1;
                }
            }
            let mut state = store.sync_state()?;
            state.history_id = next;
            store.set_sync_state(&state)?;
            Ok::<_, Error>((deleted, applied))
        })?;

        changes.pages += 1;
        changes.records += page.history.len() as u64;
        changes.threads_fetched += threads.len() as u64;
        changes.threads_deleted += deleted;
        changes.label_changes += label_changes;
        changes.labels_synced |= labels.is_some();
        changes.history_id = next;
        tracing::debug!(
            records = page.history.len(),
            fetched = threads.len(),
            deleted,
            label_changes,
            history_id = ?next,
            "applied a page of Gmail history"
        );
        Ok(())
    }
}

/// What one page of history asks for.
#[derive(Debug, Default)]
struct Plan {
    /// Threads to fetch again, in first-seen order.
    refetch: Vec<ThreadId>,
    /// Label changes to stored messages, in history order.
    label_changes: Vec<LabelChange>,
}

#[derive(Debug)]
struct LabelChange {
    message: MessageId,
    thread: ThreadId,
    labels: Vec<LabelId>,
    added: bool,
}

impl Plan {
    fn new(store: &Store, history: &[History]) -> Result<Self> {
        let mut plan = Self::default();
        let mut seen = HashSet::new();
        let mut refetch = |thread: &ThreadId| {
            if seen.insert(thread.clone()) {
                plan.refetch.push(thread.clone());
            }
        };
        let inbox = LabelId::from(SystemLabel::Inbox);
        for record in history {
            for added in &record.messages_added {
                refetch(&added.message.thread_id);
            }
            for deleted in &record.messages_deleted {
                // A thread that is not stored has nothing to update.
                if store.thread(&deleted.message.thread_id)?.is_some() {
                    refetch(&deleted.message.thread_id);
                }
            }
            for (changes, added) in [
                (&record.labels_added, true),
                (&record.labels_removed, false),
            ] {
                for change in changes {
                    let message = &change.message;
                    if added
                        && change.label_ids.contains(&inbox)
                        && store.message(&message.id)?.is_none()
                    {
                        // Moved to the Inbox from outside the synced range.
                        refetch(&message.thread_id);
                    }
                    plan.label_changes.push(LabelChange {
                        message: message.id.clone(),
                        thread: message.thread_id.clone(),
                        labels: change.label_ids.clone(),
                        added,
                    });
                }
            }
        }
        Ok(plan)
    }
}
