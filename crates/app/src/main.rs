// No console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod os_media;
mod ui_state;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tonearm_core::config::{BITRATES, CACHE_SIZES_MB, Settings};
use tonearm_core::connect::{ConnectCommand, ConnectEvent, RemoteAction};
use tonearm_core::covers::CoverCache;
use tonearm_core::library::{LibraryCommand, LibraryEvent, PageRequest};
use tonearm_core::{AuthState, Command, Event, Notice};
use tonearm_player::{AudioOptions, PlayerCommand, PlayerEvent};
use ui_state::{CoverTarget, Effect};

slint::include_modules!();

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let mut filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // librespot logs raw OAuth/token responses at trace level: cap these targets whatever RUST_LOG says.
    for directive in ["librespot_oauth=info", "librespot_core=debug"] {
        if let Ok(d) = directive.parse() {
            filter = filter.add_directive(d);
        }
    }
    // Also log to a file: a Windows release build has no console, so this is the only trace.
    let file = log_path().and_then(|p| {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).ok()?;
        }
        std::fs::File::create(p).ok()
    });
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match file {
        Some(f) => {
            use tracing_subscriber::fmt::writer::MakeWriterExt;
            builder
                .with_ansi(false)
                .with_writer(std::io::stderr.and(Mutex::new(f)))
                .init();
        }
        None => builder.init(),
    }
}

/// `tonearm.log` in the local data dir (`%LOCALAPPDATA%\tonearm\data` on Windows).
fn log_path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "", "tonearm")
        .map(|d| d.data_local_dir().join("tonearm.log"))
}

/// Logs a fatal error and, on Windows (no console), shows it in a message box.
fn report_fatal(message: &str) {
    tracing::error!("{message}");
    let log = log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let text = format!("Tonearm не смог запуститься.\n\n{message}\n\nЛог: {log}");
    eprintln!("{text}");
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let wide = |s: &str| {
            s.encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        };
        let (body, title) = (wide(&text), wide("Tonearm"));
        // SAFETY: both buffers are NUL-terminated UTF-16 and outlive the call.
        #[allow(unsafe_code)]
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                body.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!("panic: {info}\n{bt}");
        if std::thread::current().name() == Some("main") {
            report_fatal(&format!("Внутренняя ошибка: {info}"));
        }
    }));
}

/// Senders for everything the UI can ask for.
#[derive(Clone)]
struct Senders {
    auth: mpsc::Sender<Command>,
    player: mpsc::Sender<PlayerCommand>,
    library: mpsc::Sender<LibraryCommand>,
    connect: mpsc::Sender<ConnectCommand>,
    /// Spotify Connect: id of the remote device that plays now (`None` = this computer).
    remote: Arc<Mutex<Option<String>>>,
    remote_playing: Arc<AtomicBool>,
    covers: Arc<CoverCache>,
    /// What `config.toml` holds; the settings page edits it.
    settings: Arc<Mutex<Settings>>,
    rt: Handle,
    window: slint::Weak<AppWindow>,
}

impl Senders {
    fn notifications(&self) -> bool {
        self.settings.lock().is_ok_and(|s| s.ui.notifications)
    }

    /// The settings page changed something: apply it and write `config.toml`.
    fn settings_changed(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let Ok(mut current) = self.settings.lock() else {
            return;
        };
        let new = read_settings(&w, current.clone());
        if new == *current {
            return;
        }
        if new.audio != current.audio {
            self.local(PlayerCommand::Configure(audio_options(&new)));
        }
        if new.cache != current.cache {
            let covers = Arc::clone(&self.covers);
            let bytes = mb_to_bytes(new.cache.max_size_mb);
            // set_limit prunes the directory: keep it off the UI thread.
            self.rt.spawn_blocking(move || covers.set_limit(bytes));
        }
        *current = new.clone();
        drop(current);
        if std::env::var_os("TONEARM_UI_PREVIEW").is_some() {
            return; // sample data: never touch the real config
        }
        self.rt.spawn_blocking(move || {
            if let Err(e) = new.save() {
                tracing::warn!(error = %e, "cannot save config.toml");
            }
        });
    }

    fn auth(&self, cmd: Command) {
        if self.auth.try_send(cmd).is_err() {
            tracing::warn!("auth backend busy");
        }
    }

