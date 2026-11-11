//! Onboarding as plain view-models: the sign-in screen ([`SignIn`]) and the
//! progress of a newly signed-in account's first sync ([`Setup`]).
//!
//! [`AppState`](crate::AppState) holds them and moves between them; the
//! [bridge](crate::bridge) runs the sign-in itself off the main thread.

use openmail_sync::Phase;

use crate::state::{SyncState, group_thousands, short_duration};

/// Why a sign-in attempt ended without an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInError {
    /// The user cancelled it in the app.
    Cancelled,
    /// The user declined on Google's consent page.
    Denied,
    /// No OAuth client is configured; `help` says how to set one up.
    Unconfigured { help: String },
    /// Anything else, such as the network or the keychain failing.
    Failed { error: String },
}

/// The sign-in screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignIn {
    /// Offering "Sign in with Google", with a notice about why, if there is one.
    Ready { notice: Option<Notice> },
    /// Google's consent page is open in the browser; waiting for the user to
    /// finish there (or cancel here).
    InBrowser,
    /// Signing in cannot work until an OAuth client is configured; `help`
    /// says how.
    Unconfigured { help: String },
}

/// A message on the sign-in screen about the last attempt or session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Why no account was opened at start-up.
    Info(String),
    /// The user declined on Google's consent page.
    Denied,
    /// `account` was signed out by Google (its access was revoked or expired).
    Expired { account: String },
    /// The last attempt failed.
    Failed(String),
}

impl Notice {
    /// The message to show.
    pub fn text(&self) -> String {
        match self {
            Self::Info(text) => text.clone(),
            Self::Denied => "Sign-in was cancelled in the browser.".to_owned(),
            Self::Expired { account } => {
                format!("Google signed {account} out. Sign in again to keep syncing.")
            }
            Self::Failed(error) => format!("Couldn't sign in: {error}"),
        }
    }

    /// Whether to show it as an error.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

impl SignIn {
    /// Starts an attempt if one can start now, returning whether it did.
    pub fn begin(&mut self) -> bool {
        match self {
            Self::Ready { .. } => {
                *self = Self::InBrowser;
                true
            }
            Self::InBrowser | Self::Unconfigured { .. } => false,
        }
    }

    /// Whether an attempt is running.
    pub fn is_in_browser(&self) -> bool {
        matches!(self, Self::InBrowser)
    }

    /// The attempt ended without an account.
    pub fn ended(&mut self, error: SignInError) {
        *self = match error {
            SignInError::Cancelled => Self::Ready { notice: None },
            SignInError::Denied => Self::Ready {
                notice: Some(Notice::Denied),
            },
            SignInError::Unconfigured { help } => Self::Unconfigured { help },
            SignInError::Failed { error } => Self::Ready {
                notice: Some(Notice::Failed(error)),
            },
        };
    }
}

/// What the setup screen shows while a newly signed-in account's first sync
/// runs, until its inbox is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    /// What is happening.
    pub title: String,
    /// More about it, such as how many threads are synced.
    pub detail: Option<String>,
    /// Whether the sync failed and the title says so (no progress to show).
    pub stopped: bool,
}

impl Setup {
    /// The screen for the sync's last reported state.
    pub fn for_sync(sync: &SyncState) -> Self {
        let (title, detail, stopped) = match sync {
            SyncState::Waiting => ("Connecting to Gmail…".to_owned(), None, false),
            SyncState::Initial(progress) => {
                let title = match progress.phase {
                    Phase::Labels => "Syncing labels…",
                    _ => "Syncing your inbox…",
                };
                let detail = match progress.rate_limit_wait {
                    Some(wait) => {
                        format!("Waiting {} for Gmail's rate limit", short_duration(wait))
                    }
                    None if progress.threads == 1 => "1 conversation".to_owned(),
                    None => format!("{} conversations", group_thousands(progress.threads)),
                };
                (title.to_owned(), Some(detail), false)
            }
            // `inbox_ready` ends setup before these are shown; say so anyway.
            SyncState::UpToDate => ("Your inbox is ready".to_owned(), None, false),
            SyncState::Retrying { error, retry_in } => (
                "Can't reach Gmail".to_owned(),
                Some(format!(
                    "{error}. Retrying in {}.",
                    short_duration(*retry_in)
                )),
                false,
            ),
            SyncState::Stopped { error } => ("Sync stopped".to_owned(), Some(error.clone()), true),
        };
        Self {
            title,
            detail,
            stopped,
        }
    }
}

