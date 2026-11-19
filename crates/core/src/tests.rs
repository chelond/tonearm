//! State machine tests with a fake auth provider (no network, no keyring).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::auth::{AuthProvider, ConnectError, Connected, LoginError, LoginInput};
use crate::{AuthState, CREDENTIALS_KEY, Command, Event, MemoryStore, Notice, Secret, SecretStore};

#[derive(Clone)]
struct FakeSession(Arc<AtomicBool>);

#[derive(Default)]
struct Fake {
    /// Results returned by successive `connect` calls; when empty, connect succeeds.
    connect_results: Mutex<Vec<Result<(), ConnectError>>>,
    login_result: Mutex<Option<Result<Secret, LoginError>>>,
    connects: AtomicUsize,
    last_session: Mutex<Option<FakeSession>>,
}

impl AuthProvider for Arc<Fake> {
    type Session = FakeSession;

    async fn login_in_browser(&self) -> Result<Secret, LoginError> {
        self.login_result
            .lock()
            .unwrap()
            .take()
            .unwrap_or(Ok(Secret::new("token")))
    }

    fn cancel_login(&self) {}

    async fn connect(&self, input: LoginInput) -> Result<Connected<FakeSession>, ConnectError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        let next = {
            let mut results = self.connect_results.lock().unwrap();
            if results.is_empty() {
                Ok(())
            } else {
                results.remove(0)
            }
        };
        next?;
        if let LoginInput::Stored(s) = &input {
            assert_eq!(s.expose(), "reusable");
        }
        let session = FakeSession(Arc::new(AtomicBool::new(true)));
        *self.last_session.lock().unwrap() = Some(session.clone());
        Ok(Connected {
            session,
            username: "egor".into(),
            reusable: Secret::new("reusable"),
        })
    }

    fn is_alive(session: &FakeSession) -> bool {
        session.0.load(Ordering::SeqCst)
    }

    fn shutdown(session: &FakeSession) {
        session.0.store(false, Ordering::SeqCst);
    }
}

struct Harness {
    cmd: mpsc::Sender<Command>,
    ev: mpsc::Receiver<Event>,
    store: Arc<MemoryStore>,
    fake: Arc<Fake>,
    sessions: watch::Receiver<Option<FakeSession>>,
}

fn start(fake: Fake, store: MemoryStore) -> Harness {
    let fake = Arc::new(fake);
    let store = Arc::new(store);
    let (cmd, cmd_rx) = mpsc::channel(8);
    let (ev_tx, ev) = mpsc::channel(32);
    let dyn_store: Arc<dyn SecretStore> = store.clone();
    let (session_tx, sessions) = watch::channel(None);
    tokio::spawn(crate::run_backend_with(
        fake.clone(),
        dyn_store,
        cmd_rx,
        ev_tx,
        session_tx,
    ));
    Harness {
        cmd,
        ev,
        store,
        fake,
        sessions,
    }
}

impl Harness {
    /// Waits for the given auth state, skipping other events.
    async fn wait_state(&mut self, want: AuthState) {
        let res = tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                match self.ev.recv().await.expect("backend gone") {
                    Event::Auth(s) if s == want => return,
                    _ => {}
                }
            }
        })
        .await;
        assert!(res.is_ok(), "state {want:?} not reached");
    }

    /// Waits for an error notice containing `needle`.
    async fn wait_error(&mut self, needle: &str) {
        loop {
            if let Event::Notice(Notice::Error(m)) = self.ev.recv().await.expect("backend gone")
                && m.contains(needle)
            {
                return;
            }
        }
    }
}

fn logged_in() -> AuthState {
    AuthState::LoggedIn {
        username: "egor".into(),
    }
}

fn store_with_creds() -> MemoryStore {
    let store = MemoryStore::default();
    store
        .set(CREDENTIALS_KEY, &Secret::new("reusable"))
        .unwrap();
    store
}

#[tokio::test]
async fn no_saved_credentials_means_logged_out() {
    let mut h = start(Fake::default(), MemoryStore::default());
    h.wait_state(AuthState::LoggedOut).await;
    assert_eq!(h.fake.connects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn auto_login_with_saved_credentials() {
    let mut h = start(Fake::default(), store_with_creds());
    h.wait_state(logged_in()).await;
}

#[tokio::test]
async fn browser_login_saves_credentials_and_logout_deletes_them() {
    let mut h = start(Fake::default(), MemoryStore::default());
    h.wait_state(AuthState::LoggedOut).await;
    h.cmd.send(Command::Login).await.unwrap();
    h.wait_state(AuthState::WaitingForBrowser).await;
    h.wait_state(logged_in()).await;
    assert_eq!(
        h.store.get(CREDENTIALS_KEY).unwrap().unwrap().expose(),
        "reusable"
    );

    assert!(h.sessions.borrow().is_some(), "player must get the session");

    h.cmd.send(Command::Logout).await.unwrap();
    h.wait_state(AuthState::LoggedOut).await;
    assert!(
        h.sessions.borrow().is_none(),
        "player must lose the session"
    );
    assert!(h.store.get(CREDENTIALS_KEY).unwrap().is_none());
    let session = h.fake.last_session.lock().unwrap().clone().unwrap();
    assert!(
        !session.0.load(Ordering::SeqCst),
        "session must be shut down"
    );
}

#[tokio::test]
async fn cancelled_login_returns_to_logged_out_without_error() {
    let fake = Fake::default();
    *fake.login_result.lock().unwrap() = Some(Err(LoginError::Cancelled));
    let mut h = start(fake, MemoryStore::default());
    h.wait_state(AuthState::LoggedOut).await;
    h.cmd.send(Command::Login).await.unwrap();
    h.wait_state(AuthState::WaitingForBrowser).await;
    h.wait_state(AuthState::LoggedOut).await;
}

#[tokio::test]
async fn rejected_credentials_are_deleted() {
    let fake = Fake::default();
    *fake.connect_results.lock().unwrap() = vec![Err(ConnectError::Rejected("bad".into()))];
    let mut h = start(fake, store_with_creds());
    h.wait_state(AuthState::LoggedOut).await;
    assert!(h.store.get(CREDENTIALS_KEY).unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn network_error_retries_with_backoff() {
    let fake = Fake::default();
    *fake.connect_results.lock().unwrap() = vec![
        Err(ConnectError::Network("offline".into())),
        Err(ConnectError::Network("offline".into())),
    ];
    let mut h = start(fake, store_with_creds());
    h.wait_state(AuthState::Reconnecting).await;
    h.wait_state(logged_in()).await;
    assert_eq!(h.fake.connects.load(Ordering::SeqCst), 3);
    // Credentials survive network errors.
    assert!(h.store.get(CREDENTIALS_KEY).unwrap().is_some());
}

#[tokio::test(start_paused = true)]
async fn dropped_session_reconnects() {
    let mut h = start(Fake::default(), store_with_creds());
    h.wait_state(logged_in()).await;
    let session = h.fake.last_session.lock().unwrap().clone().unwrap();
    session.0.store(false, Ordering::SeqCst);
    h.wait_state(AuthState::Reconnecting).await;
    h.wait_state(logged_in()).await;
    assert_eq!(h.fake.connects.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn works_without_keyring() {
    let mut h = start(Fake::default(), MemoryStore::failing());
    h.wait_error("Автовход недоступен").await;
    h.cmd.send(Command::Login).await.unwrap();
    h.wait_error("Не удалось сохранить вход").await;
    h.wait_state(logged_in()).await;
}
