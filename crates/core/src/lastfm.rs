//! Last.fm scrobbling: sign-in in the browser (desktop auth flow), "now playing" and scrobbles,
//! with a queue on disk for plays made offline.
//!
//! The API account (key + shared secret) is built into release builds from the
//! `TONEARM_LASTFM_KEY` / `TONEARM_LASTFM_SECRET` environment variables at compile time, or
//! comes from `[lastfm]` in `config.toml`. It is never in the repository. The user's session key
//! lives in the keyring (`lastfm-session`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::LastFmSettings;
use crate::secret::{Secret, SecretStore};

const API: &str = "https://ws.audioscrobbler.com/2.0/";
const AUTH_PAGE: &str = "https://www.last.fm/api/auth/";
/// Keyring entry: `name\nkey` of the signed-in user.
pub const SESSION_KEY: &str = "lastfm-session";
/// Last.fm drops scrobbles older than two weeks.
const MAX_AGE_SECS: u64 = 14 * 24 * 3600;
const BATCH: usize = 50;
const MAX_QUEUE: usize = 5000;
const RETRY: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_secs(3);
/// The browser approval waits this long (Last.fm tokens live an hour).
const POLL_TRIES: u32 = 200;

/// The application's API account.
#[derive(Clone)]
pub struct Keys {
    pub api_key: String,
    secret: Secret,
}