    /// Player commands go to the local player, or to the remote device while one plays.
    fn player(&self, cmd: PlayerCommand) {
        let remote = self.remote.lock().ok().and_then(|r| r.clone());
        let cmd = match (remote, cmd) {
            (None, cmd) => cmd,
            (Some(device_id), cmd) => {
                let action = match cmd {
                    PlayerCommand::TogglePlay => {
                        if self.remote_playing.load(Ordering::Relaxed) {
                            ConnectCommand::Control(RemoteAction::Pause)
                        } else {
                            ConnectCommand::Control(RemoteAction::Play)
                        }
                    }
                    PlayerCommand::Pause => ConnectCommand::Control(RemoteAction::Pause),
                    PlayerCommand::Next => ConnectCommand::Control(RemoteAction::Next),
                    PlayerCommand::Prev => ConnectCommand::Control(RemoteAction::Prev),
                    PlayerCommand::Seek { position_ms } => {
                        ConnectCommand::Control(RemoteAction::Seek(position_ms))
                    }
                    PlayerCommand::SetVolume(p) => ConnectCommand::Control(RemoteAction::Volume(p)),
                    PlayerCommand::PlayContext { items, start, .. } => ConnectCommand::PlayOn {
                        device_id,
                        uris: items.into_iter().map(|i| i.uri).collect(),
                        offset: start,
                        position_ms: 0,
                    },
                    PlayerCommand::Load(uri) => ConnectCommand::PlayOn {
                        device_id,
                        uris: vec![uri],
                        offset: 0,
                        position_ms: 0,
                    },
                    // Queue edits, shuffle and repeat stay local.
                    other => return self.local(other),
                };
                return self.connect(action);
            }
        };
        self.local(cmd);
    }

    fn local(&self, cmd: PlayerCommand) {
        if self.player.try_send(cmd).is_err() {
            tracing::debug!("player busy, command dropped");
        }
    }

    fn connect(&self, cmd: ConnectCommand) {
        if self.connect.try_send(cmd).is_err() {
            tracing::debug!("connect busy, command dropped");
        }
    }

    fn set_remote(&self, device: Option<String>) {
        if let Ok(mut r) = self.remote.lock() {
            *r = device;
        }
    }

    fn run(&self, effects: Vec<Effect>) {
        for fx in effects {
            match fx {
                Effect::Open(req) => {
                    if self.library.try_send(LibraryCommand::Open(req)).is_err() {
                        tracing::warn!("library busy");
                    }
                }
                Effect::Player(cmd) => self.player(cmd),
                Effect::SetSaved { uri, saved } => {
                    if self
                        .library
                        .try_send(LibraryCommand::SetSaved { uri, saved })
                        .is_err()
                    {
                        tracing::warn!("library backend busy");
                    }
                }
                Effect::Search {
                    query,
                    kind,
                    offset,
                } => {
                    let cmd = LibraryCommand::Search {
                        query,
                        kind,
                        offset,
                    };
                    if self.library.try_send(cmd).is_err() {
                        tracing::warn!("library busy, search dropped");
                    }
                }
                Effect::Cover { url, size, target } => self.load_cover(url, size, target),
            }
        }
    }

    fn load_cover(&self, url: String, size: u32, target: CoverTarget) {
        let covers = Arc::clone(&self.covers);
        let weak = self.window.clone();
        self.rt.spawn(async move {
            match covers.get(&url, size).await {
                Ok(cover) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            ui_state::with(|st| st.on_cover(&w, &target, &cover));
                        }
                    });
                }
                Err(e) => tracing::debug!(%e, "cover not loaded"),
            }
        });
    }
}

/// Forwards events from a backend channel into the UI thread.
fn forward<E: Send + 'static>(
    rt: &tokio::runtime::Runtime,
    mut rx: mpsc::Receiver<E>,
    weak: slint::Weak<AppWindow>,
    senders: Senders,
    apply: fn(&AppWindow, &Senders, E),
) {
    rt.spawn(async move {
        while let Some(event) = rx.recv().await {
            let weak = weak.clone();
            let senders = senders.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    apply(&w, &senders, event);
                }
            });
        }
    });
}

fn apply_auth(w: &AppWindow, s: &Senders, event: Event) {
    // The preview fakes the logged-in state; real auth events would overwrite it.
    #[cfg(debug_assertions)]
    if std::env::var_os("TONEARM_UI_PREVIEW").is_some() {
        return;
    }
    match event {
        Event::Auth(state) => {
            let view = match state {
                AuthState::Checking => AuthView::Checking,
                AuthState::LoggedOut => {
                    if ui_state::with(|st| std::mem::take(&mut st.reopen_login)) {
                        s.auth(Command::Login);
                    }
                    AuthView::LoggedOut
                }
                AuthState::WaitingForBrowser => {
                    w.set_waited_secs(0);
                    AuthView::Waiting
                }
                AuthState::Connecting => AuthView::Connecting,
                AuthState::Reconnecting => {
                    // Keep the main UI while reconnecting; the red bar shows the countdown.
                    if w.get_auth_view() == AuthView::LoggedIn {
                        AuthView::LoggedIn
                    } else {
                        AuthView::Reconnecting
                    }
                }
                AuthState::LoggedIn { username } => {
                    w.set_username(username.to_uppercase().into());
                    w.set_notice("".into());
                    w.set_offline(false);
                    AuthView::LoggedIn
                }
            };
            w.set_auth_view(view);
        }
        Event::Notice(Notice::Info(text)) => {
            w.set_notice(text.into());
            w.set_notice_is_error(false);
        }
        Event::Notice(Notice::Error(text)) => {
            w.set_notice(text.into());
            w.set_notice_is_error(true);
        }
        Event::Offline { retry_in_secs } => {
            w.set_offline(true);
            w.set_retry_in_secs(i32::try_from(retry_in_secs).unwrap_or(60));
        }
        Event::Online => w.set_offline(false),
    }
}

