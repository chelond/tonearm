//! Backend state machine: auto-login, browser login, logout, reconnect with backoff.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::auth::{AuthProvider, ConnectError, Connected, LoginError, LoginInput};
use crate::secret::{Secret, SecretStore};
use crate::{AuthState, Command, Event, Notice};

/// Keyring entry holding librespot reusable credentials.
pub const CREDENTIALS_KEY: &str = "librespot-credentials";

const HEALTH_CHECK: Duration = Duration::from_secs(5);
const BACKOFF: &[u64] = &[2, 4, 8, 16, 30, 60];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectOrigin {
    /// Credentials from the keyring at startup or on reconnect.
    Stored,
    /// Fresh token from the browser login.
    Browser,
}

enum Internal<S> {
    LoginFinished(Result<Secret, LoginError>),
    ConnectFinished(ConnectOrigin, Box<Result<Connected<S>, ConnectError>>),
}

struct Backend<A: AuthProvider> {
    auth: Arc<A>,
    store: Arc<dyn SecretStore>,
    events: mpsc::Sender<Event>,
    /// The live session for the player; `None` while logged out or disconnected.
    sessions: watch::Sender<Option<A::Session>>,
    internal_tx: mpsc::Sender<Internal<A::Session>>,
    session: Option<A::Session>,
    /// Reusable credentials kept in memory, so reconnects work even without a keyring.
    credentials: Option<Secret>,
    state: AuthState,
    busy: bool,
    retry_at: Option<Instant>,
    attempt: usize,
}

