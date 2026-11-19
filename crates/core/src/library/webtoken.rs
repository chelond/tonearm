//! Access token for the Web API.
//!
//! Default: login5 token of the librespot session. It belongs to Spotify's own desktop client
//! id, which every librespot-based app shares, so the Web API often answers 429 for it.
//! With `[spotify] client_id` in config.toml the token comes from the user's own app instead
//! (OAuth PKCE in the browser once, then refresh token from the secret store).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use librespot_core::Session;
use librespot_oauth::{OAuthClient, OAuthClientBuilder, OAuthToken};

use super::webapi::ApiError;
use crate::auth::REDIRECT_URI;
use crate::secret::{Secret, SecretStore};

/// Secret-store key of the own app's refresh token.
pub const WEBAPI_REFRESH_KEY: &str = "webapi-refresh-token";

/// Scopes for the own app (everything the library, search and Connect pages call).
const OWN_SCOPES: &[&str] = &[
    "playlist-read-private",
    "playlist-read-collaborative",
    "user-library-read",
    "user-library-modify",
    "user-read-recently-played",
    "user-read-private",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-follow-read",
];

/// Renew a little before expiry so a request never goes out with a dying token.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

const BROWSER_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Tonearm</title></head>\
<body style=\"background:#121212;color:#eee;font-family:sans-serif;text-align:center;padding-top:20vh\">\
<h2>Доступ к библиотеке разрешён</h2><p>Можно закрыть вкладку и вернуться в Tonearm.</p></body></html>";

struct OwnApp {
    client_id: String,
    store: Arc<dyn SecretStore>,
    cached: Mutex<Option<(Secret, Instant)>>,
    /// One refresh / browser login at a time.
    renew: tokio::sync::Mutex<()>,
}

static OWN_APP: OnceLock<OwnApp> = OnceLock::new();

/// Uses the user's own Spotify app for Web API tokens. Empty `client_id` keeps login5.
/// Call once at startup; later calls are ignored.
pub fn use_own_client_id(client_id: &str, store: Arc<dyn SecretStore>) {
    let client_id = client_id.trim();
    if client_id.is_empty() {
        return;
    }
    tracing::info!("Web API: own client id from config");
    let _ = OWN_APP.set(OwnApp {
        client_id: client_id.to_owned(),
        store,
        cached: Mutex::new(None),
        renew: tokio::sync::Mutex::new(()),
    });
}

/// Drops the own app's tokens (logout).
pub fn forget_own_token() {
    if let Some(app) = OWN_APP.get() {
        if let Ok(mut c) = app.cached.lock() {
            *c = None;
        }
        let store = Arc::clone(&app.store);
        let _ = std::thread::spawn(move || store.delete(WEBAPI_REFRESH_KEY));
    }
}

/// A bearer token for the Web API.
pub(crate) async fn access_token(session: &Session) -> Result<Secret, ApiError> {
    if let Some(app) = OWN_APP.get() {
        return app.token().await;
    }
    // login5: Spotify retired the Mercury keymaster endpoint ("Service unavailable");
    // keymaster stays only as a fallback.
    match session.login5().auth_token().await {
        Ok(t) => Ok(Secret::new(t.access_token)),
        Err(login5_err) => {
            tracing::warn!(error = %login5_err, "login5 token failed, trying keymaster");
            session
                .token_provider()
                .get_token(&OWN_SCOPES.join(","))
                .await
                .map(|t| Secret::new(t.access_token))
                .map_err(|e| ApiError::Network(format!("токен: {login5_err}; {e}")))
        }
    }
}

impl OwnApp {
    fn cached(&self) -> Option<Secret> {
        let c = self.cached.lock().ok()?;
        c.as_ref()
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(t, _)| t.clone())
    }

    fn client(&self, browser: bool) -> Result<OAuthClient, ApiError> {
        let mut b = OAuthClientBuilder::new(&self.client_id, REDIRECT_URI, OWN_SCOPES.to_vec());
        if browser {
            b = b.open_in_browser().with_custom_message(BROWSER_PAGE);
        }
        b.build()
            .map_err(|e| ApiError::Network(format!("свой Client ID: {e}")))
    }

    async fn token(&self) -> Result<Secret, ApiError> {
        if let Some(t) = self.cached() {
            return Ok(t);
        }
        let _guard = self.renew.lock().await;
        if let Some(t) = self.cached() {
            return Ok(t);
        }

        let store = Arc::clone(&self.store);
        let saved = tokio::task::spawn_blocking(move || store.get(WEBAPI_REFRESH_KEY))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();
        if let Some(refresh) = saved {
            match self
                .client(false)?
                .refresh_token_async(refresh.expose())
                .await
            {
                Ok(token) => return Ok(self.keep(token, Some(refresh)).await),
                Err(e) => tracing::warn!(error = %e, "Web API refresh failed, logging in again"),
            }
        }

        // First use of the own app: one consent page in the browser.
        let client = self.client(true)?;
        let token = tokio::task::spawn_blocking(move || client.get_access_token())
            .await
            .map_err(|e| ApiError::Network(format!("вход для Web API: {e}")))?
            .map_err(|e| ApiError::Network(format!("вход для Web API: {e}")))?;
        Ok(self.keep(token, None).await)
    }

    async fn keep(&self, token: OAuthToken, old_refresh: Option<Secret>) -> Secret {
        let access = Secret::new(token.access_token);
        let until = token
            .expires_at
            .checked_sub(EXPIRY_MARGIN)
            .unwrap_or_else(Instant::now);
        if let Ok(mut c) = self.cached.lock() {
            *c = Some((access.clone(), until));
        }
        let changed = old_refresh
            .as_ref()
            .is_none_or(|old| old.expose() != token.refresh_token);
        if !token.refresh_token.is_empty() && changed {
            let store = Arc::clone(&self.store);
            let refresh = Secret::new(token.refresh_token);
            let saved =
                tokio::task::spawn_blocking(move || store.set(WEBAPI_REFRESH_KEY, &refresh)).await;
            if !matches!(saved, Ok(Ok(()))) {
                tracing::warn!(
                    "Web API refresh token not saved; the browser will ask again next start"
                );
            }
        }
        access
    }
}