fn apply_player(w: &AppWindow, s: &Senders, event: PlayerEvent) {
    // While a Connect device plays, the deck shows that device, not the paused local player.
    let remote = s.remote.lock().ok().is_some_and(|r| r.is_some());
    if remote && matches!(event, PlayerEvent::State { .. } | PlayerEvent::Track(_)) {
        return;
    }
    match event {
        PlayerEvent::State { state, position_ms } => {
            os_media::on_state(state, position_ms);
            w.set_play_view(ui_state::play_view(state));
            w.set_position_ms(i32::try_from(position_ms).unwrap_or(i32::MAX));
        }
        PlayerEvent::Queue(q) => ui_state::with(|st| st.on_queue(w, &q)),
        PlayerEvent::Track(track) => {
            os_media::on_track(&track, window_focused(w), s.notifications());
            let fx = ui_state::with(|st| st.on_track(w, &track));
            s.run(fx);
        }
        PlayerEvent::Volume(percent) => {
            os_media::on_volume(percent);
            w.set_volume_level((i32::from(percent) * 12 + 50) / 100);
        }
        PlayerEvent::Error(text) => {
            w.set_notice(text.into());
            w.set_notice_is_error(true);
        }
    }
}

fn apply_connect(w: &AppWindow, s: &Senders, event: ConnectEvent) {
    match event {
        ConnectEvent::Devices(list) => {
            w.set_devices_loading(false);
            ui_state::with(|st| st.on_devices(w, list));
        }
        ConnectEvent::Remote(state) => {
            let Some(state) = state else {
                // Nothing plays anywhere any more.
                s.remote_playing.store(false, Ordering::Relaxed);
                if s.remote.lock().ok().and_then(|r| r.clone()).is_some() {
                    w.set_play_view(PlayView::Paused);
                }
                return;
            };
            if s.remote.lock().ok().and_then(|r| r.clone()).is_none() {
                return; // stale poll after coming back
            }
            // Follow the playback if someone moved it to yet another device.
            s.set_remote(Some(state.device.id.clone()));
            w.set_remote_name(state.device.name.to_uppercase().into());
            s.remote_playing.store(state.is_playing, Ordering::Relaxed);
            w.set_play_view(if state.is_playing {
                PlayView::Playing
            } else {
                PlayView::Paused
            });
            w.set_position_ms(i32::try_from(state.progress_ms).unwrap_or(0));
            if let Some(v) = state.device.volume_percent {
                w.set_volume_level((i32::from(v) * 12 + 50) / 100);
            }
            let track = tonearm_player::TrackInfo {
                uri: state.uri.clone(),
                title: state.title.clone(),
                artists: state.artists.clone(),
                album: state.album.clone(),
                duration_ms: state.duration_ms,
                cover_url: state.cover_url.clone(),
            };
            let fx = ui_state::with(|st| {
                if st.playing_uri == track.uri {
                    Vec::new()
                } else {
                    st.on_track(w, &track)
                }
            });
            s.run(fx);
        }
        ConnectEvent::Released { uri, position_ms } => {
            s.set_remote(None);
            s.remote_playing.store(false, Ordering::Relaxed);
            w.set_remote_name("".into());
            if let Some(uri) = uri {
                s.local(PlayerCommand::PlayAt { uri, position_ms });
            }
        }
        ConnectEvent::Error(text) => {
            w.set_devices_loading(false);
            w.set_notice(text.into());
            w.set_notice_is_error(true);
        }
    }
}

fn window_focused(w: &AppWindow) -> bool {
    use slint::winit_030::WinitWindowAccessor;
    w.window()
        .with_winit_window(|ww| ww.has_focus())
        .unwrap_or(true)
}

fn raise(w: &AppWindow) {
    use slint::winit_030::WinitWindowAccessor;
    w.window().with_winit_window(|ww| {
        ww.set_minimized(false);
        ww.focus_window();
    });
}

fn apply_library(w: &AppWindow, s: &Senders, event: LibraryEvent) {
    let fx = ui_state::with(|st| st.on_library(w, event));
    s.run(fx);
}

