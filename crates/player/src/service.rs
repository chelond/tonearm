//! Player service: owns the librespot `Player`, translates commands and events.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use librespot_core::{Session, SpotifyUri};
use librespot_metadata::audio::{AudioItem, UniqueFields};
use librespot_playback::audio_backend::{self, Sink, SinkError, SinkResult};
use librespot_playback::config::{AudioFormat, Bitrate, PlayerConfig};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::mixer::{self, Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent as LsEvent};
use tokio::sync::{mpsc, watch};

use crate::queue::{Queue, QueueItem};
use crate::soloist::{self, SoloistStatus};
use crate::{
    AudioOptions, PlaybackState, PlayerCommand, PlayerEvent, TrackInfo, percent_to_volume, uri,
    volume_to_percent,
};
use serde_json::json;
use std::time::Instant;

const POSITION_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_VOLUME_PERCENT: u8 = 70;
/// "Previous" restarts the track when it has played longer than this.
const RESTART_THRESHOLD_MS: u32 = 3000;
/// Unavailable tracks skipped in a row before giving up. Several in a row almost always means
/// Spotify refuses audio keys to this account (librespot issue #1649), not dead tracks.
const MAX_SKIPS: u32 = 3;
/// Entries sent to the queue panel.
const QUEUE_SNAPSHOT: usize = 100;

/// Used when no audio output can be opened: refuses to start, so librespot pauses instead of crashing.
struct NoDeviceSink {
    notify: mpsc::UnboundedSender<()>,
}

impl Sink for NoDeviceSink {
    fn start(&mut self) -> SinkResult<()> {
        let _ = self.notify.send(());
        Err(SinkError::NotConnected("нет аудиоустройства".into()))
    }

    fn write(&mut self, _: AudioPacket, _: &mut Converter) -> SinkResult<()> {
        Err(SinkError::NotConnected("нет аудиоустройства".into()))
    }
}

/// librespot's rodio sink `unwrap`s device errors; contain that panic. A named device that is
/// gone falls back to the default one, and no device at all to [`NoDeviceSink`].
fn build_sink(device: &str, notify: mpsc::UnboundedSender<()>) -> Box<dyn Sink> {
    let Some(builder) = audio_backend::find(None) else {
        return Box::new(NoDeviceSink { notify });
    };
    let open = |device: Option<String>| {
        std::panic::catch_unwind(AssertUnwindSafe(|| builder(device, AudioFormat::S16))).ok()
    };
    if !device.is_empty() {
        if let Some(sink) = open(Some(device.to_owned())) {
            return sink;
        }
        tracing::warn!(device, "audio output not available, using the default one");
    }
    match open(None) {
        Some(sink) => sink,
        None => {
            tracing::error!("cannot open audio output");
            Box::new(NoDeviceSink { notify })
        }
    }
}

struct Service {
    events: mpsc::Sender<PlayerEvent>,
    mixer: Arc<dyn Mixer>,
    session: Option<Session>,
    player: Option<Arc<Player>>,
    ls_events: Option<mpsc::UnboundedReceiver<LsEvent>>,
    no_device_tx: mpsc::UnboundedSender<()>,
    audio_failed: bool,
    request_id: Option<u64>,
    state: PlaybackState,
    position_ms: u32,
    current_uri: Option<String>,
    queue: Queue,
    options: AudioOptions,
    /// Settings changed: build a new librespot player before the next track.
    rebuild: bool,
    /// Unavailable tracks skipped in a row; stops a repeat-all loop over a dead playlist.
    skips: u32,
    volume: u8,
    sessions: watch::Receiver<Option<Session>>,
    solo: Solo,
}

/// Soloist as the engine (see [`soloist`]). While `handle` is set every playback command goes
/// to Soloist; librespot stays idle.
#[derive(Default)]
struct Solo {
    handle: Option<soloist::Handle>,
    events: Option<mpsc::UnboundedReceiver<soloist::Event>>,
    ready: bool,
    active: bool,
    /// We asked Soloist to play this URI and wait for it to report it.
    awaiting: Option<(String, Instant)>,
    seek_after_load: Option<u32>,
    /// The URI Soloist plays now, as we last saw it.
    playing_uri: Option<String>,
    /// Position anchor for the 500 ms ticks (Soloist reports only changes).
    anchor: Option<(u32, Instant)>,
}

