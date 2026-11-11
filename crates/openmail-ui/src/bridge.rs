//! The bridge between the [`MailService`], which runs on a tokio runtime of its
//! own, and GPUI, which owns the main thread.
//!
//! [`AppModel`] is a GPUI entity, registered as a global, holding the
//! [`AppState`] view-model and a [`MailHandle`]. A task started with
//! `cx.spawn` awaits the service's [`MailEvents`] on GPUI's executor, applies
//! every event waiting into the `AppState` at once, then notifies observers
//! (for the session and sync status) and emits [`AppEvent`]s (for the
//! invalidations views re-query on). Views query through the handle, whose
//! futures they await in tasks of their own, so the UI never blocks on I/O.
//!
//! The app supplies a [`Backend`] that opens the signed-in account and signs
//! new accounts in; the model calls it off the main thread.

use std::sync::Arc;

use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Subscription, Task,
    WeakEntity,
};
use openmail_core::MessageId;
use openmail_sync::{MailEvent, MailEvents, MailHandle, MailService};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::onboarding::{Notice, SignIn, SignInError};
use crate::state::{AppState, Changed, Session};

/// The outcome of starting the mail service, which the app does off the main
/// thread (opening the store and reading the keychain may block).
pub enum Connection {
    /// The service is running for `account`.
    Started {
        account: String,
        service: MailService,
        events: MailEvents,
    },
    /// There is no signed-in account to open; `reason` says more, if there
    /// is more to say.
    SignedOut { reason: Option<String> },
    /// There is no signed-in account, and signing in needs an OAuth client
    /// that is not configured; `help` says how to configure one.
    Unconfigured { help: String },
    /// Starting the service failed.
    Failed { error: String },
}

/// Opening and signing in accounts, which the app provides. Both methods
/// block (keychain, store, network), so the model calls them off the main
/// thread.
pub trait Backend: Send + Sync + 'static {
    /// Starts the mail service for the signed-in account, or says why there
    /// is none.
    fn connect(&self) -> Connection;

    /// Signs an account in through the system browser, then starts its mail
    /// service. Returns once that is done, or the attempt failed or was
    /// cancelled through `cancel`.
    fn sign_in(&self, cancel: CancellationToken) -> Result<Connection, SignInError>;
}

/// What [`AppModel`] tells its subscribers. Changes to the session and the
/// sync status are reported by notifying observers instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    /// Thread lists, label counts and open threads may be out of date: query
    /// them again.
    ThreadsChanged,
    /// The label list may be out of date.
    LabelsChanged,
    /// A message body was fetched and stored.
    BodyFetched(MessageId),
}

/// The app's state and its connection to the mail service. One per app; get
/// it with [`AppModel::global`].
pub struct AppModel {
    backend: Arc<dyn Backend>,
    state: AppState,
    mail: Option<MailHandle>,
    service: Option<MailService>,
    /// Starting the service or signing in, then receiving the service's events.
    task: Option<Task<()>>,
    /// Cancels the running sign-in attempt.
    sign_in: Option<CancellationToken>,
    _quit: Subscription,
}

struct GlobalAppModel(Entity<AppModel>);

impl Global for GlobalAppModel {}

impl EventEmitter<AppEvent> for AppModel {}

