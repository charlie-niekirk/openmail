//! `AppState`: what the window shows about the account and its sync, as a plain
//! view-model with no GPUI types, so it can be tested without a window.
//!
//! The [bridge](crate::bridge) feeds it the [`MailEvent`]s a [`MailService`]
//! sends and turns the [`Changed`] it reports into GPUI events and re-renders.
//!
//! [`MailService`]: openmail_sync::MailService

use std::time::Duration;

use openmail_core::MessageId;
use openmail_sync::{Connectivity, MailEvent, Phase, Progress, SyncStatus};

/// Whether there is a mailbox to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    /// The mail service is being started.
    Starting,
    /// There is no signed-in account to open; `reason` says what is missing.
    /// Onboarding (PR 3.2) replaces this with a sign-in screen.
    SignedOut { reason: String },
    /// The mail service is running for `account`.
    Ready { account: String },
    /// The mail service could not be started, or stopped unexpectedly.
    Failed { error: String },
}

/// Where syncing with Gmail is, as last reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    /// Nothing reported yet.
    Waiting,
    /// The initial sync (or a full resync) is running.
    Initial(Progress),
    /// The last poll finished; the store is up to date.
    UpToDate,
    /// The last attempt failed; the next one starts in `retry_in`.
    Retrying { error: String, retry_in: Duration },
    /// Syncing stopped until the user acts (sign in again, quota, store).
    Stopped { error: String },
}

/// What a batch of events changed, so the bridge knows whom to tell.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changed {
    /// The session, sync state or connectivity changed: re-render the frame.
    pub status: bool,
    /// Thread lists, label counts and open threads may be out of date.
    pub threads: bool,
    /// The label list may be out of date.
    pub labels: bool,
    /// Bodies fetched and stored, in the order they arrived.
    pub bodies: Vec<MessageId>,
}

