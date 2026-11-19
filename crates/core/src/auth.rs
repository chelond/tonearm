//! Authorization: OAuth login in the browser and librespot session connection.
//!
//! [`AuthProvider`] abstracts the network side so the backend state machine can be tested
//! without Spotify. [`LibrespotAuth`] is the real implementation.

use std::future::Future;
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

use librespot_core::authentication::Credentials;
use librespot_core::error::ErrorKind;
use librespot_core::{Session, SessionConfig};
use librespot_oauth::{OAuthClientBuilder, OAuthError};
use librespot_protocol::authentication::AuthenticationType;

use crate::secret::Secret;

/// Loopback redirect accepted for librespot's client id; the port is fixed by registration.
pub const REDIRECT_PORT: u16 = 8898;
pub(crate) const REDIRECT_URI: &str = "http://127.0.0.1:8898/login";

/// Scopes for the streaming session; Web API tokens are requested separately.
const SCOPES: &[&str] = &[
    "streaming",
    "user-read-private",
    "user-library-read",
    "user-library-modify",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-private",
    "playlist-modify-public",
    "user-follow-read",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-read-recently-played",
    "user-top-read",
];

const BROWSER_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Tonearm</title></head>\
<body style=\"background:#121212;color:#eee;font-family:sans-serif;text-align:center;padding-top:20vh\">\
<h2>Вход выполнен</h2><p>Можно закрыть вкладку и вернуться в Tonearm.</p></body></html>";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How to authenticate a session.
#[derive(Debug, Clone)]
pub enum LoginInput {
    /// OAuth access token right after the browser login.
    AccessToken(Secret),
    /// Reusable credentials (JSON) previously saved in the keyring.
    Stored(Secret),
}

/// A connected session plus the reusable credentials to save for the next start.
pub struct Connected<S> {
    pub session: S,
    pub username: String,
    pub reusable: Secret,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    /// Spotify rejected the credentials: they must be dropped and the user must log in again.
    #[error("Spotify отклонил учётные данные: {0}")]
    Rejected(String),
    /// Network or server trouble: retry later with the same credentials.
    #[error("нет связи со Spotify: {0}")]
    Network(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    #[error("вход отменён")]
    Cancelled,
    #[error("порт {REDIRECT_PORT} занят другой программой, закройте её и попробуйте снова")]
    PortBusy,
    #[error("не удалось завершить вход: {0}")]
    Failed(String),
}

/// Network side of authorization.
pub trait AuthProvider: Send + Sync + 'static {
    type Session: Clone + Send + Sync + 'static;

    /// Opens the browser and waits for the OAuth redirect. Returns an access token.
    fn login_in_browser(&self) -> impl Future<Output = Result<Secret, LoginError>> + Send;
    /// Interrupts a pending [`AuthProvider::login_in_browser`].
    fn cancel_login(&self);
    fn connect(
        &self,
        input: LoginInput,
    ) -> impl Future<Output = Result<Connected<Self::Session>, ConnectError>> + Send;
    fn is_alive(session: &Self::Session) -> bool;
    fn shutdown(session: &Self::Session);
}

/// Real implementation on top of librespot.
#[derive(Debug, Default, Clone)]
pub struct LibrespotAuth;

impl AuthProvider for LibrespotAuth {
    type Session = Session;

    async fn login_in_browser(&self) -> Result<Secret, LoginError> {
        // librespot's own client id: the session protocol only accepts Spotify's client ids.
        let client_id = SessionConfig::default().client_id;
        let task = tokio::task::spawn_blocking(move || {
            let client = OAuthClientBuilder::new(&client_id, REDIRECT_URI, SCOPES.to_vec())
                .open_in_browser()
                .with_custom_message(BROWSER_PAGE)
                .build()
                .map_err(map_oauth_err)?;
            // The sync variant: its redirect listener blocks, so it must not run on an async worker.
            client
                .get_access_token()
                .map(|token| Secret::new(token.access_token))
                .map_err(map_oauth_err)
        });
        task.await
            .map_err(|e| LoginError::Failed(format!("задача входа упала: {e}")))?
    }

    fn cancel_login(&self) {
        // The listener waits for exactly one request; a request without `code` ends it.
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", REDIRECT_PORT)) {
            let _ = stream.write_all(
                b"GET /login?error=cancelled HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            );
        }
    }

    async fn connect(&self, input: LoginInput) -> Result<Connected<Session>, ConnectError> {
        let credentials = match input {
            LoginInput::AccessToken(token) => Credentials::with_access_token(token.expose()),
            LoginInput::Stored(json) => serde_json::from_str::<Credentials>(json.expose())
                .map_err(|_| ConnectError::Rejected("сохранённые данные повреждены".into()))?,
        };
        let session = Session::new(SessionConfig::default(), None);
        match tokio::time::timeout(CONNECT_TIMEOUT, session.connect(credentials, false)).await {
            Err(_) => Err(ConnectError::Network("превышено время ожидания".into())),
            Ok(Err(err)) if err.kind == ErrorKind::PermissionDenied => {
                Err(ConnectError::Rejected(err.to_string()))
            }
            Ok(Err(err)) => Err(ConnectError::Network(err.to_string())),
            Ok(Ok(())) => {
                let username = session.username();
                let reusable = Credentials {
                    username: Some(username.clone()),
                    auth_type: AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS,
                    auth_data: session.auth_data(),
                };
                let json = serde_json::to_string(&reusable)
                    .map_err(|e| ConnectError::Network(format!("сериализация: {e}")))?;
                Ok(Connected {
                    session,
                    username,
                    reusable: Secret::new(json),
                })
            }
        }
    }

    fn is_alive(session: &Session) -> bool {
        !session.is_invalid()
    }

    fn shutdown(session: &Session) {
        if !session.is_invalid() {
            session.shutdown();
        }
    }
}

fn map_oauth_err(err: OAuthError) -> LoginError {
    match err {
        // Redirect without `code`: the user denied access, or we cancelled ourselves.
        OAuthError::AuthCodeNotFound { .. } => LoginError::Cancelled,
        OAuthError::AuthCodeListenerBind { .. } => LoginError::PortBusy,
        // Messages of these variants may contain the redirect URI with the code: do not forward.
        OAuthError::AuthCodeBadUri { .. } | OAuthError::InvalidRedirectUri { .. } => {
            LoginError::Failed("некорректный адрес возврата".into())
        }
        other => LoginError::Failed(other.to_string()),
    }
}
