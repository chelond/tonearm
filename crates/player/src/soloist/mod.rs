//! Spotify Soloist as the playback engine: Spotify's own headless player, controlled over its
//! loopback WebSocket API. Tonearm downloads it from Spotify, starts it as a child process,
//! signs it in with the account Tonearm already uses, and restarts it if it stops.

pub mod install;
pub mod pair;
pub mod process;

use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use librespot_core::Session;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::TrackInfo;

/// Spotify's page where each listener creates their own Soloist key.
pub const KEY_PAGE: &str = "https://developer.spotify.com/dashboard/soloist";
/// The Spotify Connect name of Tonearm's Soloist.
pub const DEVICE_NAME: &str = "Tonearm";
/// Restarts after Soloist stops on its own before giving up.
const RESTARTS: [u64; 3] = [2, 10, 30];

/// What the user needs to start Soloist.
#[derive(Clone, PartialEq, Eq)]
pub struct SoloistConfig {
    pub api_key: String,
    /// Where builds, Soloist's state and its log live.
    pub root: PathBuf,
    pub cache_dir: PathBuf,
    /// The Spotify Connect name.
    pub device_name: String,
}

impl std::fmt::Debug for SoloistConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoloistConfig")
            .field("api_key", &"***")
            .field("root", &self.root)
            .field("device_name", &self.device_name)
            .finish_non_exhaustive()
    }
}

/// Soloist's state, for the settings page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoloistStatus {
    Off,
    Downloading(u8),
    Starting,
    /// Signing in with Tonearm's account.
    Pairing,
    /// Automatic sign-in failed: pick the device once in a Spotify app.
    NeedsPairing {
        reason: String,
    },
    Ready {
        version: String,
        days_left: u64,
    },
    Failed(String),
}

/// From the Soloist task to the player service.
#[derive(Debug, Clone)]
pub(crate) enum Event {
    Status(SoloistStatus),
    /// A `playback_state` snapshot.
    Snapshot(Snapshot),
    Track(Option<TrackInfo>),
    Position(u32),
    Playing(String),
    Volume(u8),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Snapshot {
    pub status: String,
    pub track: Option<TrackInfo>,
    pub position_ms: u32,
    pub volume: Option<u8>,
    pub active: bool,
}

enum Ctl {
    Command(Value),
    Pair,
    Stop,
}

/// The running Soloist task.
pub(crate) struct Handle {
    tx: mpsc::UnboundedSender<Ctl>,
    task: tokio::task::JoinHandle<()>,
    pub config: SoloistConfig,
}

impl Handle {
    pub fn start(
        config: SoloistConfig,
        volume: u8,
        sessions: watch::Receiver<Option<Session>>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(supervise(config.clone(), volume, sessions, events, rx));
        Self { tx, task, config }
    }

    pub fn send(&self, command: Value) {
        let _ = self.tx.send(Ctl::Command(command));
    }

    pub fn pair(&self) {
        let _ = self.tx.send(Ctl::Pair);
    }