impl AppModel {
    /// Creates the model, registers it as a global and starts the mail
    /// service by calling [`Backend::connect`] on a background thread.
    pub fn init(cx: &mut App, backend: impl Backend) {
        let backend: Arc<dyn Backend> = Arc::new(backend);
        let model = cx.new(|cx| {
            let connecting = cx.background_spawn({
                let backend = backend.clone();
                async move { backend.connect() }
            });
            let task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let connection = connecting.await;
                // If the model is gone, `connection` is dropped here, which
                // shuts the service down.
                this.update(cx, |this, cx| this.connected(connection, false, cx))
                    .ok();
            });
            // Stop syncing and sending before the process exits; unsent ops
            // stay queued for the next start.
            let quit = cx.on_app_quit(|this: &mut Self, cx| {
                if let Some(sign_in) = this.sign_in.take() {
                    sign_in.cancel();
                }
                this.mail = None;
                let service = this.service.take();
                let executor = cx.background_executor().clone();
                async move {
                    if let Some(service) = service {
                        executor.spawn(async move { service.shutdown() }).await;
                    }
                }
            });
            Self {
                backend,
                state: AppState::new(),
                mail: None,
                service: None,
                task: Some(task),
                sign_in: None,
                _quit: quit,
            }
        });
        cx.set_global(GlobalAppModel(model));
    }

    /// The app's model. Panics if [`AppModel::init`] has not run.
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalAppModel>().0.clone()
    }

    /// The app's state.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// A handle to the running mail service, if there is one. Clone it into a
    /// task to query or change the mail.
    pub fn mail(&self) -> Option<&MailHandle> {
        self.mail.as_ref()
    }

    /// Signs an account in through the system browser, if the sign-in screen
    /// can start an attempt now. The attempt runs on a thread of its own, as
    /// it waits for the user for as long as they take.
    pub fn sign_in(&mut self, cx: &mut Context<'_, Self>) {
        let changed = self.state.begin_sign_in();
        if !changed.status {
            return;
        }
        let cancel = CancellationToken::new();
        let (sender, receiver) = oneshot::channel();
        let backend = self.backend.clone();
        let attempt = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("openmail-sign-in".to_owned())
            .spawn(move || {
                // The model may be gone by the time the attempt ends.
                let _ = sender.send(backend.sign_in(attempt));
            });
        if let Err(err) = spawned {
            let changed = self.state.sign_in_ended(SignInError::Failed {
                error: format!("could not start signing in: {err}"),
            });
            self.publish(changed, cx);
            return;
        }
        self.sign_in = Some(cancel);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or_else(|_| {
                Err(SignInError::Failed {
                    error: "signing in stopped unexpectedly".to_owned(),
                })
            });
            this.update(cx, |this, cx| this.signed_in(result, cx)).ok();
        }));
        self.publish(changed, cx);
    }

    /// Cancels the running sign-in attempt, if there is one. The sign-in
    /// screen returns once the attempt has stopped.
    pub fn cancel_sign_in(&mut self) {
        if let Some(sign_in) = &self.sign_in {
            sign_in.cancel();
        }
    }

    fn signed_in(&mut self, result: Result<Connection, SignInError>, cx: &mut Context<'_, Self>) {
        self.sign_in = None;
        match result {
            Ok(connection) => self.connected(connection, true, cx),
            Err(error) => {
                let changed = self.state.sign_in_ended(error);
                self.publish(changed, cx);
            }
        }
    }

    /// Applies the outcome of starting the service; `new_account` if the
    /// account was just signed in.
    fn connected(&mut self, connection: Connection, new_account: bool, cx: &mut Context<'_, Self>) {
        let changed = match connection {
            Connection::Started {
                account,
                service,
                events,
            } => {
                let changed = if new_account {
                    self.state.signed_in(account)
                } else {
                    self.state.set_session(Session::Ready { account })
                };
                self.mail = Some(service.handle());
                self.service = Some(service);
                self.task = Some(cx.spawn(async move |this, cx| receive(events, this, cx).await));
                changed
            }
            Connection::SignedOut { reason } => {
                self.state.set_session(Session::SignedOut(SignIn::Ready {
                    notice: reason.map(Notice::Info),
                }))
            }
            Connection::Unconfigured { help } => self
                .state
                .set_session(Session::SignedOut(SignIn::Unconfigured { help })),
            Connection::Failed { error } => self.state.set_session(Session::Failed { error }),
        };
        self.publish(changed, cx);
    }

    fn apply(&mut self, events: Vec<MailEvent>, cx: &mut Context<'_, Self>) {
        let changed = self.state.apply_all(events);
        if !matches!(self.state.session(), Session::Ready { .. }) {
            // The account was signed out (Google revoked its access): stop
            // its service. A new sign-in starts another.
            self.stop_service(cx);
        }
        self.publish(changed, cx);
    }

    /// Drops the handle and shuts the service down off the main thread (it
    /// joins the service's thread).
    fn stop_service(&mut self, cx: &mut Context<'_, Self>) {
        self.mail = None;
        if let Some(service) = self.service.take() {
            cx.background_spawn(async move { service.shutdown() })
                .detach();
        }
    }

    fn service_ended(&mut self, cx: &mut Context<'_, Self>) {
        self.mail = None;
        let changed = self.state.service_ended();
        self.publish(changed, cx);
    }

    /// Tells observers and subscribers what changed.
    fn publish(&mut self, changed: Changed, cx: &mut Context<'_, Self>) {
        if changed.status {
            cx.notify();
        }
        if changed.threads {
            cx.emit(AppEvent::ThreadsChanged);
        }
        if changed.labels {
            cx.emit(AppEvent::LabelsChanged);
        }
        for id in changed.bodies {
            cx.emit(AppEvent::BodyFetched(id));
        }
    }
}

/// Applies the service's events to the model as they arrive, each time with
/// all that are waiting, until the stream ends or the model is dropped.
async fn receive(mut events: MailEvents, this: WeakEntity<AppModel>, cx: &mut AsyncApp) {
    while let Some(event) = events.recv().await {
        // A busy sync sends many invalidations at once: re-render and
        // re-query once for all of them.
        let mut batch = vec![event];
        batch.extend(std::iter::from_fn(|| events.try_recv()));
        if this.update(cx, |this, cx| this.apply(batch, cx)).is_err() {
            return;
        }
    }
    this.update(cx, AppModel::service_ended).ok();
}