impl Changed {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The app's state, outside any single view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    session: Session,
    sync: SyncState,
    online: bool,
    ops_stopped: Option<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    /// The state while the mail service starts.
    pub fn new() -> Self {
        Self {
            session: Session::Starting,
            sync: SyncState::Waiting,
            online: true,
            ops_stopped: None,
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn sync(&self) -> &SyncState {
        &self.sync
    }

    /// Whether Gmail could be reached on the last request.
    pub fn is_online(&self) -> bool {
        self.online
    }

    /// Why sending changes to Gmail stopped, if it did.
    pub fn ops_stopped(&self) -> Option<&str> {
        self.ops_stopped.as_deref()
    }

    /// Replaces the session, resetting what was known about the last one.
    pub fn set_session(&mut self, session: Session) -> Changed {
        if session == self.session {
            return Changed::default();
        }
        let ready = matches!(session, Session::Ready { .. });
        *self = Self {
            session,
            ..Self::new()
        };
        Changed {
            status: true,
            // A new mailbox: everything shown is out of date.
            threads: ready,
            labels: ready,
            bodies: Vec::new(),
        }
    }

    /// The event stream ended while the session was running.
    pub fn service_ended(&mut self) -> Changed {
        if matches!(self.session, Session::Ready { .. }) {
            self.set_session(Session::Failed {
                error: "The mail service stopped.".to_owned(),
            })
        } else {
            Changed::default()
        }
    }

    /// Applies one event.
    pub fn apply(&mut self, event: MailEvent) -> Changed {
        let mut changed = Changed::default();
        self.apply_into(event, &mut changed);
        changed
    }

    /// Applies a batch of events (all that were waiting), reporting what
    /// changed once.
    pub fn apply_all(&mut self, events: impl IntoIterator<Item = MailEvent>) -> Changed {
        let mut changed = Changed::default();
        for event in events {
            self.apply_into(event, &mut changed);
        }
        changed
    }

    fn apply_into(&mut self, event: MailEvent, changed: &mut Changed) {
        match event {
            MailEvent::SyncStatus(status) => {
                let sync = match status {
                    SyncStatus::Initial(progress) => SyncState::Initial(progress),
                    SyncStatus::UpToDate(_) => SyncState::UpToDate,
                    SyncStatus::Retrying { error, retry_in } => SyncState::Retrying {
                        error: error.to_string(),
                        retry_in,
                    },
                    SyncStatus::Stopped(error) => SyncState::Stopped {
                        error: error.to_string(),
                    },
                    // A status this version does not know: keep the last one.
                    _ => return,
                };
                changed.status |= sync != self.sync;
                self.sync = sync;
            }
            MailEvent::Connectivity(connectivity) => {
                let online = connectivity == Connectivity::Online;
                changed.status |= online != self.online;
                self.online = online;
            }
            MailEvent::ThreadsChanged => changed.threads = true,
            MailEvent::LabelsChanged => changed.labels = true,
            MailEvent::BodyFetched(id) => changed.bodies.push(id),
            MailEvent::OpsStopped(error) => {
                self.ops_stopped = Some(error.to_string());
                changed.status = true;
            }
            // Op results drive undo and error toasts, which come with triage
            // actions (Phase 5); rollbacks also send `ThreadsChanged`.
            MailEvent::Op(_) => {}
            _ => {}
        }
    }

    /// A short line for the title bar about syncing, or `None` when there is
    /// nothing to say.
    pub fn status_text(&self) -> Option<String> {
        match &self.session {
            Session::Starting => return Some("Opening mailbox…".to_owned()),
            Session::SignedOut { .. } | Session::Failed { .. } => return None,
            Session::Ready { .. } => {}
        }
        if !self.online {
            return Some("Offline".to_owned());
        }
        if self.ops_stopped.is_some() {
            return Some("Not sending changes".to_owned());
        }
        Some(match &self.sync {
            SyncState::Waiting => "Connecting…".to_owned(),
            SyncState::Initial(progress) => match progress.phase {
                Phase::Labels => "Syncing labels…".to_owned(),
                Phase::Done => "Up to date".to_owned(),
                Phase::Inbox | Phase::Prefetch | Phase::Backfill => {
                    format!("Syncing… {} threads", group_thousands(progress.threads))
                }
            },
            SyncState::UpToDate => "Up to date".to_owned(),
            SyncState::Retrying { retry_in, .. } => {
                format!("Retrying in {}", short_duration(*retry_in))
            }
            SyncState::Stopped { .. } => "Sync stopped".to_owned(),
        })
    }
}

/// `1234567` as `1,234,567`.
fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A wait as whole seconds under two minutes, else whole minutes (rounded up).
fn short_duration(d: Duration) -> String {
    let secs = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    if secs < 120 {
        format!("{secs}s")
    } else {
        format!("{}m", secs.div_ceil(60))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use openmail_sync::{Changes, Error};

    use super::*;

    fn ready() -> AppState {
        let mut state = AppState::new();
        state.set_session(Session::Ready {
            account: "ada@example.com".to_owned(),
        });
        state
    }

    fn up_to_date() -> MailEvent {
        MailEvent::SyncStatus(SyncStatus::UpToDate(Changes::default()))
    }

    fn progress(phase: Phase, threads: u64) -> MailEvent {
        MailEvent::SyncStatus(SyncStatus::Initial(Progress {
            phase,
            threads,
            ..Progress::default()
        }))
    }

    #[test]
    fn starts_waiting_for_the_service() {
        let state = AppState::new();
        assert_eq!(state.session(), &Session::Starting);
        assert_eq!(state.sync(), &SyncState::Waiting);
        assert!(state.is_online());
        assert_eq!(state.status_text().as_deref(), Some("Opening mailbox…"));
    }

    #[test]
    fn a_new_session_invalidates_everything_and_resets_sync() {
        let mut state = ready();
        state.apply(MailEvent::Connectivity(Connectivity::Offline));
        state.apply(up_to_date());

        let changed = state.set_session(Session::Ready {
            account: "grace@example.com".to_owned(),
        });
        assert!(changed.status && changed.threads && changed.labels);
        assert_eq!(state.sync(), &SyncState::Waiting);
        assert!(state.is_online());

        let same = state.set_session(Session::Ready {
            account: "grace@example.com".to_owned(),
        });
        assert!(same.is_empty());
    }

    #[test]
    fn signing_out_does_not_ask_for_queries() {
        let mut state = AppState::new();
        let changed = state.set_session(Session::SignedOut {
            reason: "No account".to_owned(),
        });
        assert!(changed.status);
        assert!(!changed.threads && !changed.labels);
        assert_eq!(state.status_text(), None);
    }

    #[test]
    fn invalidations_are_merged_across_a_batch() {
        let mut state = ready();
        let a = MessageId::new("a");
        let b = MessageId::new("b");
        let changed = state.apply_all([
            MailEvent::ThreadsChanged,
            MailEvent::BodyFetched(a.clone()),
            MailEvent::ThreadsChanged,
            MailEvent::BodyFetched(b.clone()),
        ]);
        assert_eq!(
            changed,
            Changed {
                status: false,
                threads: true,
                labels: false,
                bodies: vec![a, b],
            }
        );
        assert!(state.apply(MailEvent::LabelsChanged).labels);
    }

    #[test]
    fn status_changes_only_when_something_differs() {
        let mut state = ready();
        assert!(state.apply(progress(Phase::Inbox, 10)).status);
        assert!(!state.apply(progress(Phase::Inbox, 10)).status);
        assert!(state.apply(progress(Phase::Inbox, 20)).status);
        assert!(
            !state
                .apply(MailEvent::Connectivity(Connectivity::Online))
                .status
        );
        assert!(
            state
                .apply(MailEvent::Connectivity(Connectivity::Offline))
                .status
        );
    }

    #[test]
    fn status_text_follows_the_sync() {
        let mut state = ready();
        assert_eq!(state.status_text().as_deref(), Some("Connecting…"));

        state.apply(progress(Phase::Labels, 0));
        assert_eq!(state.status_text().as_deref(), Some("Syncing labels…"));

        state.apply(progress(Phase::Backfill, 12_345));
        assert_eq!(
            state.status_text().as_deref(),
            Some("Syncing… 12,345 threads")
        );

        state.apply(up_to_date());
        assert_eq!(state.status_text().as_deref(), Some("Up to date"));

        state.apply(MailEvent::SyncStatus(SyncStatus::Retrying {
            error: Arc::new(Error::Cancelled),
            retry_in: Duration::from_millis(29_500),
        }));
        assert_eq!(state.status_text().as_deref(), Some("Retrying in 30s"));

        state.apply(MailEvent::SyncStatus(SyncStatus::Stopped(Arc::new(
            Error::ServiceStopped,
        ))));
        assert_eq!(state.status_text().as_deref(), Some("Sync stopped"));
        assert!(matches!(state.sync(), SyncState::Stopped { error } if !error.is_empty()));
    }

    #[test]
    fn offline_and_stopped_sending_take_precedence() {
        let mut state = ready();
        state.apply(up_to_date());

        let changed = state.apply(MailEvent::OpsStopped(Arc::new(Error::ServiceStopped)));
        assert!(changed.status);
        assert!(state.ops_stopped().is_some());
        assert_eq!(state.status_text().as_deref(), Some("Not sending changes"));

        state.apply(MailEvent::Connectivity(Connectivity::Offline));
        assert_eq!(state.status_text().as_deref(), Some("Offline"));
    }

    #[test]
    fn the_stream_ending_fails_a_running_session_only() {
        let mut state = ready();
        assert!(state.service_ended().status);
        assert!(matches!(state.session(), Session::Failed { .. }));
        assert!(state.service_ended().is_empty());

        let mut state = AppState::new();
        state.set_session(Session::SignedOut {
            reason: String::new(),
        });
        assert!(state.service_ended().is_empty());
    }

    #[test]
    fn formats_counts_and_waits() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
        assert_eq!(short_duration(Duration::from_secs(5)), "5s");
        assert_eq!(short_duration(Duration::from_secs(119)), "119s");
        assert_eq!(short_duration(Duration::from_secs(120)), "2m");
        assert_eq!(short_duration(Duration::from_secs(121)), "3m");
    }
}