    pub async fn stop(self) {
        let _ = self.tx.send(Ctl::Stop);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

async fn supervise(
    cfg: SoloistConfig,
    volume: u8,
    sessions: watch::Receiver<Option<Session>>,
    events: mpsc::UnboundedSender<Event>,
    mut ctl: mpsc::UnboundedReceiver<Ctl>,
) {
    let status = |s: SoloistStatus| {
        let _ = events.send(Event::Status(s));
    };
    let mut restarts = 0usize;
    loop {
        let build = match ensure_build(&cfg, &events).await {
            Ok(b) => b,
            Err(e) => {
                status(SoloistStatus::Failed(e.to_string()));
                if !wait_for_retry(&mut ctl).await {
                    return;
                }
                continue;
            }
        };
        install::collect(&cfg.root, &build).await;
        if build.renew_due(install::now()) {
            let root = cfg.root.clone();
            let b = build.clone();
            tokio::spawn(async move {
                match install::download(&root, Some(&b), |_| {}).await {
                    Ok(Some(new)) => tracing::info!(
                        version = new.version,
                        "newer soloist downloaded, used from next start"
                    ),
                    Ok(None) => install::mark_checked(&root, &b).await,
                    Err(e) => {
                        tracing::warn!(error = %e, "soloist update check failed");
                        install::mark_checked(&root, &b).await;
                    }
                }
            });
        }

        status(SoloistStatus::Starting);
        let exe = build.exe(&cfg.root);
        let args = process::Args {
            exe: &exe,
            device_name: &cfg.device_name,
            api_key: &cfg.api_key,
            data_dir: cfg.root.join("data"),
            cache_dir: cfg.cache_dir.clone(),
            volume,
        };
        let spawned = match process::spawn(&args, cfg.root.join("soloist.log")).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "soloist did not start");
                if e.to_string().contains("устарела") {
                    let _ = tokio::fs::remove_file(cfg.root.join("current.json")).await;
                    continue;
                }
                status(SoloistStatus::Failed(format!(
                    "{e}. Проверьте ключ Soloist: он берётся на developer.spotify.com, нужен Premium."
                )));
                if !wait_for_retry(&mut ctl).await {
                    return;
                }
                continue;
            }
        };
        let ready = SoloistStatus::Ready {
            version: build.version.clone(),
            days_left: build.days_left(install::now()),
        };
        match run(spawned, &cfg, ready, &sessions, &events, &mut ctl).await {
            Outcome::Stopped => return,
            Outcome::Expired => {
                let _ = tokio::fs::remove_file(cfg.root.join("current.json")).await;
            }
            Outcome::Crashed(message) => {
                let Some(&delay) = RESTARTS.get(restarts) else {
                    status(SoloistStatus::Failed(message));
                    restarts = 0;
                    if !wait_for_retry(&mut ctl).await {
                        return;
                    }
                    continue;
                };
                restarts += 1;
                status(SoloistStatus::Failed(format!(
                    "{message}. Перезапускаю через {delay} с…"
                )));
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        }
    }
}

/// After a failure: `true` when the user asks to try again, `false` to stop.
async fn wait_for_retry(ctl: &mut mpsc::UnboundedReceiver<Ctl>) -> bool {
    loop {
        match ctl.recv().await {
            Some(Ctl::Pair) => return true,
            Some(Ctl::Command(_)) => {}
            Some(Ctl::Stop) | None => return false,
        }
    }
}

async fn ensure_build(
    cfg: &SoloistConfig,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<install::Build, install::InstallError> {
    if let Some(b) = install::current(&cfg.root).await {
        return Ok(b);
    }
    let _ = events.send(Event::Status(SoloistStatus::Downloading(0)));
    let ev = events.clone();
    let progress = move |p| {
        let _ = ev.send(Event::Status(SoloistStatus::Downloading(p)));
    };
    install::download(&cfg.root, None, progress)
        .await?
        .ok_or(install::InstallError::Archive("пустая загрузка".into()))
}

enum Outcome {
    Stopped,
    Expired,
    Crashed(String),
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(port: u16) -> Option<Socket> {
    for _ in 0..50 {
        if let Ok((socket, _)) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}")).await
        {
            return Some(socket);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

#[allow(clippy::too_many_lines)]
async fn run(
    mut spawned: process::Spawned,
    cfg: &SoloistConfig,
    ready: SoloistStatus,
    sessions: &watch::Receiver<Option<Session>>,
    events: &mpsc::UnboundedSender<Event>,
    ctl: &mut mpsc::UnboundedReceiver<Ctl>,
) -> Outcome {
    let status = |s: SoloistStatus| {
        let _ = events.send(Event::Status(s));
    };
    let Some(mut socket) = connect(spawned.ws_port).await else {
        let _ = spawned.child.kill().await;
        return Outcome::Crashed("Soloist не отвечает по WebSocket".into());
    };
    let (pair_tx, mut pair_rx) = mpsc::unbounded_channel::<Result<(), String>>();
    let mut tried_pairing = false;
    let mut logged_in = false;
    let start_pairing = |tried: &mut bool| {
        *tried = true;
        let session = sessions.borrow().clone();
        let name = cfg.device_name.clone();
        let tx = pair_tx.clone();
        tokio::spawn(async move {
            let result = match session {
                Some(s) => pair_with(&s, &name).await,
                None => Err("сначала войдите в аккаунт".into()),
            };
            let _ = tx.send(result);
        });
    };
    loop {
        tokio::select! {
            msg = socket.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t.to_string(),
                    Some(Ok(Message::Close(_)) | Err(_)) | None => {
                        // The socket dropped; the process tells whether Soloist itself is gone.
                        match tokio::time::timeout(Duration::from_secs(3), spawned.child.wait()).await {
                            Ok(Ok(st)) => return exit_outcome(st.code()),
                            _ => match connect(spawned.ws_port).await {
                                Some(s) => { socket = s; continue; }
                                None => {
                                    let _ = spawned.child.kill().await;
                                    return Outcome::Crashed("связь с Soloist потеряна".into());
                                }
                            },
                        }
                    }
                    Some(Ok(_)) => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                match v["type"].as_str().unwrap_or_default() {
                    "auth_state" => {
                        logged_in = v["logged_in"].as_bool().unwrap_or(false);
                        if logged_in {
                            status(ready.clone());
                            let _ = send(&mut socket, json!({"command": "get_state"})).await;
                        } else if !tried_pairing {
                            status(SoloistStatus::Pairing);
                            start_pairing(&mut tried_pairing);
                        }
                    }
                    "error" => {
                        tracing::warn!(message = v["message"].as_str().unwrap_or(""), "soloist refused a command");
                    }
                    _ => {
                        if let Some(ev) = parse_event(&v) {
                            let _ = events.send(ev);
                        }
                    }
                }
            }
            Some(result) = pair_rx.recv() => {
                match result {
                    Ok(()) => tracing::info!("soloist signed in from tonearm"),
                    Err(reason) if !logged_in => {
                        tracing::warn!(reason, "automatic soloist sign-in failed");
                        status(SoloistStatus::NeedsPairing { reason });
                    }
                    Err(_) => {}
                }
            }
            c = ctl.recv() => match c {
                Some(Ctl::Command(cmd)) => {
                    if send(&mut socket, cmd).await.is_err()
                        && let Some(s) = connect(spawned.ws_port).await
                    {
                        socket = s;
                    }
                }
                Some(Ctl::Pair) => {
                    if !logged_in {
                        status(SoloistStatus::Pairing);
                        start_pairing(&mut tried_pairing);
                    }
                }
                Some(Ctl::Stop) | None => {
                    let _ = socket.close(None).await;
                    let _ = spawned.child.kill().await;
                    return Outcome::Stopped;
                }
            },
            st = spawned.child.wait() => {
                return match st {
                    Ok(st) => exit_outcome(st.code()),
                    Err(e) => Outcome::Crashed(e.to_string()),
                };
            }
        }
    }
}

fn exit_outcome(code: Option<i32>) -> Outcome {
    if code == Some(process::EXIT_EXPIRED) {
        Outcome::Expired
    } else {
        Outcome::Crashed(process::exit_message(code))
    }
}

async fn send(socket: &mut Socket, mut body: Value) -> Result<(), String> {
    body["type"] = json!("command");
    socket
        .send(Message::Text(body.to_string().into()))
        .await
        .map_err(|e| e.to_string())
}

/// Signs Soloist in with the session's reusable credentials (see [`pair`]).
async fn pair_with(session: &Session, device_name: &str) -> Result<(), String> {
    let mut last = String::new();
    // mDNS answers can take a moment after Soloist starts.
    for wait in [3, 6, 10] {
        match pair::find(device_name, Duration::from_secs(wait)).await {
            Ok(endpoint) => {
                return pair::add_user(&endpoint, &session.username(), &session.auth_data())
                    .await
                    .map_err(|e| e.to_string());
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

fn parse_event(v: &Value) -> Option<Event> {
    match v["type"].as_str()? {
        "playback_state" => Some(Event::Snapshot(Snapshot {
            status: v["status"].as_str().unwrap_or("idle").to_owned(),
            track: track(&v["item"]),
            position_ms: position(&v["position"]),
            volume: v["volume"].as_u64().map(percent),
            active: v["is_active"].as_bool().unwrap_or(false),
        })),
        "track_changed" => Some(Event::Track(track(&v["item"]))),
        "position_sync" => Some(Event::Position(position(&v["position"]))),
        "playback_changed" => Some(Event::Playing(
            v["status"].as_str().unwrap_or_default().to_owned(),
        )),
        "volume_changed" => v["volume"].as_u64().map(|p| Event::Volume(percent(p))),
        _ => None,
    }
}

fn percent(v: u64) -> u8 {
    u8::try_from(v.min(100)).unwrap_or(100)
}

/// `position_ms` advanced to now by `(now - timestamp_ms) * speed`.
fn position(p: &Value) -> u32 {
    let ms = p["position_ms"].as_u64().unwrap_or(0);
    let at = p["timestamp_ms"].as_u64().unwrap_or(0);
    let speed = p["speed"].as_f64().unwrap_or(0.0).clamp(0.0, 4.0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let elapsed = if at == 0 { 0 } else { now.saturating_sub(at) };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let advanced = ms.saturating_add((elapsed as f64 * speed) as u64);
    u32::try_from(advanced).unwrap_or(u32::MAX)
}

fn track(item: &Value) -> Option<TrackInfo> {
    let uri = item["uri"].as_str()?.to_owned();
    let d = &item["decorations"];
    let name = |e: &Value| {
        e["decorations"]["identity"]["name"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    };
    let artists = d["creators"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| name(&c["entity"]))
        .filter(|n| !n.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let covers = d["visual_identity"]["cover"].as_array();
    let cover = covers.and_then(|c| {
        c.iter()
            .find(|i| i["size"] == "small")
            .or_else(|| c.first())
            .and_then(|i| i["url"].as_str())
            .map(str::to_owned)
    });
    Some(TrackInfo {
        uri,
        title: d["identity"]["name"].as_str().unwrap_or("").to_owned(),
        artists,
        album: name(&d["parent"]["entity"]),
        duration_ms: d["playback"]["duration_ms"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0),
        cover_url: cover,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_state_is_parsed() {
        let v = json!({"type":"playback_state","is_active":true,"status":"playing","volume":52,
            "position":{"position_ms":2300,"speed":0},
            "item":{"uri":"spotify:track:t","entity_type":"track","decorations":{
                "identity":{"name":"Song"},"playback":{"duration_ms":123000},
                "parent":{"entity":{"uri":"spotify:album:a","decorations":{"identity":{"name":"Album"}}}},
                "visual_identity":{"cover":[{"url":"https://i.scdn.co/image/big","size":"large"},{"url":"https://i.scdn.co/image/s","size":"small"}]},
                "creators":[{"entity":{"uri":"spotify:artist:a","decorations":{"identity":{"name":"A"}}}},
                            {"entity":{"uri":"spotify:artist:b","decorations":{"identity":{"name":"B"}}}}]}}});
        let Some(Event::Snapshot(s)) = parse_event(&v) else {
            panic!("not a snapshot")
        };
        assert_eq!(s.status, "playing");
        assert_eq!(s.position_ms, 2300);
        assert_eq!(s.volume, Some(52));
        let t = s.track.unwrap();
        assert_eq!(t.title, "Song");
        assert_eq!(t.artists, "A, B");
        assert_eq!(t.album, "Album");
        assert_eq!(t.duration_ms, 123_000);
        assert_eq!(t.cover_url.as_deref(), Some("https://i.scdn.co/image/s"));
    }

    #[test]
    fn config_debug_hides_the_key() {
        let c = SoloistConfig {
            api_key: "secret-key".into(),
            root: PathBuf::new(),
            cache_dir: PathBuf::new(),
            device_name: "Tonearm".into(),
        };
        assert!(!format!("{c:?}").contains("secret-key"));
    }
}