/// Whether the first sync has stored the inbox, so the mailbox can be shown
/// while older mail keeps syncing in the background.
pub fn inbox_ready(sync: &SyncState) -> bool {
    match sync {
        SyncState::UpToDate => true,
        SyncState::Initial(progress) => {
            matches!(
                progress.phase,
                Phase::Prefetch | Phase::Backfill | Phase::Done
            )
        }
        SyncState::Waiting | SyncState::Retrying { .. } | SyncState::Stopped { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use openmail_sync::Progress;

    use super::*;

    fn initial(phase: Phase, threads: u64) -> SyncState {
        SyncState::Initial(Progress {
            phase,
            threads,
            ..Progress::default()
        })
    }

    #[test]
    fn one_attempt_runs_at_a_time() {
        let mut sign_in = SignIn::Ready { notice: None };
        assert!(sign_in.begin());
        assert!(sign_in.is_in_browser());
        assert!(!sign_in.begin());

        let mut unconfigured = SignIn::Unconfigured {
            help: "Set it".to_owned(),
        };
        assert!(!unconfigured.begin());
    }

    #[test]
    fn an_attempt_ends_back_on_the_sign_in_screen() {
        let ended = |error| {
            let mut sign_in = SignIn::InBrowser;
            sign_in.ended(error);
            sign_in
        };
        assert_eq!(
            ended(SignInError::Cancelled),
            SignIn::Ready { notice: None }
        );
        assert_eq!(
            ended(SignInError::Denied),
            SignIn::Ready {
                notice: Some(Notice::Denied)
            }
        );
        assert_eq!(
            ended(SignInError::Failed {
                error: "offline".to_owned()
            }),
            SignIn::Ready {
                notice: Some(Notice::Failed("offline".to_owned()))
            }
        );
        assert_eq!(
            ended(SignInError::Unconfigured {
                help: "Set it".to_owned()
            }),
            SignIn::Unconfigured {
                help: "Set it".to_owned()
            }
        );
    }

    #[test]
    fn notices_explain_themselves() {
        assert!(Notice::Denied.text().contains("cancelled"));
        assert!(
            Notice::Expired {
                account: "ada@example.com".to_owned()
            }
            .text()
            .contains("ada@example.com")
        );
        let failed = Notice::Failed("offline".to_owned());
        assert_eq!(failed.text(), "Couldn't sign in: offline");
        assert!(failed.is_error());
        assert!(!Notice::Denied.is_error());
    }

    #[test]
    fn setup_follows_the_first_sync() {
        let setup = Setup::for_sync(&SyncState::Waiting);
        assert_eq!(setup.title, "Connecting to Gmail…");
        assert!(!setup.stopped);

        assert_eq!(
            Setup::for_sync(&initial(Phase::Labels, 0)).title,
            "Syncing labels…"
        );
        let inbox = Setup::for_sync(&initial(Phase::Inbox, 1_250));
        assert_eq!(inbox.title, "Syncing your inbox…");
        assert_eq!(inbox.detail.as_deref(), Some("1,250 conversations"));
        assert_eq!(
            Setup::for_sync(&initial(Phase::Inbox, 1)).detail.as_deref(),
            Some("1 conversation")
        );

        let waiting = Setup::for_sync(&SyncState::Initial(Progress {
            phase: Phase::Inbox,
            rate_limit_wait: Some(Duration::from_secs(20)),
            ..Progress::default()
        }));
        assert_eq!(
            waiting.detail.as_deref(),
            Some("Waiting 20s for Gmail's rate limit")
        );

        let retrying = Setup::for_sync(&SyncState::Retrying {
            error: "connection refused".to_owned(),
            retry_in: Duration::from_secs(5),
        });
        assert_eq!(
            retrying.detail.as_deref(),
            Some("connection refused. Retrying in 5s.")
        );
        assert!(!retrying.stopped);

        let stopped = Setup::for_sync(&SyncState::Stopped {
            error: "quota".to_owned(),
        });
        assert!(stopped.stopped);
        assert_eq!(stopped.detail.as_deref(), Some("quota"));
    }

    #[test]
    fn the_inbox_is_ready_once_it_is_stored() {
        assert!(!inbox_ready(&SyncState::Waiting));
        assert!(!inbox_ready(&initial(Phase::Labels, 0)));
        assert!(!inbox_ready(&initial(Phase::Inbox, 40)));
        assert!(inbox_ready(&initial(Phase::Prefetch, 40)));
        assert!(inbox_ready(&initial(Phase::Backfill, 400)));
        assert!(inbox_ready(&SyncState::UpToDate));
        assert!(!inbox_ready(&SyncState::Stopped {
            error: String::new()
        }));
    }
}
