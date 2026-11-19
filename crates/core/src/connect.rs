//! Spotify Connect, controller side: list the user's other devices (phone, speakers, the official
//! desktop app), move playback there and back, and control it remotely, all through the Web API
//! `/me/player` endpoints (still available in Development Mode after February 2026).
//!
//! Tonearm itself is not announced as a Connect target: that needs librespot's `Spirc`, which takes
//! over the player and its queue (see docs/PLAN.md).

use std::time::Duration;

use librespot_core::Session;
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::library::SessionHttp;
use crate::library::webapi::{API, ApiError, Http, Method, WebApi};

/// How often the remote device's state is polled while it plays.
const POLL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// "Computer", "Smartphone", "Speaker"…
    pub kind: String,
    pub is_active: bool,
    /// Some devices (e.g. restricted ones) ignore Web API commands.
    pub is_restricted: bool,
    pub volume_percent: Option<u8>,
}

/// What a remote device is playing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteState {
    pub device: Device,
    pub is_playing: bool,
    pub progress_ms: u32,
    pub duration_ms: u32,
    pub uri: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAction {
    Play,
    Pause,
    Next,
    Prev,
    Seek(u32),
    Volume(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectCommand {
    /// Reload the device list.
    Refresh,
    /// Start `uris` on a device at `offset` / `position_ms` (moving the local queue there).
    PlayOn {
        device_id: String,
        uris: Vec<String>,
        offset: usize,
        position_ms: u32,
    },
    /// Make a device active without choosing what plays (it continues the account's last track).
    Transfer { device_id: String },
    /// Control the remote device that plays now.
    Control(RemoteAction),
    /// Bring playback back here: pause the remote device and report where it was.
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectEvent {
    Devices(Vec<Device>),
    /// State of the remote device; `None` when it stopped or disappeared.
    Remote(Option<RemoteState>),
    /// Playback came back: resume `uri` at `position_ms` locally.
    Released {
        uri: Option<String>,
        position_ms: u32,
    },
    Error(String),
}

// ---- JSON ----

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiDevice {
    id: Option<String>,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    is_active: bool,
    is_restricted: bool,
    volume_percent: Option<u8>,
}

impl From<ApiDevice> for Device {
    fn from(d: ApiDevice) -> Self {
        Self {
            id: d.id.unwrap_or_default(),
            name: d.name,
            kind: d.kind,
            is_active: d.is_active,
            is_restricted: d.is_restricted,
            volume_percent: d.volume_percent,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Devices {
    devices: Vec<ApiDevice>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Named {
    name: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Image {
    url: String,
    width: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Album {
    name: String,
    images: Vec<Image>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Item {
    uri: String,
    name: String,
    duration_ms: u32,
    artists: Vec<Named>,
    album: Option<Album>,
    /// Episodes: the show instead of artists.
    show: Option<Named>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Playback {
    device: Option<ApiDevice>,
    is_playing: bool,
    progress_ms: Option<u32>,
    item: Option<Item>,
}

fn ok(status: u16) -> Result<(), ApiError> {
    match status {
        200..=299 => Ok(()),
        401 | 403 => Err(ApiError::Forbidden(status)),
        404 => Err(ApiError::NotFound),
        429 => Err(ApiError::RateLimited),
        s => Err(ApiError::Status(s)),
    }
}

fn encode(s: &str) -> String {
    reqwest::Url::parse_with_params("http://x/", [("v", s)])
        .ok()
        .and_then(|u| u.query().map(|q| q.trim_start_matches("v=").to_owned()))
        .unwrap_or_default()
}

impl<H: Http> WebApi<H> {
    pub async fn devices(&self) -> Result<Vec<Device>, ApiError> {
        let d: Devices = self.get_json(&format!("{API}/me/player/devices")).await?;
        Ok(d.devices
            .into_iter()
            .map(Device::from)
            .filter(|d| !d.id.is_empty())
            .collect())
    }

    /// Current playback anywhere on the account; `None` when nothing is active (HTTP 204).
    pub async fn playback(&self) -> Result<Option<RemoteState>, ApiError> {
        let resp = self.http.get(&format!("{API}/me/player")).await?;
        ok(resp.status)?;
        if resp.status == 204 || resp.body.is_empty() {
            return Ok(None);
        }
        let p: Playback = serde_json::from_slice(&resp.body).map_err(|_| ApiError::Parse)?;
        let Some(device) = p.device else {
            return Ok(None);
        };
        let item = p.item.unwrap_or_default();
        let album = item.album.unwrap_or_default();
        Ok(Some(RemoteState {
            device: device.into(),
            is_playing: p.is_playing,
            progress_ms: p.progress_ms.unwrap_or(0),
            duration_ms: item.duration_ms,
            uri: item.uri,
            title: item.name,
            artists: if item.artists.is_empty() {
                item.show.map(|s| s.name).unwrap_or_default()
            } else {
                item.artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            cover_url: album
                .images
                .iter()
                .min_by_key(|i| i.width.unwrap_or(300).abs_diff(300))
                .map(|i| i.url.clone()),
            album: album.name,
        }))
    }

    async fn command(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<(), ApiError> {
        let resp = self
            .http
            .send(method, &format!("{API}/me/player{path}"), body)
            .await?;
        ok(resp.status)
    }

    pub async fn play_on(
        &self,
        device_id: &str,
        uris: &[String],
        offset: usize,
        position_ms: u32,
    ) -> Result<(), ApiError> {
        let body = serde_json::json!({
            "uris": uris,
            "offset": { "position": offset },
            "position_ms": position_ms,
        });
        self.command(
            Method::Put,
            &format!("/play?device_id={}", encode(device_id)),
            Some(body.to_string()),
        )
        .await
    }

    pub async fn transfer(&self, device_id: &str, play: bool) -> Result<(), ApiError> {
        let body = serde_json::json!({ "device_ids": [device_id], "play": play });
        self.command(Method::Put, "", Some(body.to_string())).await
    }

    pub async fn control(&self, action: &RemoteAction) -> Result<(), ApiError> {
        match action {
            RemoteAction::Play => self.command(Method::Put, "/play", None).await,
            RemoteAction::Pause => self.command(Method::Put, "/pause", None).await,
            RemoteAction::Next => self.command(Method::Post, "/next", None).await,
            RemoteAction::Prev => self.command(Method::Post, "/previous", None).await,
            RemoteAction::Seek(ms) => {
                self.command(Method::Put, &format!("/seek?position_ms={ms}"), None)
                    .await
            }
            RemoteAction::Volume(p) => {
                let p = (*p).min(100);
                self.command(Method::Put, &format!("/volume?volume_percent={p}"), None)
                    .await
            }
        }
    }
}

/// Spotify can only hold this many URIs in one `play` request in practice; longer queues are cut.
pub const MAX_URIS: usize = 100;

struct Service {
    events: mpsc::Sender<ConnectEvent>,
    session: Option<Session>,
    /// Polls the remote device while playback is there.
    poll: Option<JoinHandle<()>>,
}

pub async fn run(
    mut commands: mpsc::Receiver<ConnectCommand>,
    events: mpsc::Sender<ConnectEvent>,
    mut sessions: watch::Receiver<Option<Session>>,
) {
    let mut svc = Service {
        events,
        session: sessions.borrow_and_update().clone(),
        poll: None,
    };
    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                Some(cmd) => svc.handle(cmd).await,
                None => break,
            },
            changed = sessions.changed() => {
                if changed.is_err() {
                    break;
                }
                svc.session = sessions.borrow_and_update().clone();
                if svc.session.is_none() {
                    svc.stop_polling();
                }
            }
        }
    }
    svc.stop_polling();
}

impl Service {
    fn api(&self) -> Option<WebApi<SessionHttp>> {
        self.session
            .clone()
            .map(|s| WebApi::new(SessionHttp::new(s)))
    }

    async fn emit(&self, ev: ConnectEvent) {
        let _ = self.events.send(ev).await;
    }

    fn stop_polling(&mut self) {
        if let Some(t) = self.poll.take() {
            t.abort();
        }
    }

    fn start_polling(&mut self) {
        self.stop_polling();
        let Some(api) = self.api() else { return };
        let events = self.events.clone();
        self.poll = Some(tokio::spawn(async move {
            loop {
                match api.playback().await {
                    Ok(state) => {
                        if events.send(ConnectEvent::Remote(state)).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => tracing::debug!(%e, "remote playback poll failed"),
                }
                tokio::time::sleep(POLL).await;
            }
        }));
    }

    async fn handle(&mut self, cmd: ConnectCommand) {
        let Some(api) = self.api() else {
            return self
                .emit(ConnectEvent::Error("Нет подключения к Spotify".into()))
                .await;
        };
        let result = match cmd {
            ConnectCommand::Refresh => api.devices().await.map(|d| Some(ConnectEvent::Devices(d))),
            ConnectCommand::PlayOn {
                device_id,
                uris,
                offset,
                position_ms,
            } => {
                // A window of the queue that contains the current track.
                let from = offset.saturating_sub(MAX_URIS / 2).min(uris.len());
                let to = (from + MAX_URIS).min(uris.len());
                let r = api
                    .play_on(&device_id, &uris[from..to], offset - from, position_ms)
                    .await;
                if r.is_ok() {
                    self.start_polling();
                }
                r.map(|()| None)
            }
            ConnectCommand::Transfer { device_id } => {
                let r = api.transfer(&device_id, true).await;
                if r.is_ok() {
                    self.start_polling();
                }
                r.map(|()| None)
            }
            ConnectCommand::Control(action) => {
                let r = api.control(&action).await;
                // Show the effect without waiting for the next poll.
                if r.is_ok() && self.poll.is_some() {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if let Ok(state) = api.playback().await {
                        self.emit(ConnectEvent::Remote(state)).await;
                    }
                }
                r.map(|()| None)
            }
            ConnectCommand::Release => {
                self.stop_polling();
                let state = api.playback().await.ok().flatten();
                if state.as_ref().is_some_and(|s| s.is_playing) {
                    let _ = api.control(&RemoteAction::Pause).await;
                }
                Ok(Some(ConnectEvent::Released {
                    position_ms: state.as_ref().map_or(0, |s| s.progress_ms),
                    uri: state.map(|s| s.uri).filter(|u| !u.is_empty()),
                }))
            }
        };
        match result {
            Ok(Some(ev)) => self.emit(ev).await,
            Ok(None) => {}
            Err(e) => {
                self.emit(ConnectEvent::Error(format!("Spotify Connect: {e}")))
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::webapi::HttpResponse;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        get_body: Option<(u16, String)>,
        sent: Mutex<Vec<(Method, String, Option<String>)>>,
    }

    impl Http for Fake {
        async fn get(&self, _url: &str) -> Result<HttpResponse, ApiError> {
            let (status, body) = self.get_body.clone().unwrap_or((204, String::new()));
            Ok(HttpResponse {
                status,
                body: body.into_bytes(),
                retry_after: None,
            })
        }

        async fn send(
            &self,
            method: Method,
            url: &str,
            body: Option<String>,
        ) -> Result<HttpResponse, ApiError> {
            self.sent
                .lock()
                .unwrap()
                .push((method, url.to_owned(), body));
            Ok(HttpResponse {
                status: 204,
                body: Vec::new(),
                retry_after: None,
            })
        }
    }

    #[tokio::test]
    async fn library_save_and_remove() {
        let api = WebApi::new(Fake::default());
        let uris = vec!["spotify:track:1".to_owned(), "spotify:album:2".to_owned()];
        api.set_saved(&uris, true).await.unwrap();
        api.set_saved(&uris[..1], false).await.unwrap();
        api.set_saved(&[], true).await.unwrap();
        let sent = api.http.sent.lock().unwrap();
        assert_eq!(sent.len(), 2, "empty list sends nothing");
        assert_eq!(sent[0].0, Method::Put);
        assert_eq!(
            sent[0].1,
            format!("{API}/me/library?uris=spotify%3Atrack%3A1%2Cspotify%3Aalbum%3A2")
        );
        assert_eq!(sent[1].0, Method::Delete);
        assert!(sent[1].2.is_none());
    }

    #[tokio::test]
    async fn devices_and_playback_parse() {
        let api = WebApi::new(Fake {
            get_body: Some((
                200,
                r#"{"devices":[{"id":"d1","name":"Кухня","type":"Speaker","is_active":true,"volume_percent":40},
                               {"id":null,"name":"restricted"}]}"#
                    .into(),
            )),
            ..Fake::default()
        });
        let d = api.devices().await.unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].volume_percent, Some(40));

        let api = WebApi::new(Fake {
            get_body: Some((
                200,
                r#"{"device":{"id":"d1","name":"Кухня","type":"Speaker"},"is_playing":true,"progress_ms":5000,
                    "item":{"uri":"spotify:track:1","name":"Огни","duration_ms":9000,
                            "artists":[{"name":"А"},{"name":"Б"}],"album":{"name":"Альбом","images":[{"url":"u","width":300}]}}}"#
                    .into(),
            )),
            ..Fake::default()
        });
        let s = api.playback().await.unwrap().unwrap();
        assert_eq!(s.device.name, "Кухня");
        assert_eq!(s.artists, "А, Б");
        assert_eq!(s.cover_url.as_deref(), Some("u"));

        let idle = WebApi::new(Fake::default());
        assert_eq!(idle.playback().await.unwrap(), None);
    }

    #[tokio::test]
    async fn commands_hit_the_right_endpoints() {
        let api = WebApi::new(Fake::default());
        api.play_on("a b", &["spotify:track:1".into()], 0, 1500)
            .await
            .unwrap();
        api.control(&RemoteAction::Next).await.unwrap();
        api.control(&RemoteAction::Volume(150)).await.unwrap();
        let sent = api.http.sent.lock().unwrap();
        assert_eq!(sent[0].0, Method::Put);
        assert_eq!(sent[0].1, format!("{API}/me/player/play?device_id=a+b"));
        let body: serde_json::Value = serde_json::from_str(sent[0].2.as_ref().unwrap()).unwrap();
        assert_eq!(body["position_ms"], 1500);
        assert_eq!(body["offset"]["position"], 0);
        assert_eq!(
            sent[1],
            (Method::Post, format!("{API}/me/player/next"), None)
        );
        assert!(sent[2].1.ends_with("volume?volume_percent=100"));
    }
}
