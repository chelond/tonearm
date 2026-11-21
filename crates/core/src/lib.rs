//! Core layer: session, authorization, librespot wrapper, Web API client, models, cache.
//!
//! The UI talks to the core only through [`Command`] and [`Event`] channels.

pub mod auth;
mod backend;
pub mod config;
pub mod connect;
pub mod covers;
pub mod file_store;
pub mod lastfm;
pub mod library;
pub mod secret;
pub mod update;

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

pub use auth::{AuthProvider, LibrespotAuth};
pub use backend::CREDENTIALS_KEY;
pub use librespot_core::Session;
pub use secret::{KeyringStore, MemoryStore, Secret, SecretStore};

/// Keyring service name for all secrets of the app.
pub const KEYRING_SERVICE: &str = "tonearm";

/// Commands sent from the UI to the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Start the OAuth login in the system browser.
    Login,
    /// Abort a pending browser login.
    CancelLogin,
    /// Forget saved credentials and disconnect.
    Logout,
    /// Retry the connection now instead of waiting for the backoff.
    RetryNow,
    /// Shut the backend down gracefully.
    Shutdown,
}

/// Authorization state shown by the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    /// Looking for saved credentials at startup.
    Checking,
    LoggedOut,
    /// Browser opened, waiting for the user to finish the login there.
    WaitingForBrowser,
    Connecting,
    /// Connection lost or not reachable; retrying with saved credentials.
    Reconnecting,
    LoggedIn {
        username: String,
    },
}

/// A message for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Info(String),
    Error(String),
}

/// Events sent from the backend to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Auth(AuthState),
    Notice(Notice),
    /// Spotify unreachable; the next attempt is in `retry_in_secs`.
    Offline {
        retry_in_secs: u64,
    },
    /// Connected again after [`Event::Offline`].
    Online,
}

/// Runs the real backend (librespot + system keyring).
/// The connected session is published on `sessions` for the player.
pub async fn run_backend(
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    sessions: watch::Sender<Option<Session>>,
) {
    backend::run(LibrespotAuth, system_store(), commands, events, sessions).await;
}

/// Windows: Credential Manager. Linux: Secret Service, or the encrypted file when no daemon runs.
#[must_use]
pub fn system_store() -> Arc<dyn SecretStore> {
    let keyring = KeyringStore::new(KEYRING_SERVICE);
    if cfg!(target_os = "linux") {
        match file_store::FileStore::for_current_user() {
            Ok(file) => {
                return Arc::new(file_store::FallbackStore::new(
                    Box::new(keyring),
                    Box::new(file),
                ));
            }
            Err(err) => tracing::warn!(%err, "encrypted file fallback unavailable"),
        }
    }
    Arc::new(keyring)
}

/// Runs the backend with a custom provider and store (tests, offline mode).
pub async fn run_backend_with<A: AuthProvider>(
    auth: A,
    store: Arc<dyn SecretStore>,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    sessions: watch::Sender<Option<A::Session>>,
) {
    backend::run(auth, store, commands, events, sessions).await;
}

#[cfg(test)]
mod tests;