fn date_label() -> String {
    // Only a decorative header; UTC date is good enough and avoids a time-zone dependency.
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    // Civil-from-days (Howard Hinnant), proleptic Gregorian.
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    const MONTHS: [&str; 12] = [
        "ЯНВАРЯ",
        "ФЕВРАЛЯ",
        "МАРТА",
        "АПРЕЛЯ",
        "МАЯ",
        "ИЮНЯ",
        "ИЮЛЯ",
        "АВГУСТА",
        "СЕНТЯБРЯ",
        "ОКТЯБРЯ",
        "НОЯБРЯ",
        "ДЕКАБРЯ",
    ];
    const WEEKDAYS: [&str; 7] = [
        "ЧЕТВЕРГ",
        "ПЯТНИЦА",
        "СУББОТА",
        "ВОСКРЕСЕНЬЕ",
        "ПОНЕДЕЛЬНИК",
        "ВТОРНИК",
        "СРЕДА",
    ];
    let month = MONTHS[usize::try_from(m - 1).unwrap_or(0)];
    let weekday = WEEKDAYS[usize::try_from(days % 7).unwrap_or(0)];
    format!("{weekday}, {d} {month}")
}

fn mb_to_bytes(mb: u32) -> u64 {
    u64::from(mb) * 1024 * 1024
}

fn cache_mb(covers: &CoverCache) -> i32 {
    i32::try_from(covers.usage().div_ceil(1024 * 1024)).unwrap_or(i32::MAX)
}

fn audio_options(s: &Settings) -> AudioOptions {
    AudioOptions {
        bitrate: s.audio.bitrate,
        normalisation: s.audio.normalisation,
        gapless: s.audio.gapless,
        device: s.audio.device.clone(),
    }
}

fn index_of<T: PartialEq>(values: &[T], value: &T, default: usize) -> i32 {
    let i = values.iter().position(|v| v == value).unwrap_or(default);
    i32::try_from(i).unwrap_or(0)
}

/// Settings → the settings page.
fn show_settings(w: &AppWindow, s: &Settings) {
    w.set_set_bitrate(index_of(&BITRATES, &s.audio.bitrate, 2));
    w.set_set_normalisation(i32::from(s.audio.normalisation));
    w.set_set_gapless(i32::from(s.audio.gapless));
    w.set_set_notifications(i32::from(s.ui.notifications));
    w.set_set_device(s.audio.device.as_str().into());
    // A hand-edited size that is not one of the steps shows as the nearest larger step.
    let cache = CACHE_SIZES_MB
        .iter()
        .position(|&mb| mb >= s.cache.max_size_mb)
        .unwrap_or(CACHE_SIZES_MB.len() - 1);
    w.set_set_cache(i32::try_from(cache).unwrap_or(2));
}

/// The settings page → Settings (keeps what the page does not show, like the Client ID).
fn read_settings(w: &AppWindow, mut s: Settings) -> Settings {
    let pick = |i: i32, len: usize| usize::try_from(i).unwrap_or(0).min(len - 1);
    s.audio.bitrate = BITRATES[pick(w.get_set_bitrate(), BITRATES.len())];
    s.audio.normalisation = w.get_set_normalisation() != 0;
    s.audio.gapless = w.get_set_gapless() != 0;
    s.audio.device = w.get_set_device().to_string();
    s.ui.notifications = w.get_set_notifications() != 0;
    s.cache.max_size_mb = CACHE_SIZES_MB[pick(w.get_set_cache(), CACHE_SIZES_MB.len())];
    s
}