/// After this long a missing confirmation of `play` no longer holds events back.
const AWAIT_LIMIT: Duration = Duration::from_secs(8);

/// Runs until the command channel closes. Sessions come from the core via `sessions`.
pub async fn run(
    mut commands: mpsc::Receiver<PlayerCommand>,
    events: mpsc::Sender<PlayerEvent>,
    mut sessions: watch::Receiver<Option<Session>>,
) {
    let solo_sessions = sessions.clone();
    let mixer = match mixer::find(None).map(|mk| mk(MixerConfig::default())) {
        Some(Ok(m)) => m,
        _ => {
            let _ = events
                .send(PlayerEvent::Error(
                    "Не удалось создать микшер громкости".into(),
                ))
                .await;
            return;
        }
    };
    let (no_device_tx, mut no_device_rx) = mpsc::unbounded_channel();
    let mut svc = Service {
        events,
        mixer,
        session: sessions.borrow_and_update().clone(),
        player: None,
        ls_events: None,
        no_device_tx,
        audio_failed: false,
        request_id: None,
        state: PlaybackState::Stopped,
        position_ms: 0,
        current_uri: None,
        queue: Queue::default(),
        options: AudioOptions::default(),
        rebuild: false,
        skips: 0,
        volume: DEFAULT_VOLUME_PERCENT,
        sessions: solo_sessions,
        solo: Solo::default(),
    };
    let mut tick = tokio::time::interval(POSITION_INTERVAL);
    svc.set_volume(DEFAULT_VOLUME_PERCENT).await;

    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                Some(cmd) => svc.handle_command(cmd).await,
                None => break,
            },
            changed = sessions.changed() => {
                if changed.is_err() {
                    break;
                }
                let session = sessions.borrow_and_update().clone();
                svc.on_session(session).await;
            }
            Some(ev) = recv_opt(&mut svc.ls_events) => svc.handle_ls_event(ev).await,
            Some(ev) = recv_opt(&mut svc.solo.events) => svc.handle_solo_event(ev).await,
            _ = tick.tick(), if svc.solo.anchor.is_some() && svc.state == PlaybackState::Playing => {
                svc.solo_tick().await;
            }
            Some(()) = no_device_rx.recv() => {
                svc.audio_failed = true;
                svc.emit(PlayerEvent::Error(
                    "Не найдено аудиоустройство. Подключите наушники или колонки и нажмите «Играть» снова.".into(),
                )).await;
            }
        }
    }
    if let Some(player) = svc.player.take() {
        player.stop();
        drop_player(player).await;
    }
    if let Some(h) = svc.solo.handle.take() {
        h.stop().await;
    }
    tracing::info!("player stopped");
}

