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

use crate::effects::EffectsOptions;
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
    fx: Fx,
}

/// Equalizer, fades and visualizer state (see [`crate::effects`]). Only Soloist plays through
/// Tonearm's PipeWire sink; with the built-in player these stay idle.
#[derive(Default)]
struct Fx {
    options: EffectsOptions,
    /// The Soloist config as the app sent it; `pipewire_device` is filled in here.
    base: Option<soloist::SoloistConfig>,
    #[cfg(target_os = "linux")]
    sink: Option<crate::effects::pipewire::Sink>,
    #[cfg(target_os = "linux")]
    capture: Option<crate::effects::spectrum::Capture>,
    /// The sink could not start; not tried again until the settings change.
    failed: bool,
    /// The track whose end is fading out now.
    out_uri: Option<String>,
    /// The output gain is (going) down.
    down: bool,
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
    /// Soloist cannot load silently: keep the queue selection local until resume.
    deferred_load: bool,
    /// The URI Soloist plays now, as we last saw it.
    playing_uri: Option<String>,
    /// What the player bar got for it; a fuller report of the same track replaces it.
    shown: Option<TrackInfo>,
    /// Position anchor for the 500 ms ticks (Soloist reports only changes).
    anchor: Option<(u32, Instant)>,
    /// Play/pause already shown on click; reports of the old state are stale for a moment.
    expect: Option<(PlaybackState, Instant)>,
    /// A seek already shown on click; positions from before it are stale for a moment.
    seek_sent: Option<(u32, Instant)>,
    /// Volume set here; Soloist's echoes of older values are ignored for a moment.
    volume_sent: Option<Instant>,
}

