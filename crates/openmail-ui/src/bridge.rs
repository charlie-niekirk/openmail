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

use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Subscription, Task,
    WeakEntity,
};
use openmail_core::MessageId;
use openmail_sync::{MailEvent, MailEvents, MailHandle, MailService};

use crate::onboarding::{Notice, SignIn};
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
    /// There is no signed-in account to open; `reason` says what is missing.
    SignedOut { reason: String },
    /// Starting the service failed.
    Failed { error: String },
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
    state: AppState,
    mail: Option<MailHandle>,
    service: Option<MailService>,
    /// Starting the service, then receiving its events.
    task: Option<Task<()>>,
    _quit: Subscription,
}

struct GlobalAppModel(Entity<AppModel>);

impl Global for GlobalAppModel {}

impl EventEmitter<AppEvent> for AppModel {}

impl AppModel {
    /// Creates the model, registers it as a global and starts the mail
    /// service by running `connect` on a background thread.
    pub fn init(cx: &mut App, connect: impl FnOnce() -> Connection + Send + 'static) {
        let model = cx.new(|cx| {
            let connecting = cx.background_spawn(async move { connect() });
            let task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let connection = connecting.await;
                // If the model is gone, `connection` is dropped here, which
                // shuts the service down.
                this.update(cx, |this, cx| this.connected(connection, cx))
                    .ok();
            });
            // Stop syncing and sending before the process exits; unsent ops
            // stay queued for the next start.
            let quit = cx.on_app_quit(|this: &mut Self, cx| {
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
                state: AppState::new(),
                mail: None,
                service: None,
                task: Some(task),
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

    fn connected(&mut self, connection: Connection, cx: &mut Context<'_, Self>) {
        let changed = match connection {
            Connection::Started {
                account,
                service,
                events,
            } => {
                let changed = self.state.set_session(Session::Ready { account });
                self.mail = Some(service.handle());
                self.service = Some(service);
                self.task = Some(cx.spawn(async move |this, cx| receive(events, this, cx).await));
                changed
            }
            Connection::SignedOut { reason } => {
                self.state.set_session(Session::SignedOut(SignIn::Ready {
                    notice: Some(Notice::Info(reason)),
                }))
            }
            Connection::Failed { error } => self.state.set_session(Session::Failed { error }),
        };
        self.publish(changed, cx);
    }

    fn apply(&mut self, events: Vec<MailEvent>, cx: &mut Context<'_, Self>) {
        let changed = self.state.apply_all(events);
        self.publish(changed, cx);
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