async fn recv_opt<T>(rx: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Dropping a librespot `Player` joins its thread, so do it off the async workers.
async fn drop_player(player: Arc<Player>) {
    let _ = tokio::task::spawn_blocking(move || drop(player)).await;
}

impl Service {
    async fn emit(&self, event: PlayerEvent) {
        let _ = self.events.send(event).await;
    }

    async fn set_state(&mut self, state: PlaybackState, position_ms: u32) {
        self.state = state;
        self.position_ms = position_ms;
        self.emit(PlayerEvent::State { state, position_ms }).await;
    }

    async fn on_session(&mut self, session: Option<Session>) {
        match (&session, &self.player) {
            (Some(s), Some(player)) => player.set_session(s.clone()),
            (None, Some(player)) => {
                // Logged out or disconnected: stop and forget the player bound to the old session.
                player.stop();
                if let Some(p) = self.player.take() {
                    drop_player(p).await;
                }
                self.ls_events = None;
                self.set_state(PlaybackState::Stopped, 0).await;
            }
            _ => {}
        }
        self.session = session;
    }

    fn ensure_player(&mut self) -> Option<Arc<Player>> {
        if self.rebuild {
            self.rebuild = false;
            if let Some(old) = self.player.take() {
                old.stop();
                // Joining librespot's thread is blocking; let it go in the background.
                std::thread::spawn(move || drop(old));
            }
            self.ls_events = None;
        }
        if self.audio_failed {
            // Retry the audio device: build a fresh player.
            self.player = None;
            self.ls_events = None;
            self.audio_failed = false;
        }
        if self.player.is_none() {
            let session = self.session.clone()?;
            let config = PlayerConfig {
                bitrate: match self.options.bitrate {
                    96 => Bitrate::Bitrate96,
                    160 => Bitrate::Bitrate160,
                    _ => Bitrate::Bitrate320,
                },
                normalisation: self.options.normalisation,
                gapless: self.options.gapless,
                position_update_interval: Some(POSITION_INTERVAL),
                ..PlayerConfig::default()
            };
            let notify = self.no_device_tx.clone();
            let device = self.options.device.clone();
            let player = Player::new(config, session, self.mixer.get_soft_volume(), move || {
                build_sink(&device, notify)
            });
            self.ls_events = Some(player.get_player_event_channel());
            self.player = Some(player);
        }
        self.player.clone()
    }

    async fn handle_command(&mut self, cmd: PlayerCommand) {
        match cmd {
            PlayerCommand::Load(input) => match uri::parse_playable(&input) {
                Ok(u) => {
                    let item = QueueItem {
                        uri: u,
                        ..QueueItem::default()
                    };
                    self.queue.set_context(vec![item.clone()], 0, String::new());
                    self.play_item(&item).await;
                    self.emit_queue().await;
                }
                Err(e) => self.emit(PlayerEvent::Error(e.to_string())).await,
            },
            PlayerCommand::PlayContext {
                items,
                start,
                name,
                shuffle,
            } => {
                if let Some(on) = shuffle {
                    self.queue.set_shuffle(false);
                    self.queue.set_context(items, start, name);
                    self.queue.set_shuffle(on);
                } else {
                    self.queue.set_context(items, start, name);
                }
                if let Some(item) = self.queue.current().cloned() {
                    self.play_item(&item).await;
                }
                self.emit_queue().await;
            }
            PlayerCommand::Next => {
                match self.queue.next(false) {
                    Some(item) => self.play_item(&item).await,
                    None => self.stop().await,
                }
                self.emit_queue().await;
            }
            PlayerCommand::Prev => {
                if self.position_ms > RESTART_THRESHOLD_MS && self.solo.handle.is_some() {
                    self.solo_send(json!({"command": "seek", "position_ms": 0}));
                } else if self.position_ms > RESTART_THRESHOLD_MS
                    && let Some(p) = &self.player
                {
                    p.seek(0);
                } else if let Some(item) = self.queue.prev() {
                    self.play_item(&item).await;
                    self.emit_queue().await;
                }
            }
            PlayerCommand::ToggleShuffle => {
                let on = !self.queue.shuffle();
                self.queue.set_shuffle(on);
                self.emit_queue().await;
            }
            PlayerCommand::CycleRepeat => {
                self.queue.set_repeat(self.queue.repeat().next());
                self.emit_queue().await;
            }
            PlayerCommand::Configure(options) => {
                if options != self.options {
                    self.options = options;
                    // Applies from the next loaded track; a paused track keeps its player.
                    self.rebuild = self.player.is_some();
                }
            }
            PlayerCommand::Pause if self.solo.handle.is_some() => {
                if self.state == PlaybackState::Playing {
                    self.solo_send(json!({"command": "pause"}));
                }
            }
            PlayerCommand::Pause => {
                if self.state == PlaybackState::Playing
                    && let Some(p) = &self.player
                {
                    p.pause();
                }
            }
            PlayerCommand::PlayAt { uri, position_ms } => {
                if !self.queue.select(&uri) {
                    let item = QueueItem {
                        uri,
                        ..QueueItem::default()
                    };
                    self.queue.set_context(vec![item], 0, String::new());
                }
                if let Some(item) = self.queue.current().cloned() {
                    self.play_item_at(&item, position_ms).await;
                }
                self.emit_queue().await;
            }
            PlayerCommand::ClearQueue => {
                self.queue.clear_manual();
                self.emit_queue().await;
            }
            PlayerCommand::MoveInQueue { from, to } => {
                self.queue.move_upcoming(from, to);
                self.emit_queue().await;
            }
            PlayerCommand::Enqueue(item) => {
                self.queue.add(item);
                self.emit_queue().await;
            }
            PlayerCommand::JumpTo(index) => {
                if let Some(item) = self.queue.jump(index) {
                    self.play_item(&item).await;
                }
                self.emit_queue().await;
            }
            PlayerCommand::TogglePlay if self.solo.handle.is_some() => match self.state {
                PlaybackState::Playing => self.solo_send(json!({"command": "pause"})),
                PlaybackState::Paused if self.solo.playing_uri.is_some() => {
                    self.solo_send(json!({"command": "play"}));
                }
                _ => {
                    if let Some(item) = self.queue.current().cloned() {
                        self.play_item(&item).await;
                    }
                }
            },
            PlayerCommand::TogglePlay => match (self.state, self.player.clone()) {
                (PlaybackState::Playing, Some(p)) => p.pause(),
                (PlaybackState::Paused, Some(p)) if !self.audio_failed => p.play(),
                _ => {
                    if let Some(item) = self.queue.current().cloned() {
                        self.play_item(&item).await;
                    }
                }
            },
            PlayerCommand::Seek { position_ms } if self.solo.handle.is_some() => {
                self.solo_send(json!({"command": "seek", "position_ms": position_ms}));
                self.solo.anchor = Some((position_ms, Instant::now()));
            }
            PlayerCommand::Seek { position_ms } => {
                if let Some(p) = &self.player {
                    p.seek(position_ms);
                }
            }
            PlayerCommand::SetVolume(percent) => self.set_volume(percent).await,
            PlayerCommand::Soloist(config) => self.configure_soloist(config).await,
            PlayerCommand::SoloistRetry => {
                if let Some(h) = &self.solo.handle {
                    h.pair();
                }
            }
        }
    }

    async fn emit_queue(&self) {
        self.emit(PlayerEvent::Queue(self.queue.snapshot(QUEUE_SNAPSHOT)))
            .await;
    }

    async fn stop(&mut self) {
        if self.solo.handle.is_some() {
            self.solo_send(json!({"command": "pause"}));
            self.solo.anchor = None;
        }
        if let Some(p) = &self.player {
            p.stop();
        }
        let end = self.position_ms;
        self.set_state(PlaybackState::Stopped, end).await;
    }

    async fn play_item(&mut self, item: &QueueItem) {
        self.play_item_at(item, 0).await;
    }

    async fn play_item_at(&mut self, item: &QueueItem, position_ms: u32) {
        if self.solo.handle.is_some() {
            return self.solo_play(item, position_ms).await;
        }
        let Ok(uri) = SpotifyUri::from_uri(&item.uri) else {
            return self
                .emit(PlayerEvent::Error("Некорректный URI".into()))
                .await;
        };
        let Some(player) = self.ensure_player() else {
            return self
                .emit(PlayerEvent::Error("Сначала войдите в аккаунт".into()))
                .await;
        };
        self.current_uri = Some(item.uri.clone());
        player.load(uri, true, position_ms);
        self.set_state(PlaybackState::Loading, position_ms).await;
    }

    /// End of track: continue with the queue, or stop at its end.
    async fn advance(&mut self) {
        match self.queue.next(true) {
            Some(item) => self.play_item(&item).await,
            None => {
                let end = self.position_ms;
                self.set_state(PlaybackState::Stopped, end).await;
            }
        }
        self.emit_queue().await;
    }

    async fn set_volume(&mut self, percent: u8) {
        self.volume = percent.min(100);
        if self.solo.handle.is_some() {
            self.solo_send(json!({"command": "set_volume", "volume": self.volume}));
        }
        let volume = percent_to_volume(percent);
        self.mixer.set_volume(volume);
        if let Some(p) = &self.player {
            p.emit_volume_changed_event(volume);
        }
        self.emit(PlayerEvent::Volume(percent.min(100))).await;
    }

    async fn handle_ls_event(&mut self, ev: LsEvent) {
        if let LsEvent::PlayRequestIdChanged { play_request_id } = ev {
            self.request_id = Some(play_request_id);
            return;
        }
        if let Some(id) = ev.get_play_request_id()
            && Some(id) != self.request_id
        {
            return; // Stale event from a previous load.
        }
        match ev {
            LsEvent::Loading { position_ms, .. } => {
                self.set_state(PlaybackState::Loading, position_ms).await;
            }
            LsEvent::Playing { position_ms, .. } => {
                self.skips = 0;
                self.set_state(PlaybackState::Playing, position_ms).await;
            }
            LsEvent::Paused { position_ms, .. } => {
                self.set_state(PlaybackState::Paused, position_ms).await;
            }
            LsEvent::PositionChanged { position_ms, .. }
            | LsEvent::PositionCorrection { position_ms, .. }
            | LsEvent::Seeked { position_ms, .. } => {
                let state = self.state;
                self.set_state(state, position_ms).await;
            }
            LsEvent::Stopped { .. } => self.set_state(PlaybackState::Stopped, 0).await,
            LsEvent::EndOfTrack { .. } => self.advance().await,
            LsEvent::TimeToPreloadNextTrack { .. } if self.options.gapless && !self.rebuild => {
                // Gapless: librespot buffers the next track before this one ends.
                if let (Some(p), Some(next)) = (&self.player, self.queue.peek_next(true))
                    && let Ok(uri) = SpotifyUri::from_uri(&next.uri)
                {
                    p.preload(uri);
                }
            }
            LsEvent::Unavailable { .. } => {
                self.emit(PlayerEvent::Error(
                    "Трек недоступен: удалён или закрыт в вашем регионе. Играет следующий.".into(),
                ))
                .await;
                self.skips += 1;
                if self.skips < MAX_SKIPS {
                    self.advance().await;
                } else {
                    self.emit(PlayerEvent::AudioKeyRefused).await;
                    self.set_state(PlaybackState::Stopped, 0).await;
                }
            }
            LsEvent::TrackChanged { audio_item } => {
                self.emit(PlayerEvent::Track(track_info(&audio_item))).await;
            }
            LsEvent::VolumeChanged { volume } => {
                self.emit(PlayerEvent::Volume(volume_to_percent(volume)))
                    .await;
            }
            _ => {}
        }
    }
}

impl Service {
    fn solo_send(&self, command: serde_json::Value) {
        if let Some(h) = &self.solo.handle {
            h.send(command);
        }
    }

    async fn configure_soloist(&mut self, config: Option<soloist::SoloistConfig>) {
        if self.solo.handle.as_ref().map(|h| &h.config) == config.as_ref() {
            return;
        }
        if let Some(old) = self.solo.handle.take() {
            old.stop().await;
        }
        self.solo = Solo::default();
        match config {
            Some(cfg) => {
                if let Some(p) = &self.player {
                    p.stop();
                }
                let (tx, rx) = mpsc::unbounded_channel();
                self.solo.handle = Some(soloist::Handle::start(
                    cfg,
                    self.volume,
                    self.sessions.clone(),
                    tx,
                ));
                self.solo.events = Some(rx);
            }
            None => self.emit(PlayerEvent::Soloist(SoloistStatus::Off)).await,
        }
        self.set_state(PlaybackState::Stopped, 0).await;
    }

    async fn solo_play(&mut self, item: &QueueItem, position_ms: u32) {
        if !self.solo.ready {
            return self
                .emit(PlayerEvent::Error(
                    "Soloist ещё не готов: статус в Настройках → Плеер".into(),
                ))
                .await;
        }
        if !self.solo.active {
            self.solo_send(json!({"command": "activate"}));
            self.solo.active = true;
        }
        self.current_uri = Some(item.uri.clone());
        self.solo.awaiting = Some((item.uri.clone(), Instant::now()));
        self.solo.seek_after_load = (position_ms > 0).then_some(position_ms);
        self.solo.anchor = None;
        self.solo_send(json!({"command": "play", "uri": item.uri}));
        self.set_state(PlaybackState::Loading, position_ms).await;
    }

    async fn solo_tick(&mut self) {
        if let Some((ms, at)) = self.solo.anchor {
            let now =
                ms.saturating_add(u32::try_from(at.elapsed().as_millis()).unwrap_or(u32::MAX));
            self.set_state(PlaybackState::Playing, now).await;
        }
    }

    /// `true` while an answer to our `play` is still due: older reports are about the old track.
    fn solo_waiting(&mut self) -> bool {
        match &self.solo.awaiting {
            Some((_, at)) if at.elapsed() > AWAIT_LIMIT => {
                self.solo.awaiting = None;
                false
            }
            Some(_) => true,
            None => false,
        }
    }

    /// Soloist reports a track. Ours: show it. Another one while nothing is pending: Soloist
    /// went on by itself (the track ended), so continue with Tonearm's queue instead.
    async fn solo_track(&mut self, track: Option<TrackInfo>) {
        let Some(track) = track else { return };
        if let Some((want, _)) = &self.solo.awaiting {
            if *want == track.uri {
                self.solo.awaiting = None;
                self.solo.playing_uri = Some(track.uri.clone());
                if let Some(ms) = self.solo.seek_after_load.take() {
                    self.solo_send(json!({"command": "seek", "position_ms": ms}));
                }
                self.emit(PlayerEvent::Track(track)).await;
                return;
            }
            if self.solo_waiting() {
                return;
            }
        }
        if self.solo.playing_uri.as_deref() == Some(track.uri.as_str()) {
            return;
        }
        if self.solo.playing_uri.is_none() {
            // Something started on Soloist from another app: show it.
            self.solo.playing_uri = Some(track.uri.clone());
            self.emit(PlayerEvent::Track(track)).await;
            return;
        }
        self.solo_advance().await;
    }

    async fn solo_advance(&mut self) {
        match self.queue.next(true) {
            Some(item) => self.play_item(&item).await,
            None => {
                self.solo_send(json!({"command": "pause"}));
                self.solo.playing_uri = None;
                self.solo.anchor = None;
                let end = self.position_ms;
                self.set_state(PlaybackState::Stopped, end).await;
            }
        }
        self.emit_queue().await;
    }

    async fn solo_status(&mut self, status: &str, position_ms: Option<u32>) {
        if self.solo_waiting() {
            return;
        }
        let pos = position_ms.unwrap_or(self.position_ms);
        let state = match status {
            "playing" => PlaybackState::Playing,
            "paused" => PlaybackState::Paused,
            "buffering" => PlaybackState::Loading,
            _ => {
                // Idle after one of ours played: the track ended without a next one.
                if self.solo.playing_uri.is_some() && self.state == PlaybackState::Playing {
                    self.solo_advance().await;
                }
                return;
            }
        };
        self.solo.anchor = (state == PlaybackState::Playing).then(|| (pos, Instant::now()));
        self.set_state(state, pos).await;
    }

    async fn handle_solo_event(&mut self, ev: soloist::Event) {
        match ev {
            soloist::Event::Status(status) => {
                self.solo.ready = matches!(status, SoloistStatus::Ready { .. });
                self.emit(PlayerEvent::Soloist(status)).await;
            }
            soloist::Event::Snapshot(snap) => {
                self.solo.active = snap.active;
                if let Some(v) = snap.volume {
                    self.volume = v;
                    self.emit(PlayerEvent::Volume(v)).await;
                }
                self.solo_track(snap.track).await;
                self.solo_status(&snap.status, Some(snap.position_ms)).await;
            }
            soloist::Event::Track(track) => self.solo_track(track).await,
            soloist::Event::Playing(status) => self.solo_status(&status, None).await,
            soloist::Event::Position(ms) => {
                if !self.solo_waiting() {
                    if self.state == PlaybackState::Playing {
                        self.solo.anchor = Some((ms, Instant::now()));
                    }
                    let state = self.state;
                    self.set_state(state, ms).await;
                }
            }
            soloist::Event::Volume(v) => {
                self.volume = v;
                self.emit(PlayerEvent::Volume(v)).await;
            }
        }
    }
}

fn track_info(item: &AudioItem) -> TrackInfo {
    let (artists, album) = match &item.unique_fields {
        UniqueFields::Track { artists, album, .. } => (
            artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            album.clone(),
        ),
        UniqueFields::Episode { show_name, .. } => (show_name.clone(), String::new()),
        UniqueFields::Local { artists, album, .. } => (
            artists.clone().unwrap_or_default(),
            album.clone().unwrap_or_default(),
        ),
    };
    TrackInfo {
        uri: item.uri.clone(),
        title: item.name.clone(),
        artists,
        album,
        duration_ms: item.duration_ms,
        // Covers come largest-last in practice; the bar needs a small one, so take the first.
        cover_url: item.covers.first().map(|c| c.url.clone()),
    }
}