/// Runs the backend until [`Command::Shutdown`] or until the UI drops the command channel.
pub async fn run<A: AuthProvider>(
    auth: A,
    store: Arc<dyn SecretStore>,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    sessions: watch::Sender<Option<A::Session>>,
) {
    let (internal_tx, mut internal_rx) = mpsc::channel(8);
    let mut backend = Backend {
        auth: Arc::new(auth),
        store,
        events,
        sessions,
        internal_tx,
        session: None,
        credentials: None,
        state: AuthState::Checking,
        busy: false,
        retry_at: None,
        attempt: 0,
    };
    backend.startup().await;

    let mut health = tokio::time::interval(HEALTH_CHECK);
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let retry_at = backend.retry_at;
        tokio::select! {
            cmd = commands.recv() => match cmd {
                None | Some(Command::Shutdown) => break,
                Some(cmd) => backend.handle_command(cmd).await,
            },
            Some(msg) = internal_rx.recv() => backend.handle_internal(msg).await,
            () = sleep_until_opt(retry_at) => {
                backend.retry_at = None;
                backend.reconnect();
            }
            _ = health.tick() => backend.check_health().await,
        }
    }
    backend.auth.cancel_login();
    backend.sessions.send_replace(None);
    if let Some(session) = backend.session.take() {
        A::shutdown(&session);
    }
    tracing::info!("backend stopped");
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl<A: AuthProvider> Backend<A> {
    async fn emit(&self, event: Event) {
        // The UI may already be gone during shutdown; nothing to do then.
        let _ = self.events.send(event).await;
    }

    async fn set_state(&mut self, state: AuthState) {
        if self.state != state {
            tracing::info!(?state, "auth state");
            self.state = state.clone();
            self.emit(Event::Auth(state)).await;
        }
    }

    async fn notice(&self, notice: Notice) {
        match &notice {
            Notice::Error(m) => tracing::warn!("{m}"),
            Notice::Info(m) => tracing::info!("{m}"),
        }
        self.emit(Event::Notice(notice)).await;
    }

    async fn startup(&mut self) {
        self.emit(Event::Auth(AuthState::Checking)).await;
        match self.store_call(|s| s.get(CREDENTIALS_KEY)).await {
            Ok(Some(creds)) => {
                self.credentials = Some(creds);
                self.reconnect();
            }
            Ok(None) => self.set_state(AuthState::LoggedOut).await,
            Err(err) => {
                self.set_state(AuthState::LoggedOut).await;
                self.notice(Notice::Error(format!(
                    "Автовход недоступен ({err}). Войдите вручную."
                )))
                .await;
            }
        }
    }

    async fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Login => self.start_browser_login().await,
            Command::CancelLogin => self.auth.cancel_login(),
            Command::Logout => self.logout().await,
            Command::RetryNow => {
                if self.retry_at.is_some() {
                    self.retry_at = Some(Instant::now());
                }
            }
            Command::Shutdown => {}
        }
    }

    async fn start_browser_login(&mut self) {
        if self.busy || matches!(self.state, AuthState::LoggedIn { .. }) {
            return;
        }
        self.busy = true;
        self.retry_at = None;
        self.set_state(AuthState::WaitingForBrowser).await;
        let auth = Arc::clone(&self.auth);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = auth.login_in_browser().await;
            let _ = tx.send(Internal::LoginFinished(result)).await;
        });
    }

    fn spawn_connect(&mut self, origin: ConnectOrigin, input: LoginInput) {
        self.busy = true;
        let auth = Arc::clone(&self.auth);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = auth.connect(input).await;
            let _ = tx
                .send(Internal::ConnectFinished(origin, Box::new(result)))
                .await;
        });
    }

    /// Connects with the reusable credentials kept in memory.
    fn reconnect(&mut self) {
        if self.busy {
            return;
        }
        if let Some(creds) = self.credentials.clone() {
            self.state = AuthState::Connecting;
            let _ = self.events.try_send(Event::Auth(AuthState::Connecting));
            self.spawn_connect(ConnectOrigin::Stored, LoginInput::Stored(creds));
        }
    }

    async fn handle_internal(&mut self, msg: Internal<A::Session>) {
        match msg {
            Internal::LoginFinished(Ok(token)) => {
                self.busy = false;
                self.set_state(AuthState::Connecting).await;
                self.spawn_connect(ConnectOrigin::Browser, LoginInput::AccessToken(token));
            }
            Internal::LoginFinished(Err(err)) => {
                self.busy = false;
                self.set_state(AuthState::LoggedOut).await;
                if err != LoginError::Cancelled {
                    self.notice(Notice::Error(err.to_string())).await;
                }
            }
            Internal::ConnectFinished(origin, result) => {
                self.busy = false;
                self.on_connected(origin, *result).await;
            }
        }
    }

    async fn on_connected(
        &mut self,
        origin: ConnectOrigin,
        result: Result<Connected<A::Session>, ConnectError>,
    ) {
        match result {
            Ok(connected) => {
                if self.attempt > 0 || self.state == AuthState::Reconnecting {
                    self.emit(Event::Online).await;
                }
                self.attempt = 0;
                self.sessions.send_replace(Some(connected.session.clone()));
                if let Some(old) = self.session.replace(connected.session) {
                    A::shutdown(&old);
                }
                self.credentials = Some(connected.reusable.clone());
                let reusable = connected.reusable;
                if let Err(err) = self
                    .store_call(move |s| s.set(CREDENTIALS_KEY, &reusable))
                    .await
                {
                    self.notice(Notice::Error(format!(
                        "Не удалось сохранить вход ({err}). При следующем запуске придётся войти снова."
                    )))
                    .await;
                }
                self.set_state(AuthState::LoggedIn {
                    username: connected.username,
                })
                .await;
            }
            Err(ConnectError::Rejected(reason)) => {
                tracing::warn!(%reason, "credentials rejected");
                self.credentials = None;
                let _ = self.store_call(|s| s.delete(CREDENTIALS_KEY)).await;
                self.set_state(AuthState::LoggedOut).await;
                self.notice(Notice::Error(
                    "Сессия истекла или вход отклонён. Войдите снова.".into(),
                ))
                .await;
            }
            Err(ConnectError::Network(reason)) if origin == ConnectOrigin::Browser => {
                self.set_state(AuthState::LoggedOut).await;
                self.notice(Notice::Error(format!(
                    "Не удалось подключиться: {reason}. Попробуйте ещё раз."
                )))
                .await;
            }
            Err(ConnectError::Network(reason)) => {
                let delay = BACKOFF[self.attempt.min(BACKOFF.len() - 1)];
                self.attempt += 1;
                self.retry_at = Some(Instant::now() + Duration::from_secs(delay));
                self.set_state(AuthState::Reconnecting).await;
                tracing::warn!(%reason, delay, "Spotify unreachable, retrying");
                self.emit(Event::Offline {
                    retry_in_secs: delay,
                })
                .await;
            }
        }
    }

    async fn check_health(&mut self) {
        let dead = self.session.as_ref().is_some_and(|s| !A::is_alive(s));
        if dead && !self.busy && self.retry_at.is_none() {
            self.session = None;
            self.sessions.send_replace(None);
            self.set_state(AuthState::Reconnecting).await;
            self.notice(Notice::Info("Соединение потеряно, переподключаюсь…".into()))
                .await;
            self.reconnect();
        }
    }

    async fn logout(&mut self) {
        self.auth.cancel_login();
        self.retry_at = None;
        self.attempt = 0;
        self.credentials = None;
        self.sessions.send_replace(None);
        if let Some(session) = self.session.take() {
            A::shutdown(&session);
        }
        crate::library::forget_own_token();
        if let Err(err) = self.store_call(|s| s.delete(CREDENTIALS_KEY)).await {
            self.notice(Notice::Error(format!(
                "Не удалось удалить сохранённый вход: {err}"
            )))
            .await;
        }
        self.set_state(AuthState::LoggedOut).await;
    }

    /// Keyring calls may block on D-Bus / WinAPI, so they run on the blocking pool.
    async fn store_call<T, F>(&self, f: F) -> Result<T, crate::secret::SecretStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&dyn SecretStore) -> Result<T, crate::secret::SecretStoreError> + Send + 'static,
    {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || f(store.as_ref()))
            .await
            .unwrap_or_else(|e| Err(crate::secret::SecretStoreError::Unavailable(e.to_string())))
    }
}