fn main() -> std::process::ExitCode {
    init_logging();
    install_panic_hook();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        "starting"
    );
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            report_fatal(&e.to_string());
            std::process::ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Prefer the lightweight software renderer unless the user overrides it via SLINT_BACKEND.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("software".into())
            .select()?;
    }

    // The backend runs on its own tokio runtime; the Slint event loop owns the main thread.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(32);
    let (ev_tx, ev_rx) = mpsc::channel::<Event>(32);
    let (pcmd_tx, pcmd_rx) = mpsc::channel::<PlayerCommand>(64);
    let (pev_tx, pev_rx) = mpsc::channel::<PlayerEvent>(64);
    let (lcmd_tx, lcmd_rx) = mpsc::channel::<LibraryCommand>(16);
    let (lev_tx, lev_rx) = mpsc::channel::<LibraryEvent>(64);
    let (ccmd_tx, ccmd_rx) = mpsc::channel::<ConnectCommand>(16);
    let (cev_tx, cev_rx) = mpsc::channel::<ConnectEvent>(16);
    // The core publishes the connected session; player and library pick it up.
    let (session_tx, session_rx) = watch::channel(None);

    let settings = Settings::load();
    let window = AppWindow::new()?;
    ui_state::init(&window);
    show_settings(&window, &settings);
    window.set_version(env!("CARGO_PKG_VERSION").into());
    window.set_date_label(date_label().into());

    let senders = Senders {
        auth: cmd_tx.clone(),
        player: pcmd_tx,
        connect: ccmd_tx,
        remote: Arc::new(Mutex::new(None)),
        remote_playing: Arc::new(AtomicBool::new(false)),
        library: lcmd_tx,
        covers: Arc::new(CoverCache::new(mb_to_bytes(settings.cache.max_size_mb))),
        settings: Arc::new(Mutex::new(settings.clone())),
        rt: runtime.handle().clone(),
        window: window.as_weak(),
    };

    senders.local(PlayerCommand::Configure(audio_options(&settings)));
    let s = senders.clone();
    window.on_settings_changed(move || s.settings_changed());
    let s = senders.clone();
    window.on_clear_cache(move || {
        let covers = Arc::clone(&s.covers);
        let weak = s.window.clone();
        s.rt.spawn_blocking(move || {
            covers.clear();
            let used = cache_mb(&covers);
            let _ = weak.upgrade_in_event_loop(move |w| w.set_cache_used_mb(used));
        });
    });
    let s = senders.clone();
    window.on_refresh_audio_devices(move || {
        let covers = Arc::clone(&s.covers);
        let weak = s.window.clone();
        s.rt.spawn_blocking(move || {
            let devices = tonearm_player::output_devices();
            let used = cache_mb(&covers);
            let _ = weak.upgrade_in_event_loop(move |w| {
                let model: Vec<slint::SharedString> = devices.into_iter().map(Into::into).collect();
                w.set_audio_devices(slint::ModelRc::new(slint::VecModel::from(model)));
                w.set_cache_used_mb(used);
            });
        });
    });
    let s = senders.clone();
    window.on_login(move || s.auth(Command::Login));
    let s = senders.clone();
    window.on_cancel_login(move || s.auth(Command::CancelLogin));
    let s = senders.clone();
    window.on_reopen_login(move || {
        ui_state::with(|st| st.reopen_login = true);
        s.auth(Command::CancelLogin);
    });
    let s = senders.clone();
    window.on_logout(move || s.auth(Command::Logout));
    let s = senders.clone();
    window.on_retry_now(move || s.auth(Command::RetryNow));
    let weak = window.as_weak();
    window.on_dismiss_notice(move || {
        if let Some(w) = weak.upgrade() {
            w.set_notice("".into());
        }
    });

    let s = senders.clone();
    window.on_open_liked(move || {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.open(&w, PageRequest::Liked, true));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_open(move |kind, uri| {
        if let Some(w) = s.window.upgrade() {
            let req = ui_state::request_from(kind, uri.to_string());
            let fx = ui_state::with(|st| st.open(&w, req, true));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_open_byline(move || {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| match st.byline_uri.clone() {
                Some(uri) => st.open(&w, PageRequest::Artist(uri), true),
                None => Vec::new(),
            });
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_search_edited(move |text| {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.search_edited(&w, &text));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_search_accepted(move |text| {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.search_accepted(&w, &text));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_search_tab_changed(move |tab| {
        if let Some(w) = s.window.upgrade() {
            ui_state::with(|st| st.search_tab_changed(&w, tab));
        }
    });
    let s = senders.clone();
    window.on_search_load_more(move || {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.search_load_more(&w));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_search_play(move |i| {
        let fx = ui_state::with(|st| st.search_play(usize::try_from(i).unwrap_or(usize::MAX)));
        s.run(fx);
    });
    let s = senders.clone();
    window.on_back(move || {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.back(&w));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_play_row(move |i| {
        let fx = ui_state::with(|st| st.play_row(usize::try_from(i).unwrap_or(usize::MAX)));
        s.run(fx);
    });
    let s = senders.clone();
    window.on_play_page(move || {
        let fx = ui_state::with(|st| st.play_page(false));
        s.run(fx);
    });
    let s = senders.clone();
    window.on_play_shuffled(move || {
        let fx = ui_state::with(|st| st.play_page(true));
        s.run(fx);
    });
    for (cb, cmd) in [
        (0, PlayerCommand::Next),
        (1, PlayerCommand::Prev),
        (2, PlayerCommand::ToggleShuffle),
        (3, PlayerCommand::CycleRepeat),
        (4, PlayerCommand::ClearQueue),
    ] {
        let s = senders.clone();
        let f = move || s.player(cmd.clone());
        match cb {
            0 => window.on_next(f),
            1 => window.on_prev(f),
            2 => window.on_toggle_shuffle(f),
            3 => window.on_cycle_repeat(f),
            _ => window.on_queue_clear(f),
        }
    }
    let s = senders.clone();
    window.on_refresh_devices(move || {
        if let Some(w) = s.window.upgrade() {
            w.set_devices_loading(true);
        }
        s.connect(ConnectCommand::Refresh);
    });
    let s = senders.clone();
    window.on_pick_device(move |id| {
        let Some(w) = s.window.upgrade() else { return };
        let remote = s.remote.lock().ok().and_then(|r| r.clone());
        if id.is_empty() {
            // Back to this computer.
            if remote.is_some() {
                s.connect(ConnectCommand::Release);
            }
            return;
        }
        if remote.as_deref() == Some(id.as_str()) {
            return;
        }
        let (uris, position_ms) = ui_state::with(|st| st.handoff());
        let position_ms = if remote.is_some() {
            0
        } else {
            position_ms.unwrap_or(u32::try_from(w.get_position_ms()).unwrap_or(0))
        };
        // Pause here first, then route everything to the remote device.
        s.local(PlayerCommand::Pause);
        s.set_remote(Some(id.to_string()));
        if uris.is_empty() || remote.is_some() {
            s.connect(ConnectCommand::Transfer {
                device_id: id.to_string(),
            });
        } else {
            s.connect(ConnectCommand::PlayOn {
                device_id: id.to_string(),
                uris,
                offset: 0,
                position_ms,
            });
        }
        let name = ui_state::with(|st| st.device_name(&id));
        w.set_remote_name(name.to_uppercase().into());
    });
    let s = senders.clone();
    window.on_queue_jump(move |i| {
        s.player(PlayerCommand::JumpTo(
            usize::try_from(i).unwrap_or(usize::MAX),
        ));
    });
    let s = senders.clone();
    window.on_row_action(move |source, index, action| {
        if let Some(w) = s.window.upgrade() {
            let index = usize::try_from(index).unwrap_or(usize::MAX);
            let fx = ui_state::with(|st| st.row_action(&w, source, index, action));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_toggle_play(move || s.player(PlayerCommand::TogglePlay));
    let weak = window.as_weak();
    let s = senders.clone();
    window.on_seek(move |ratio| {
        if let Some(w) = weak.upgrade() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let ms = (f64::from(ratio.clamp(0.0, 1.0)) * f64::from(w.get_duration_ms())) as u32;
            s.player(PlayerCommand::Seek { position_ms: ms });
        }
    });
    let s = senders.clone();
    window.on_volume_changed(move |level| {
        let percent = u8::try_from(level.clamp(0, 12) * 100 / 12).unwrap_or(100);
        s.player(PlayerCommand::SetVolume(percent));
    });

    // Once-a-second UI clock: login wait time and offline countdown.
    let tick = slint::Timer::default();
    let weak = window.as_weak();
    tick.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        move || {
            if let Some(w) = weak.upgrade() {
                if w.get_auth_view() == AuthView::Waiting {
                    w.set_waited_secs(w.get_waited_secs() + 1);
                }
                if w.get_offline() && w.get_retry_in_secs() > 0 {
                    w.set_retry_in_secs(w.get_retry_in_secs() - 1);
                }
                // A remote device is polled every few seconds; move the dial in between.
                if !w.get_remote_name().is_empty() && w.get_play_view() == PlayView::Playing {
                    let next = (w.get_position_ms() + 1000).min(w.get_duration_ms());
                    w.set_position_ms(next);
                }
            }
        },
    );

    let backend = runtime.spawn(tonearm_core::run_backend(cmd_rx, ev_tx, session_tx));
    let player = runtime.spawn(tonearm_player::run(pcmd_rx, pev_tx, session_rx.clone()));
    let library = runtime.spawn(tonearm_core::library::run(
        lcmd_rx,
        lev_tx,
        session_rx.clone(),
    ));
    let connect = runtime.spawn(tonearm_core::connect::run(ccmd_rx, cev_tx, session_rx));
    forward(
        &runtime,
        cev_rx,
        window.as_weak(),
        senders.clone(),
        apply_connect,
    );
    forward(
        &runtime,
        ev_rx,
        window.as_weak(),
        senders.clone(),
        apply_auth,
    );
    forward(
        &runtime,
        pev_rx,
        window.as_weak(),
        senders.clone(),
        apply_player,
    );
    forward(
        &runtime,
        lev_rx,
        window.as_weak(),
        senders.clone(),
        apply_library,
    );

    // Debug builds only: TONEARM_UI_PREVIEW=1 shows the logged-in layout with sample data,
    // to check the UI without a Spotify account.
    #[cfg(debug_assertions)]
    if std::env::var_os("TONEARM_UI_PREVIEW").is_some() {
        preview(&window);
    }

    // Media keys / MPRIS / SMTC, once the event loop runs and the window has a native handle.
    let weak = window.as_weak();
    let media_senders = senders.clone();
    // winit creates the native window lazily, so retry until it has a handle (gives up after ~10 s).
    let media_timer = std::rc::Rc::new(slint::Timer::default());
    let attempts = std::cell::Cell::new(0u32);
    let timer_weak = std::rc::Rc::downgrade(&media_timer);
    media_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let stop = || {
                if let Some(t) = timer_weak.upgrade() {
                    t.stop();
                }
            };
            let Some(w) = weak.upgrade() else {
                return stop();
            };
            attempts.set(attempts.get() + 1);
            let ui_weak = weak.clone();
            let s = media_senders.clone();
            let done = os_media::init(
                &w,
                move |cmd| s.player(cmd),
                move |req| {
                    let ui_weak = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || match req {
                        os_media::UiRequest::Raise => {
                            if let Some(w) = ui_weak.upgrade() {
                                raise(&w);
                            }
                        }
                        os_media::UiRequest::Quit => {
                            let _ = slint::quit_event_loop();
                        }
                    });
                },
            );
            if done {
                stop();
            } else if attempts.get() >= 100 {
                tracing::warn!("media controls: no native window handle, giving up");
                stop();
            }
        },
    );

    window.run()?;

    let _ = cmd_tx.blocking_send(Command::Shutdown);
    // Dropping the window and the senders closes the player and library channels.
    drop(window);
    drop(senders);
    drop(tick);
    runtime.block_on(async {
        let wait = Duration::from_secs(2);
        let _ = tokio::time::timeout(wait, backend).await;
        let _ = tokio::time::timeout(wait, player).await;
        let _ = tokio::time::timeout(wait, library).await;
        let _ = tokio::time::timeout(wait, connect).await;
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}

#[cfg(debug_assertions)]
fn preview(w: &AppWindow) {
    use tonearm_core::library::{CollectionKind, PageHeader, PlaylistSummary, Tile, TrackItem};
    let screen = std::env::var("TONEARM_UI_PREVIEW").unwrap_or_default();
    w.set_auth_view(match screen.as_str() {
        "login" => AuthView::LoggedOut,
        "waiting" => AuthView::Waiting,
        _ => AuthView::LoggedIn,
    });
    w.set_waited_secs(42);
    w.set_username("ЕГОР".into());
    if screen == "offline" {
        w.set_offline(true);
        w.set_retry_in_secs(8);
    }
    let lists = [
        "Ночные маршруты",
        "Утренний кофе",
        "Фокус без слов",
        "Дорога на север",
    ]
    .iter()
    .enumerate()
    .map(|(i, n)| PlaylistSummary {
        uri: format!("spotify:playlist:{i}"),
        name: (*n).into(),
        owner: "Егор".into(),
        track_count: 40 + u32::try_from(i).unwrap_or(0) * 17,
        image_url: None,
    })
    .collect();
    let fx = ui_state::with(|st| {
        let mut fx = st.on_library(w, LibraryEvent::Playlists(lists));
        fx.extend(st.on_library(w, LibraryEvent::LikedCount(1284)));
        let tiles = [
            "Холодный свет",
            "Paper Boats",
            "Маяк",
            "Депо",
            "Сезоны",
            "Kite Harbor",
        ]
        .iter()
        .map(|t| Tile {
            uri: format!("spotify:album:{t}"),
            kind: CollectionKind::Album,
            title: (*t).into(),
            subtitle: "Исполнитель".into(),
            image_url: None,
        })
        .collect();
        fx.extend(st.on_library(w, LibraryEvent::Home(tiles)));
        if matches!(screen.as_str(), "playlist" | "queue" | "menu") {
            fx.extend(st.open(w, PageRequest::Playlist("spotify:playlist:0".into()), true));
            fx.extend(st.on_library(
                w,
                LibraryEvent::PageHeader {
                    page_id: 0,
                    header: PageHeader {
                        kind: CollectionKind::Playlist,
                        uri: "spotify:playlist:0".into(),
                        title: "Ночные маршруты".into(),
                        byline: "Егор".into(),
                        byline_uri: None,
                        year: None,
                        total_tracks: Some(7),
                        image_url: None,
                    },
                },
            ));
            let names = [
                ("Огни над рекой", "Северный Ветер", 221),
                ("Тёплый асфальт", "Лиса Мороз", 242),
                ("Half Light", "Kite Harbor", 198),
                ("Шум прибоя", "Тихий Порт", 307),
                ("Северное сияние", "Мира Лес", 235),
                ("Signal Lost", "Neon Fjord", 266),
                ("Последний трамвай", "Пыль и Медь", 179),
            ];
            let tracks = names
                .iter()
                .enumerate()
                .map(|(i, (t, a, d))| TrackItem {
                    uri: format!("spotify:track:{i}"),
                    title: (*t).into(),
                    artists: (*a).into(),
                    duration_ms: d * 1000,
                    playable: i != 5,
                    ..TrackItem::default()
                })
                .collect();
            fx.extend(st.on_library(
                w,
                LibraryEvent::PageTracks {
                    page_id: 0,
                    tracks,
                    done: true,
                },
            ));
        }
        let track = tonearm_player::TrackInfo {
            uri: "spotify:track:0".into(),
            title: "Огни над рекой".into(),
            artists: "Северный Ветер".into(),
            album: "Ночные маршруты".into(),
            duration_ms: 221_000,
            cover_url: None,
        };
        fx.extend(st.on_track(w, &track));
        // Feed the OS media controls too, once they are registered (after the event loop starts).
        slint::Timer::single_shot(Duration::from_millis(300), move || {
            os_media::on_track(&track, true, false);
            os_media::on_state(tonearm_player::PlaybackState::Playing, 84_000);
        });
        fx
    });
    drop(fx);
    if screen == "library" {
        w.set_page(Page::Library);
    }
    if screen == "settings" {
        w.set_page(Page::Settings);
        w.set_username("nightowl".into());
        w.set_cache_used_mb(214);
        let devices: Vec<slint::SharedString> =
            vec!["Built-in Audio Analog Stereo".into(), "USB DAC".into()];
        w.set_audio_devices(slint::ModelRc::new(slint::VecModel::from(devices)));
    }
    if screen == "queue" || screen == "menu" {
        use tonearm_player::{QueueItem, QueueSnapshot, RepeatMode};
        let item = |t: &str, a: &str| QueueItem {
            uri: format!("spotify:track:{t}"),
            title: t.into(),
            artists: a.into(),
            duration_ms: 200_000,
        };
        let snap = QueueSnapshot {
            context_name: "Ночные маршруты".into(),
            upcoming: vec![
                item("Метро в 6:10", "Лиса Мороз"),
                item("Half Light", "Kite Harbor"),
                item("Тёплый асфальт", "Лиса Мороз"),
                item("Шум прибоя", "Тихий Порт"),
                item("Северное сияние", "Мира Лес"),
                item("Последний трамвай", "Пыль и Медь"),
            ],
            manual: 2,
            shuffle: true,
            repeat: RepeatMode::All,
        };
        ui_state::with(|st| st.on_queue(w, &snap));
        w.set_queue_open(true);
    }
    if screen == "devices" {
        use tonearm_core::connect::Device;
        let dev = |id: &str, name: &str, kind: &str, active: bool| Device {
            id: id.into(),
            name: name.into(),
            kind: kind.into(),
            is_active: active,
            ..Device::default()
        };
        ui_state::with(|st| {
            st.on_devices(
                w,
                vec![
                    dev("a", "Pixel 8", "Smartphone", false),
                    dev("b", "Кухня", "Speaker", true),
                    dev("c", "Ноутбук", "Computer", false),
                ],
            );
        });
        w.set_remote_name("КУХНЯ".into());
        let weak = w.as_weak();
        slint::Timer::single_shot(Duration::from_millis(500), move || {
            if let Some(w) = weak.upgrade() {
                w.invoke_show_devices();
            }
        });
    }
    if screen == "menu" {
        // After the first layout, so the window has its size.
        let weak = w.as_weak();
        slint::Timer::single_shot(Duration::from_millis(500), move || {
            if let Some(w) = weak.upgrade() {
                w.invoke_show_row_menu(0, 3, 640.0, 600.0);
            }
        });
    }
    if screen.starts_with("search") {
        preview_search(w, &screen);
    }
    w.set_position_ms(84_000);
    w.set_play_view(PlayView::Playing);
}

#[cfg(debug_assertions)]
fn preview_search(w: &AppWindow, screen: &str) {
    use tonearm_core::library::{CollectionKind, SearchResults, Tile, TrackItem};
    let tile = |kind, title: &str, sub: &str| Tile {
        uri: format!("spotify:x:{title}"),
        kind,
        title: title.into(),
        subtitle: sub.into(),
        image_url: None,
    };
    let tracks = [
        ("Ночной поезд", "Северный Ветер", 192),
        ("Огни над рекой", "Северный Ветер", 221),
        ("Ночник", "Ола Ринг", 167),
        ("Огни ночного города", "Пыль и Медь", 238),
        ("Ночь в порту", "Тихий Порт", 280),
        ("Ночные окна", "Мира Лес", 201),
        ("Ночная смена", "Мира Лес", 245),
    ]
    .iter()
    .enumerate()
    .map(|(i, (t, a, d))| TrackItem {
        uri: if i == 1 {
            "spotify:track:0".into()
        } else {
            format!("spotify:track:s{i}")
        },
        title: (*t).into(),
        artists: (*a).into(),
        duration_ms: d * 1000,
        playable: true,
        ..TrackItem::default()
    })
    .collect();
    let results = SearchResults {
        tracks,
        albums: [
            "Ночные маршруты",
            "Ночь",
            "Noctua",
            "Ночной эфир",
            "Night Ferry",
            "После полуночи",
        ]
        .iter()
        .map(|t| tile(CollectionKind::Album, t, "Северный Ветер · 2024"))
        .collect(),
        artists: ["Ночные Огни", "Night Ferry", "Noctua", "Ночь"]
            .iter()
            .map(|t| tile(CollectionKind::Artist, t, ""))
            .collect(),
        playlists: ["Ночные поездки", "Ночной джаз", "Ночь и дождь"]
            .iter()
            .map(|t| tile(CollectionKind::Playlist, t, "Егор"))
            .collect(),
        more: [true, true, false, false],
    };
    w.set_page(Page::Search);
    w.set_search_query("Ночн".into());
    if screen == "search-tracks" {
        w.set_search_tab(1);
    }
    ui_state::with(|st| {
        drop(st.search_edited(w, "Ночн"));
        drop(st.on_library(
            w,
            LibraryEvent::Search {
                query: "Ночн".into(),
                kind: None,
                offset: 0,
                results,
            },
        ));
    });
}