/// After this long a missing confirmation of `play` no longer holds events back.
const AWAIT_LIMIT: Duration = Duration::from_secs(8);
/// The next track is started this long before Soloist's end, before it can loop the track.
const SOLO_END_EARLY_MS: u32 = 250;
/// How long a click's result stays on screen while Soloist still reports the old state.
const OPTIMISTIC_HOLD: Duration = Duration::from_millis(2000);

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
        fx: Fx::default(),
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
        self.fx_visualizer().await;
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
            // Retry the audio device: build a fresh player. Dropping the old one joins
            // librespot's thread, which must not block this async worker.
            if let Some(old) = self.player.take() {
                old.stop();
                std::thread::spawn(move || drop(old));
            }
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
        // "Play" from the OS is a toggle that never pauses.
        let cmd = match cmd {
            PlayerCommand::Resume if self.state == PlaybackState::Playing => return,
            PlayerCommand::Resume => PlayerCommand::TogglePlay,
            cmd => cmd,
        };
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
                position_ms,
            } => {
                if let Some(on) = shuffle {
                    self.queue.set_shuffle(false);
                    self.queue.set_context(items, start, name);
                    self.queue.set_shuffle(on);
                } else {
                    self.queue.set_context(items, start, name);
                }
                if let Some(item) = self.queue.current().cloned() {
                    self.play_item_at(&item, position_ms, true).await;
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
                    self.solo_seek(0).await;
                } else if self.position_ms > RESTART_THRESHOLD_MS
                    && let Some(p) = &self.player
                {
                    p.seek(0);
                    let state = self.state;
                    self.set_state(state, 0).await;
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
            PlayerCommand::SetModes { shuffle, repeat } => {
                self.queue.set_shuffle(shuffle);
                self.queue.set_repeat(repeat);
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
                    self.solo_pause().await;
                }
            }
            PlayerCommand::Pause => {
                if self.state == PlaybackState::Playing
                    && let Some(p) = &self.player
                {
                    p.pause();
                }
            }
            PlayerCommand::PlayAt {
                uri,
                position_ms,
                start_playing,
            } => {
                if !self.queue.select(&uri) {
                    let item = QueueItem {
                        uri,
                        ..QueueItem::default()
                    };
                    self.queue.set_context(vec![item], 0, String::new());
                }
                if let Some(item) = self.queue.current().cloned() {
                    self.play_item_at(&item, position_ms, start_playing).await;
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
            PlayerCommand::PlayNext(item) => {
                self.queue.add_next(item);
                self.emit_queue().await;
            }
            PlayerCommand::RemoveFromQueue(index) => {
                self.queue.remove_upcoming(index);
                self.emit_queue().await;
            }
            PlayerCommand::JumpTo(index) => {
                if let Some(item) = self.queue.jump(index) {
                    self.play_item(&item).await;
                }
                self.emit_queue().await;
            }
            PlayerCommand::TogglePlay if self.solo.deferred_load => {
                if let Some(item) = self.queue.current().cloned() {
                    self.play_item_at(&item, self.position_ms, true).await;
                }
            }
            PlayerCommand::TogglePlay if self.solo.handle.is_some() => match self.state {
                PlaybackState::Playing => self.solo_pause().await,
                PlaybackState::Paused if self.solo.playing_uri.is_some() => {
                    self.solo_send(json!({"command": "play"}));
                    self.solo.expect = Some((PlaybackState::Playing, Instant::now()));
                    self.solo.anchor = Some((self.position_ms, Instant::now()));
                    let pos = self.position_ms;
                    self.set_state(PlaybackState::Playing, pos).await;
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
                self.solo_seek(position_ms).await;
            }
            PlayerCommand::Seek { position_ms } => {
                if let Some(p) = &self.player {
                    p.seek(position_ms);
                }
            }
            PlayerCommand::SetVolume(percent) => self.set_volume(percent).await,
            PlayerCommand::Soloist(config) => {
                self.fx.base.clone_from(&config);
                self.configure_soloist(config).await;
            }
            PlayerCommand::Effects(options) => self.set_effects(options).await,
            PlayerCommand::Resume => unreachable!("mapped to TogglePlay above"),
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
        self.solo.deferred_load = false;
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
        self.play_item_at(item, 0, true).await;
    }

    /// Puts the picked track into the player bar right away, from what the list already knows;
    /// the engine's own report of it fills in the rest later.
    async fn show_picked(&mut self, item: &QueueItem) {
        if item.title.is_empty() {
            return;
        }
        let track = TrackInfo {
            uri: item.uri.clone(),
            title: item.title.clone(),
            artists: item.artists.clone(),
            album: String::new(),
            duration_ms: item.duration_ms,
            cover_url: item.cover_url.clone(),
        };
        if self.solo.handle.is_some() {
            self.solo.shown = Some(track.clone());
        }
        self.emit(PlayerEvent::Track(track)).await;
    }

    async fn play_item_at(&mut self, item: &QueueItem, position_ms: u32, start_playing: bool) {
        self.solo.deferred_load = self.solo.handle.is_some() && !start_playing;
        if self.solo.deferred_load {
            self.solo.awaiting = None;
            self.solo.playing_uri = None;
            self.solo.anchor = None;
            self.current_uri = Some(item.uri.clone());
            self.show_picked(item).await;
            self.set_state(PlaybackState::Paused, position_ms).await;
            return;
        }
        if start_playing && position_ms == 0 {
            self.emit(PlayerEvent::Started(item.uri.clone())).await;
        }
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
        player.load(uri, start_playing, position_ms);
        self.show_picked(item).await;
        self.set_state(PlaybackState::Loading, position_ms).await;
    }

    /// End of track: continue with the queue, or stop at its end.
    async fn advance(&mut self) {
        self.advance_by(true).await;
    }

    /// `auto = false` skips even in "repeat one": a track that cannot play is not retried.
    async fn advance_by(&mut self, auto: bool) {
        match self.queue.next(auto) {
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
            self.solo.volume_sent = Some(Instant::now());
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
                    self.advance_by(false).await;
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
        let config = match config {
            Some(mut cfg) => {
                cfg.pipewire_device = self.fx_sink(&cfg.root).await;
                Some(cfg)
            }
            None => {
                self.fx_stop();
                None
            }
        };
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
        // The queue is ours: Soloist must not loop the one track it was given. With repeat off it
        // goes idle at the end, which advances the queue (the end-of-track check is the fallback).
        self.solo_send(json!({"command": "set_repeat_track", "enabled": false}));
        self.solo_send(json!({"command": "set_repeat_context", "enabled": false}));
        self.solo.expect = None;
        self.solo.seek_sent = None;
        self.show_picked(item).await;
        self.set_state(PlaybackState::Loading, position_ms).await;
    }

    /// Pause, shown at once: the position stops where it is now.
    async fn solo_pause(&mut self) {
        self.solo_send(json!({"command": "pause"}));
        // Nothing sounds while paused: put the gain back now, the end fades again on resume.
        self.fx_fade_up(0);
        let pos = self.solo_now();
        self.solo.anchor = None;
        self.solo.expect = Some((PlaybackState::Paused, Instant::now()));
        self.set_state(PlaybackState::Paused, pos).await;
    }

    async fn solo_seek(&mut self, position_ms: u32) {
        if self.solo.deferred_load {
            self.set_state(PlaybackState::Paused, position_ms).await;
            return;
        }
        self.solo_send(json!({"command": "seek", "position_ms": position_ms}));
        self.solo.seek_sent = Some((position_ms, Instant::now()));
        if self.state == PlaybackState::Playing {
            self.solo.anchor = Some((position_ms, Instant::now()));
        }
        let state = self.state;
        self.set_state(state, position_ms).await;
    }

    /// The position now, moved on from the last report while playing.
    fn solo_now(&self) -> u32 {
        match self.solo.anchor {
            Some((ms, at)) => {
                ms.saturating_add(u32::try_from(at.elapsed().as_millis()).unwrap_or(u32::MAX))
            }
            None => self.position_ms,
        }
    }

    /// `false` while a report contradicts what a click just put on screen.
    fn solo_agrees(&mut self, state: PlaybackState) -> bool {
        match self.solo.expect {
            Some((want, at)) if want != state && at.elapsed() < OPTIMISTIC_HOLD => false,
            Some(_) => {
                self.solo.expect = None;
                true
            }
            None => true,
        }
    }

    /// `false` for a position reported from before our seek.
    fn solo_seek_settled(&mut self, ms: u32) -> bool {
        match self.solo.seek_sent {
            Some((want, at)) if at.elapsed() < OPTIMISTIC_HOLD && want.abs_diff(ms) > 3000 => false,
            Some(_) => {
                self.solo.seek_sent = None;
                true
            }
            None => true,
        }
    }

    /// `false` for a volume echo older than the one we just set.
    fn solo_volume_settled(&mut self, v: u8) -> bool {
        match self.solo.volume_sent {
            Some(at) if at.elapsed() < OPTIMISTIC_HOLD && v != self.volume => false,
            _ => {
                self.solo.volume_sent = None;
                true
            }
        }
    }

    async fn solo_tick(&mut self) {
        if self.solo.anchor.is_some() {
            let now = self.solo_now();
            // Soloist plays a single URI and, at its end, starts it again (or goes idle without
            // saying so): move on with Tonearm's queue right at the end instead.
            let duration = self.solo_duration();
            if duration > 0 && now + SOLO_END_EARLY_MS >= duration && !self.solo_waiting() {
                tracing::info!(now, duration, "soloist track ended, next from the queue");
                self.solo_advance().await;
                return;
            }
            self.fx_fade_tick(now, duration);
            self.set_state(PlaybackState::Playing, now).await;
        }
    }

    /// Length of the track Soloist plays, as last reported (0 when unknown).
    fn solo_duration(&self) -> u32 {
        match (&self.solo.shown, &self.solo.playing_uri) {
            (Some(t), Some(uri)) if t.uri == *uri => t.duration_ms,
            _ => 0,
        }
    }

    /// The position jumped from the end of the track back to its start without a seek of ours:
    /// Soloist looped the track.
    fn solo_wrapped(&self, ms: u32) -> bool {
        let duration = self.solo_duration();
        duration > 0
            && self.state == PlaybackState::Playing
            && self.position_ms + 5_000 >= duration
            && ms < 3_000
    }

    /// `true` while an answer to our `play` is still due: older reports are about the old track.
    fn solo_waiting(&mut self) -> bool {
        if self.solo.deferred_load {
            return true;
        }
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
        if self.solo.deferred_load {
            return;
        }
        let Some(track) = track else { return };
        if let Some((want, _)) = &self.solo.awaiting {
            if *want == track.uri {
                self.solo.awaiting = None;
                self.solo.playing_uri = Some(track.uri.clone());
                if let Some(ms) = self.solo.seek_after_load.take() {
                    self.solo_send(json!({"command": "seek", "position_ms": ms}));
                }
                self.solo_show(track).await;
                return;
            }
            if self.solo_waiting() {
                return;
            }
        }
        if self.solo.playing_uri.as_deref() == Some(track.uri.as_str()) {
            // track_changed may carry only the title; playback_state the rest.
            self.solo_show(track).await;
            return;
        }
        if self.solo.playing_uri.is_none() {
            // Something started on Soloist from another app: show it.
            self.solo.playing_uri = Some(track.uri.clone());
            self.solo_show(track).await;
            return;
        }
        self.solo_advance().await;
    }

    /// Sends `track` to the player bar, keeping fields an earlier report of it had.
    async fn solo_show(&mut self, mut track: TrackInfo) {
        if let Some(old) = self.solo.shown.as_ref().filter(|o| o.uri == track.uri) {
            if track.title.is_empty() {
                track.title.clone_from(&old.title);
            }
            if track.artists.is_empty() {
                track.artists.clone_from(&old.artists);
            }
            if track.album.is_empty() {
                track.album.clone_from(&old.album);
            }
            if track.duration_ms == 0 {
                track.duration_ms = old.duration_ms;
            }
            if track.cover_url.is_none() {
                track.cover_url.clone_from(&old.cover_url);
            }
            if *old == track {
                return;
            }
        }
        self.solo.shown = Some(track.clone());
        self.emit(PlayerEvent::Track(track)).await;
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
        if !self.solo_agrees(state) {
            return;
        }
        let pos = if self.solo.seek_sent.is_some() && !self.solo_seek_settled(pos) {
            self.solo_now()
        } else {
            pos
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
                if let Some(v) = snap.volume
                    && self.solo_volume_settled(v)
                {
                    self.volume = v;
                    self.emit(PlayerEvent::Volume(v)).await;
                }
                self.solo_track(snap.track).await;
                self.solo_status(&snap.status, Some(snap.position_ms)).await;
            }
            soloist::Event::Track(track) => self.solo_track(track).await,
            soloist::Event::Playing(status) => self.solo_status(&status, None).await,
            soloist::Event::Position(ms) => {
                if !self.solo_waiting() && self.solo_wrapped(ms) {
                    tracing::info!(ms, "soloist looped the track, next from the queue");
                    self.solo_advance().await;
                } else if !self.solo_waiting() && self.solo_seek_settled(ms) {
                    if self.state == PlaybackState::Playing {
                        self.solo.anchor = Some((ms, Instant::now()));
                    }
                    let state = self.state;
                    self.set_state(state, ms).await;
                }
            }
            soloist::Event::Volume(v) if self.solo_volume_settled(v) => {
                self.volume = v;
                self.emit(PlayerEvent::Volume(v)).await;
            }
            soloist::Event::Volume(_) => {}
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

/// How long the next track takes to come up after a faded end.
const FADE_IN_MAX_MS: u32 = 2000;

impl Service {
    async fn set_effects(&mut self, options: EffectsOptions) {
        if options == self.fx.options {
            return;
        }
        let was_needed = self.fx.options.needs_sink();
        self.fx.options = options;
        if self.fx.options.needs_sink() && !was_needed {
            self.fx.failed = false;
        }
        if self.fx.options.fade_ms == 0 {
            self.fx_fade_up(150);
        }
        #[cfg(target_os = "linux")]
        if let Some(sink) = &self.fx.sink {
            sink.set_eq(self.fx.options.eq);
        }
        // Soloist plays into the default output: restart it into Tonearm's sink.
        let routed = self
            .solo
            .handle
            .as_ref()
            .is_some_and(|h| h.config.pipewire_device.is_some());
        if self.fx.options.needs_sink() && !routed && self.fx.base.is_some() {
            let base = self.fx.base.clone();
            self.configure_soloist(base).await;
        }
        self.fx_visualizer().await;
    }

    /// Starts Tonearm's sink if the settings need it and returns its node for Soloist.
    #[cfg(target_os = "linux")]
    async fn fx_sink(&mut self, dir: &std::path::Path) -> Option<String> {
        if let Some(sink) = &mut self.fx.sink
            && !sink.alive()
        {
            tracing::warn!("tonearm sink stopped, starting it again");
            self.fx.sink = None;
        }
        if self.fx.sink.is_none() && self.fx.options.needs_sink() && !self.fx.failed {
            match crate::effects::pipewire::Sink::start(dir, self.fx.options.eq).await {
                Ok(sink) => {
                    self.fx.sink = Some(sink);
                    self.emit(PlayerEvent::Effects(None)).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "no pipewire sink: equalizer and fades are off");
                    self.fx.failed = true;
                    self.emit(PlayerEvent::Effects(Some(
                        "Не получилось: нужен PipeWire (pipewire и pw-cli)".into(),
                    )))
                    .await;
                }
            }
        }
        self.fx
            .sink
            .is_some()
            .then(|| crate::effects::pipewire::NODE.to_owned())
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(clippy::unused_async)]
    async fn fx_sink(&mut self, _dir: &std::path::Path) -> Option<String> {
        None
    }

    fn fx_stop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            self.fx.capture = None;
            self.fx.sink = None;
        }
        self.fx.out_uri = None;
        self.fx.down = false;
    }

    /// The fade gain lives in the sink; without one there is nothing to fade.
    fn fx_fade(&self, to: f32, ms: u32) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(sink) = &self.fx.sink {
            sink.fade(to, ms);
            return true;
        }
        let _ = (to, ms);
        false
    }

    fn fx_fade_up(&mut self, ms: u32) {
        if self.fx.down {
            self.fx_fade(1.0, ms);
            self.fx.down = false;
            self.fx.out_uri = None;
        }
    }

    /// On every position tick while Soloist plays: fade out the last seconds of the track and
    /// bring the gain back up once the next one (or the same one, after a seek) plays.
    fn fx_fade_tick(&mut self, now: u32, duration: u32) {
        let fade = self.fx.options.fade_ms;
        let uri = self.solo.playing_uri.clone();
        let left = duration.saturating_sub(now + SOLO_END_EARLY_MS);
        if self.fx.down {
            let same = self.fx.out_uri.is_some() && self.fx.out_uri == uri;
            if !same {
                self.fx_fade_up(fade.min(FADE_IN_MAX_MS));
            } else if left > fade + 1000 {
                self.fx_fade_up(150);
            }
            return;
        }
        // Short tracks (and unknown lengths) are left alone.
        if fade > 0
            && duration > fade * 3
            && left <= fade
            && uri.is_some()
            && self.fx_fade(0.0, left)
        {
            self.fx.down = true;
            self.fx.out_uri = uri;
        }
    }

    /// Runs the visualizer's capture while it is on and Soloist plays into the sink.
    async fn fx_visualizer(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let want = self.fx.options.visualizer
                && self.fx.sink.is_some()
                && self.solo.handle.is_some()
                && self.state == PlaybackState::Playing;
            if want && self.fx.capture.is_none() {
                let events = self.events.clone();
                match crate::effects::spectrum::Capture::start(
                    crate::effects::pipewire::NODE,
                    move |frame| {
                        // A busy UI drops frames instead of queueing them, and frames never
                        // take the last free slots: state and track events must get through.
                        if events.capacity() > events.max_capacity() / 2 {
                            let _ = events.try_send(PlayerEvent::Spectrum(frame));
                        }
                    },
                ) {
                    Ok(c) => self.fx.capture = Some(c),
                    Err(e) => tracing::warn!(error = %e, "cannot start pw-record"),
                }
            } else if !want && self.fx.capture.take().is_some() {
                self.emit(PlayerEvent::Spectrum([0; crate::effects::spectrum::BARS]))
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! The Soloist side of the service: it only sees Soloist's reports (there is no API to ask
    //! "what is next"), so these heuristics decide when Tonearm's queue moves on.

    use super::*;
    use crate::soloist::{Event as SoloEvent, Snapshot};
    use serde_json::Value;

    struct Rig {
        svc: Service,
        events: mpsc::Receiver<PlayerEvent>,
        sent: mpsc::UnboundedReceiver<Value>,
    }

    fn item(n: u32) -> QueueItem {
        QueueItem {
            uri: format!("spotify:track:{n}"),
            title: format!("Трек {n}"),
            artists: "Исполнитель".into(),
            duration_ms: 200_000,
            cover_url: None,
        }
    }

    fn info(n: u32) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{n}"),
            title: format!("Трек {n}"),
            artists: "Исполнитель".into(),
            album: "Альбом".into(),
            duration_ms: 200_000,
            cover_url: None,
        }
    }

    fn rig() -> Rig {
        let (events_tx, events) = mpsc::channel(1024);
        let (_sessions_tx, sessions) = watch::channel(None);
        let (no_device_tx, _) = mpsc::unbounded_channel();
        let mixer = mixer::find(None).expect("soft mixer")(MixerConfig::default()).expect("mixer");
        let (handle, sent) = soloist::Handle::fake(soloist::SoloistConfig {
            api_key: "k".into(),
            root: std::env::temp_dir(),
            cache_dir: std::env::temp_dir(),
            device_name: "Tonearm".into(),
            pipewire_device: None,
        });
        let svc = Service {
            events: events_tx,
            mixer,
            session: None,
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
            sessions,
            solo: Solo {
                handle: Some(handle),
                ready: true,
                active: true,
                ..Solo::default()
            },
            fx: Fx::default(),
        };
        Rig { svc, events, sent }
    }

    impl Rig {
        async fn play_context(&mut self, n: u32, start: usize) {
            self.svc
                .handle_command(PlayerCommand::PlayContext {
                    items: (0..n).map(item).collect(),
                    start,
                    name: "Плейлист".into(),
                    shuffle: None,
                    position_ms: 0,
                })
                .await;
        }

        /// Soloist confirms the track we asked for and plays it.
        async fn soloist_plays(&mut self, n: u32, position_ms: u32) {
            self.svc
                .handle_solo_event(SoloEvent::Snapshot(Snapshot {
                    status: "playing".into(),
                    track: Some(info(n)),
                    position_ms,
                    volume: None,
                    active: true,
                }))
                .await;
        }

        fn played_uris(&mut self) -> Vec<String> {
            let mut out = Vec::new();
            while let Ok(v) = self.sent.try_recv() {
                if v["command"] == "play"
                    && let Some(u) = v["uri"].as_str()
                {
                    out.push(u.to_owned());
                }
            }
            out
        }

        fn commands(&mut self) -> Vec<String> {
            let mut out = Vec::new();
            while let Ok(v) = self.sent.try_recv() {
                out.push(v["command"].as_str().unwrap_or("").to_owned());
            }
            out
        }

        fn drain_events(&mut self) -> Vec<PlayerEvent> {
            let mut out = Vec::new();
            while let Ok(e) = self.events.try_recv() {
                out.push(e);
            }
            out
        }
    }

    async fn settle() {
        // The fake handle forwards commands on its own task.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn paused_connect_release_stays_silent_until_resume() {
        let mut r = rig();
        r.play_context(3, 0).await;
        r.soloist_plays(0, 100).await;
        settle().await;
        r.commands();
        r.svc
            .handle_command(PlayerCommand::PlayAt {
                uri: item(1).uri,
                position_ms: 4567,
                start_playing: false,
            })
            .await;
        r.soloist_plays(0, 200).await; // A stale snapshot must not restart or reposition it.
        assert_eq!(r.svc.state, PlaybackState::Paused);
        assert_eq!(r.svc.position_ms, 4567);
        settle().await;
        assert!(!r.commands().iter().any(|c| c == "play"));
        r.svc.handle_command(PlayerCommand::Resume).await;
        settle().await;
        assert_eq!(r.played_uris(), vec![item(1).uri]);
        r.soloist_plays(1, 0).await;
        settle().await;
        assert_eq!(
            r.sent.try_recv().unwrap(),
            json!({"command": "seek", "position_ms": 4567})
        );
    }

    #[tokio::test]
    async fn plays_the_picked_track_and_shows_it() {
        let mut r = rig();
        r.play_context(3, 1).await;
        settle().await;
        assert_eq!(r.played_uris(), vec!["spotify:track:1"]);
        r.soloist_plays(1, 0).await;
        assert_eq!(r.svc.state, PlaybackState::Playing);
        assert_eq!(r.svc.solo.playing_uri.as_deref(), Some("spotify:track:1"));
        let ev = r.drain_events();
        assert!(ev.contains(&PlayerEvent::Started("spotify:track:1".into())));
    }

    #[tokio::test]
    async fn stale_reports_of_the_old_track_do_not_skip_the_new_one() {
        let mut r = rig();
        r.play_context(3, 0).await;
        r.soloist_plays(0, 1_000).await;
        r.svc.handle_command(PlayerCommand::Next).await;
        settle().await;
        let _ = r.played_uris();
        // Soloist still talks about track 0 for a moment after our "play 1".
        r.soloist_plays(0, 2_000).await;
        settle().await;
        assert!(r.played_uris().is_empty(), "no extra skip");
        assert_eq!(
            r.svc.solo.awaiting.as_ref().map(|a| a.0.as_str()),
            Some("spotify:track:1")
        );
        r.soloist_plays(1, 0).await;
        assert!(r.svc.solo.awaiting.is_none());
        assert_eq!(
            r.svc.queue.current().map(|i| i.uri.as_str()),
            Some("spotify:track:1")
        );
    }

    #[tokio::test]
    async fn soloist_moving_on_by_itself_continues_with_our_queue() {
        let mut r = rig();
        r.play_context(3, 0).await;
        r.soloist_plays(0, 0).await;
        settle().await;
        let _ = r.played_uris();
        // A track we did not ask for (Soloist's own autoplay).
        r.soloist_plays(42, 0).await;
        settle().await;
        assert_eq!(r.played_uris(), vec!["spotify:track:1"]);
    }

    #[tokio::test]
    async fn a_loop_back_to_the_start_counts_as_the_end() {
        let mut r = rig();
        r.play_context(2, 0).await;
        r.soloist_plays(0, 0).await;
        r.svc.position_ms = 198_000;
        settle().await;
        let _ = r.played_uris();
        r.svc.handle_solo_event(SoloEvent::Position(500)).await;
        settle().await;
        assert_eq!(r.played_uris(), vec!["spotify:track:1"]);
    }

    #[tokio::test]
    async fn idle_at_the_end_of_the_queue_stops() {
        let mut r = rig();
        r.play_context(1, 0).await;
        r.soloist_plays(0, 0).await;
        r.svc
            .handle_solo_event(SoloEvent::Playing("idle".into()))
            .await;
        settle().await;
        assert_eq!(r.svc.state, PlaybackState::Stopped);
        assert!(r.svc.solo.playing_uri.is_none());
        assert!(r.commands().contains(&"pause".to_owned()));
    }

    #[tokio::test]
    async fn repeat_one_starts_the_same_track_again_as_a_new_listen() {
        let mut r = rig();
        r.play_context(2, 0).await;
        r.soloist_plays(0, 0).await;
        r.svc.handle_command(PlayerCommand::CycleRepeat).await; // All
        r.svc.handle_command(PlayerCommand::CycleRepeat).await; // One
        let _ = r.drain_events();
        r.svc
            .handle_solo_event(SoloEvent::Playing("idle".into()))
            .await;
        let started: Vec<_> = r
            .drain_events()
            .into_iter()
            .filter(|e| matches!(e, PlayerEvent::Started(_)))
            .collect();
        assert_eq!(
            started,
            vec![PlayerEvent::Started("spotify:track:0".into())]
        );
    }

    #[tokio::test]
    async fn os_play_and_pause_are_not_toggles() {
        let mut r = rig();
        r.play_context(1, 0).await;
        r.soloist_plays(0, 0).await;
        settle().await;
        let _ = r.commands();
        // "Play" while playing: nothing.
        r.svc.handle_command(PlayerCommand::Resume).await;
        settle().await;
        assert!(r.commands().is_empty());
        r.svc.handle_command(PlayerCommand::Pause).await;
        settle().await;
        assert_eq!(r.commands(), vec!["pause"]);
        assert_eq!(r.svc.state, PlaybackState::Paused);
        // "Pause" while paused: nothing (a lock screen must not start the music).
        r.svc.handle_command(PlayerCommand::Pause).await;
        settle().await;
        assert!(r.commands().is_empty());
        r.svc.handle_command(PlayerCommand::Resume).await;
        settle().await;
        assert_eq!(r.commands(), vec!["play"]);
        assert_eq!(r.svc.state, PlaybackState::Playing);
    }

    #[tokio::test]
    async fn a_resume_mid_track_is_not_a_new_listen() {
        let mut r = rig();
        r.svc
            .handle_command(PlayerCommand::PlayContext {
                items: vec![item(0)],
                start: 0,
                name: String::new(),
                shuffle: None,
                position_ms: 90_000,
            })
            .await;
        let started = r
            .drain_events()
            .into_iter()
            .any(|e| matches!(e, PlayerEvent::Started(_)));
        assert!(!started);
    }
}