impl Keys {
    /// Own keys from the config, else the ones built in. `None`: this build cannot scrobble.
    #[must_use]
    pub fn resolve(settings: &LastFmSettings) -> Option<Self> {
        let (key, secret) = if settings.api_key.trim().is_empty() {
            (
                option_env!("TONEARM_LASTFM_KEY").unwrap_or(""),
                option_env!("TONEARM_LASTFM_SECRET").unwrap_or(""),
            )
        } else {
            (settings.api_key.trim(), settings.api_secret.trim())
        };
        (!key.is_empty() && !secret.is_empty()).then(|| Self {
            api_key: key.to_owned(),
            secret: Secret::new(secret),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Track {
    pub artist: String,
    pub title: String,
    pub album: String,
    pub duration_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Scrobble {
    pub artist: String,
    pub title: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub duration_secs: u32,
    /// Unix seconds when the track started.
    pub timestamp: u64,
}

/// Last.fm's rule: a track longer than 30 s counts after half of it or 4 minutes.
#[must_use]
pub fn counts(duration_ms: u32, listened_ms: u32) -> bool {
    duration_ms > 30_000 && listened_ms >= (duration_ms / 2).min(240_000)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastFmCommand {
    Login,
    CancelLogin,
    Logout,
    SetEnabled(bool),
    NowPlaying(Track),
    Scrobble(Scrobble),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastFmState {
    /// No API account in this build.
    Unavailable,
    SignedOut,
    /// The approval page is open in the browser.
    Waiting,
    SignedIn {
        user: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastFmEvent {
    State(LastFmState),
    /// Open this page in the browser (the approval).
    OpenUrl(String),
    /// Plays waiting to be sent.
    Pending(usize),
    Error(String),
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("нет связи с Last.fm")]
    Network,
    #[error("Last.fm: {message} ({code})")]
    Api { code: i64, message: String },
    #[error("непонятный ответ Last.fm")]
    Format,
}

impl Error {
    /// Worth sending again later.
    fn temporary(&self) -> bool {
        match self {
            Self::Network | Self::Format => true,
            // 8 operation failed, 11 offline, 16 temporary, 29 rate limit
            Self::Api { code, .. } => matches!(code, 8 | 11 | 16 | 29),
        }
    }
}

// ───── MD5 (for the API signature; nothing else needs it, so no extra crate)

#[must_use]
pub fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let k: [u32; 64] = std::array::from_fn(|i| {
        #[allow(clippy::cast_precision_loss)]
        let v = ((i + 1) as f64).sin().abs() * 4_294_967_296.0;
        v as u32
    });
    let mut h: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());
    for chunk in msg.as_chunks::<64>().0 {
        let words = chunk.as_chunks::<4>().0;
        let m: [u32; 16] = std::array::from_fn(|i| u32::from_le_bytes(words[i]));
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `api_sig`: parameters sorted by name, `name value` pairs joined, the secret appended, MD5.
#[must_use]
pub fn signature(params: &[(String, String)], secret: &str) -> String {
    let mut sorted: Vec<&(String, String)> = params
        .iter()
        .filter(|(k, _)| k != "format" && k != "callback")
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut text = String::new();
    for (k, v) in sorted {
        text.push_str(k);
        text.push_str(v);
    }
    text.push_str(secret);
    hex(&md5(text.as_bytes()))
}

fn encode(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

struct Client {
    http: reqwest::Client,
    keys: Keys,
}

#[derive(Deserialize)]
struct ApiError {
    error: i64,
    #[serde(default)]
    message: String,
}

impl Client {
    fn new(keys: Keys) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent(concat!("Tonearm/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            keys,
        }
    }

    /// A signed POST call; returns the JSON body.
    async fn call(
        &self,
        method: &str,
        mut params: Vec<(String, String)>,
    ) -> Result<serde_json::Value, Error> {
        params.push(("method".into(), method.into()));
        params.push(("api_key".into(), self.keys.api_key.clone()));
        let sig = signature(&params, self.keys.secret.expose());
        params.push(("api_sig".into(), sig));
        params.push(("format".into(), "json".into()));
        let body = params
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let resp = self
            .http
            .post(API)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| Error::Network)?;
        let bytes = resp.bytes().await.map_err(|_| Error::Network)?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| Error::Format)?;
        if let Ok(e) = serde_json::from_value::<ApiError>(json.clone()) {
            return Err(Error::Api {
                code: e.error,
                message: e.message,
            });
        }
        Ok(json)
    }

    async fn token(&self) -> Result<String, Error> {
        let v = self.call("auth.getToken", Vec::new()).await?;
        v["token"].as_str().map(str::to_owned).ok_or(Error::Format)
    }

    async fn session(&self, token: &str) -> Result<(String, String), Error> {
        let v = self
            .call("auth.getSession", vec![("token".into(), token.into())])
            .await?;
        let s = &v["session"];
        match (s["name"].as_str(), s["key"].as_str()) {
            (Some(n), Some(k)) => Ok((n.to_owned(), k.to_owned())),
            _ => Err(Error::Format),
        }
    }

    async fn now_playing(&self, sk: &str, t: &Track) -> Result<(), Error> {
        let mut p = vec![
            ("artist".into(), t.artist.clone()),
            ("track".into(), t.title.clone()),
            ("sk".into(), sk.into()),
        ];
        if !t.album.is_empty() {
            p.push(("album".into(), t.album.clone()));
        }
        if t.duration_secs > 0 {
            p.push(("duration".into(), t.duration_secs.to_string()));
        }
        self.call("track.updateNowPlaying", p).await.map(|_| ())
    }

    async fn scrobble(&self, sk: &str, batch: &[Scrobble]) -> Result<(), Error> {
        let mut p: Vec<(String, String)> = vec![("sk".into(), sk.into())];
        for (i, s) in batch.iter().enumerate() {
            p.push((format!("artist[{i}]"), s.artist.clone()));
            p.push((format!("track[{i}]"), s.title.clone()));
            p.push((format!("timestamp[{i}]"), s.timestamp.to_string()));
            if !s.album.is_empty() {
                p.push((format!("album[{i}]"), s.album.clone()));
            }
            if s.duration_secs > 0 {
                p.push((format!("duration[{i}]"), s.duration_secs.to_string()));
            }
        }
        self.call("track.scrobble", p).await.map(|_| ())
    }
}

/// Plays not sent yet, kept on disk across restarts.
pub struct Queue {
    path: Option<PathBuf>,
    items: Vec<Scrobble>,
}

impl Queue {
    #[must_use]
    pub fn load(path: Option<PathBuf>) -> Self {
        let items = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, items }
    }

    fn save(&self) {
        let Some(p) = &self.path else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(data) = serde_json::to_vec(&self.items) {
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, data).is_ok() {
                let _ = std::fs::rename(&tmp, p);
            }
        }
    }

    pub fn push(&mut self, s: Scrobble) {
        self.items.push(s);
        if self.items.len() > MAX_QUEUE {
            let extra = self.items.len() - MAX_QUEUE;
            self.items.drain(..extra);
        }
        self.save();
    }

    /// Drops what Last.fm would refuse as too old; returns the next batch.
    pub fn batch(&mut self, now: u64) -> Vec<Scrobble> {
        let before = self.items.len();
        self.items
            .retain(|s| s.timestamp + MAX_AGE_SECS > now && !s.artist.is_empty());
        if self.items.len() != before {
            self.save();
        }
        self.items.iter().take(BATCH).cloned().collect()
    }

    pub fn done(&mut self, n: usize) {
        self.items.drain(..n.min(self.items.len()));
        self.save();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `scrobbles.json` in the data dir.
#[must_use]
pub fn queue_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "tonearm").map(|d| d.data_dir().join("scrobbles.json"))
}

struct Service {
    client: Option<Arc<Client>>,
    store: Arc<dyn SecretStore>,
    events: mpsc::Sender<LastFmEvent>,
    enabled: bool,
    /// (user, session key)
    session: Option<(String, Secret)>,
    queue: Queue,
    login: Option<JoinHandle<()>>,
}

impl Service {
    async fn emit(&self, ev: LastFmEvent) {
        let _ = self.events.send(ev).await;
    }

    fn state(&self) -> LastFmState {
        match (&self.client, &self.session, &self.login) {
            (None, _, _) => LastFmState::Unavailable,
            (_, Some((user, _)), _) => LastFmState::SignedIn { user: user.clone() },
            (_, None, Some(_)) => LastFmState::Waiting,
            _ => LastFmState::SignedOut,
        }
    }

    async fn publish(&self) {
        self.emit(LastFmEvent::State(self.state())).await;
        self.emit(LastFmEvent::Pending(self.queue.len())).await;
    }

    async fn store_session(&mut self, user: String, key: String) {
        let store = Arc::clone(&self.store);
        let value = Secret::new(format!("{user}\n{key}"));
        let saved = tokio::task::spawn_blocking(move || store.set(SESSION_KEY, &value)).await;
        if !matches!(saved, Ok(Ok(()))) {
            tracing::warn!("cannot save the Last.fm session");
            self.emit(LastFmEvent::Error(
                "Вход в Last.fm не сохранился: хранилище паролей недоступно".into(),
            ))
            .await;
        }
        self.session = Some((user, Secret::new(key)));
    }

    async fn forget_session(&mut self) {
        self.session = None;
        let store = Arc::clone(&self.store);
        let _ = tokio::task::spawn_blocking(move || store.delete(SESSION_KEY)).await;
    }

    /// Sends what is queued, batch by batch, until done or a failure.
    async fn flush(&mut self) {
        let (Some(client), true) = (self.client.clone(), self.enabled) else {
            return;
        };
        loop {
            let Some((_, sk)) = self.session.clone() else {
                return;
            };
            let batch = self.queue.batch(now_secs());
            if batch.is_empty() {
                break;
            }
            match client.scrobble(sk.expose(), &batch).await {
                Ok(()) => self.queue.done(batch.len()),
                Err(Error::Api { code: 9, .. }) => {
                    self.forget_session().await;
                    self.emit(LastFmEvent::Error(
                        "Last.fm отозвал вход, войдите снова в настройках".into(),
                    ))
                    .await;
                    break;
                }
                Err(e) if e.temporary() => {
                    tracing::info!(error = %e, "scrobbles stay queued");
                    break;
                }
                Err(e) => {
                    // Refused for good (bad data): do not block the rest.
                    tracing::warn!(error = %e, n = batch.len(), "scrobbles refused");
                    self.queue.done(batch.len());
                }
            }
        }
        self.emit(LastFmEvent::Pending(self.queue.len())).await;
    }
}

/// Runs the Last.fm side until `commands` closes.
pub async fn run(
    settings: LastFmSettings,
    store: Arc<dyn SecretStore>,
    mut commands: mpsc::Receiver<LastFmCommand>,
    events: mpsc::Sender<LastFmEvent>,
) {
    let client = Keys::resolve(&settings).map(|k| Arc::new(Client::new(k)));
    let saved = {
        let store = Arc::clone(&store);
        tokio::task::spawn_blocking(move || store.get(SESSION_KEY).ok().flatten())
            .await
            .ok()
            .flatten()
    };
    let session = saved.and_then(|s| {
        let (user, key) = s.expose().split_once('\n')?;
        Some((user.to_owned(), Secret::new(key)))
    });
    let queue = tokio::task::spawn_blocking(|| Queue::load(queue_path()))
        .await
        .unwrap_or_else(|_| Queue::load(None));
    let mut svc = Service {
        client,
        store,
        events,
        enabled: settings.scrobble,
        session,
        queue,
        login: None,
    };
    let (login_tx, mut login_rx) = mpsc::channel::<Result<(String, String), String>>(1);
    svc.publish().await;
    svc.flush().await;
    let mut retry = tokio::time::interval(RETRY);
    retry.tick().await;
    loop {
        tokio::select! {
            cmd = commands.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    LastFmCommand::Login => {
                        let Some(client) = svc.client.clone() else {
                            svc.publish().await;
                            continue;
                        };
                        if let Some(t) = svc.login.take() {
                            t.abort();
                        }
                        let events = svc.events.clone();
                        let tx = login_tx.clone();
                        svc.login = Some(tokio::spawn(async move {
                            let result = login(&client, &events).await;
                            let _ = tx.send(result).await;
                        }));
                        svc.publish().await;
                    }
                    LastFmCommand::CancelLogin => {
                        if let Some(t) = svc.login.take() {
                            t.abort();
                        }
                        svc.publish().await;
                    }
                    LastFmCommand::Logout => {
                        if let Some(t) = svc.login.take() {
                            t.abort();
                        }
                        svc.forget_session().await;
                        svc.publish().await;
                    }
                    LastFmCommand::SetEnabled(on) => {
                        svc.enabled = on;
                        if on {
                            svc.flush().await;
                        }
                    }
                    LastFmCommand::NowPlaying(track) => {
                        if let (Some(c), Some((_, sk)), true) = (&svc.client, &svc.session, svc.enabled)
                            && !track.artist.is_empty()
                        {
                            let (c, sk) = (Arc::clone(c), sk.clone());
                            // Fire and forget: "now playing" is not worth a retry.
                            tokio::spawn(async move {
                                if let Err(e) = c.now_playing(sk.expose(), &track).await {
                                    tracing::debug!(error = %e, "now playing not sent");
                                }
                            });
                        }
                    }
                    LastFmCommand::Scrobble(s) => {
                        if svc.client.is_some() && svc.session.is_some() && svc.enabled {
                            svc.queue.push(s);
                            svc.flush().await;
                        }
                    }
                }
            }
            result = login_rx.recv() => {
                svc.login = None;
                match result {
                    Some(Ok((user, key))) => {
                        svc.store_session(user, key).await;
                        svc.publish().await;
                        svc.flush().await;
                    }
                    Some(Err(message)) => {
                        svc.publish().await;
                        svc.emit(LastFmEvent::Error(message)).await;
                    }
                    None => {}
                }
            }
            _ = retry.tick() => {
                if !svc.queue.is_empty() {
                    svc.flush().await;
                }
            }
        }
    }
    if let Some(t) = svc.login.take() {
        t.abort();
    }
}

/// Desktop auth: a token, the approval page in the browser, then the session once approved.
async fn login(
    client: &Client,
    events: &mpsc::Sender<LastFmEvent>,
) -> Result<(String, String), String> {
    let token = client.token().await.map_err(|e| e.to_string())?;
    let url = format!(
        "{AUTH_PAGE}?api_key={}&token={}",
        encode(&client.keys.api_key),
        encode(&token)
    );
    let _ = events.send(LastFmEvent::OpenUrl(url)).await;
    for _ in 0..POLL_TRIES {
        tokio::time::sleep(POLL).await;
        match client.session(&token).await {
            Ok(s) => return Ok(s),
            // 14: not approved yet; network hiccups: keep waiting.
            Err(Error::Api { code: 14, .. } | Error::Network) => {}
            Err(Error::Api { code: 15, .. }) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("Вход в Last.fm не подтвердили вовремя, попробуйте ещё раз".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_rfc_vectors() {
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(b"The quick brown fox jumps over the lazy dog")),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        let long = "1234567890".repeat(8);
        assert_eq!(
            hex(&md5(long.as_bytes())),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn signature_sorts_and_skips_format() {
        let p = vec![
            ("method".to_owned(), "auth.getSession".to_owned()),
            ("api_key".to_owned(), "KEY".to_owned()),
            ("token".to_owned(), "TOK".to_owned()),
            ("format".to_owned(), "json".to_owned()),
        ];
        let expected = hex(&md5(b"api_keyKEYmethodauth.getSessiontokenTOKsecret"));
        assert_eq!(signature(&p, "secret"), expected);
    }

    #[test]
    fn form_encoding() {
        assert_eq!(encode("Мир & co/1"), "%D0%9C%D0%B8%D1%80%20%26%20co%2F1");
        assert_eq!(encode("artist[0]"), "artist%5B0%5D");
    }

    #[test]
    fn scrobble_rule() {
        assert!(!counts(25_000, 25_000));
        assert!(!counts(200_000, 99_000));
        assert!(counts(200_000, 100_000));
        // Long tracks: 4 minutes are enough.
        assert!(counts(1_200_000, 240_000));
    }

    #[test]
    fn queue_persists_and_drops_old_plays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.json");
        let now = 10_000_000;
        let mut q = Queue::load(Some(path.clone()));
        let s = |t: u64| Scrobble {
            artist: "A".into(),
            title: "T".into(),
            timestamp: t,
            ..Scrobble::default()
        };
        q.push(s(now - MAX_AGE_SECS - 1));
        for i in 0..60 {
            q.push(s(now - 100 + i));
        }
        let mut q = Queue::load(Some(path));
        assert_eq!(q.len(), 61);
        let b = q.batch(now);
        assert_eq!(q.len(), 60);
        assert_eq!(b.len(), BATCH);
        assert_eq!(b[0].timestamp, now - 100);
        q.done(b.len());
        assert_eq!(q.batch(now).len(), 10);
    }

    #[test]
    fn no_keys_no_client() {
        let s = LastFmSettings {
            api_key: "k".into(),
            api_secret: String::new(),
            ..LastFmSettings::default()
        };
        assert!(Keys::resolve(&s).is_none());
        let s = LastFmSettings {
            api_key: "k".into(),
            api_secret: "s".into(),
            ..LastFmSettings::default()
        };
        assert_eq!(Keys::resolve(&s).unwrap().api_key, "k");
    }
}
