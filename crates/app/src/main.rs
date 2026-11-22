// No console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod history;
mod mini;
mod os_media;
mod ru;
mod smart;
mod themes;
mod ui_state;
mod updater;
mod wrapped;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::Model;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tonearm_core::config::{BITRATES, CACHE_SIZES_MB, FADE_SECS, Settings};
use tonearm_core::connect::{ConnectCommand, ConnectEvent, RemoteAction};
use tonearm_core::covers::CoverCache;
use tonearm_core::lastfm::{LastFmCommand, LastFmEvent};
use tonearm_core::library::{LibraryCommand, LibraryEvent, PageRequest};
use tonearm_core::{AuthState, Command, Event, Notice};
use tonearm_player::effects::{self, EffectsOptions};
use tonearm_player::{AudioOptions, PlayerCommand, PlayerEvent, SoloistConfig, SoloistStatus};
use ui_state::{CoverTarget, Effect, UiState};

slint::include_modules!();

/// How long an informational notice stays up.
const NOTICE_SECS: u32 = 5;
/// The library is fetched again on coming back to the window, at most this often.
const LIBRARY_REFRESH: Duration = Duration::from_secs(300);

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let mut filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // librespot logs raw OAuth/token responses at trace level: cap these targets whatever RUST_LOG says.
    for directive in ["librespot_oauth=info", "librespot_core=info"] {
        if let Ok(d) = directive.parse() {
            filter = filter.add_directive(d);
        }
    }
    // Also log to a file: a Windows release build has no console, so this is the only trace.
    let file = log_path().and_then(|p| open_log(&p));
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

/// Starts a fresh log, keeping the previous run's as `tonearm.log.1` (after a crash that one is
/// what tells what happened). Owner-only: track names and account ids are in it.
fn open_log(path: &std::path::Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    let _ = std::fs::rename(path, path.with_extension("log.1"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).ok()
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

/// Runs one of the background services. If it panics, the rest of the app keeps going, so the
/// user is told instead of seeing buttons that silently do nothing (the panic itself is logged
/// by the panic hook, with a backtrace).
fn watched(
    rt: &tokio::runtime::Runtime,
    name: &'static str,
    window: &slint::Weak<AppWindow>,
    service: impl std::future::Future<Output = ()> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let inner = rt.spawn(service);
    let window = window.clone();
    rt.spawn(async move {
        if let Err(e) = inner.await
            && e.is_panic()
        {
            tracing::error!(service = name, "background service crashed");
            let _ = window.upgrade_in_event_loop(move |w| {
                w.set_notice(
                    format!("Сбой: {name} перестал работать. Перезапустите Tonearm, а в журнале tonearm.log есть подробности.")
                        .into(),
                );
                w.set_notice_is_error(true);
            });
        }
    })
}

/// Keyring entry of the user's own Soloist key (from developer.spotify.com).
const SOLOIST_KEY: &str = "soloist-api-key";

fn soloist_dirs() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let dirs = directories::ProjectDirs::from("", "", "tonearm")?;
    Some((
        dirs.data_local_dir().join("soloist"),
        dirs.cache_dir().join("soloist"),
    ))
}

fn soloist_config(api_key: String) -> Option<SoloistConfig> {
    let (root, cache_dir) = soloist_dirs()?;
    Some(SoloistConfig {
        api_key,
        root,
        cache_dir,
        device_name: tonearm_player::soloist::DEVICE_NAME.into(),
        pipewire_device: None,
    })
}

/// How long "ПОЛУЧИТЬ КЛЮЧ" waits for the key to be copied.
const KEY_WATCH_SECS: u32 = 180;

/// What Spotify's "Copy" button puts on the clipboard: one token of printable characters.
/// The format is not documented, so stay loose but never take ordinary text or a link.
fn looks_like_key(text: &str) -> bool {
    let t = text.trim();
    (20..=512).contains(&t.len())
        && t.bytes().all(|b| b.is_ascii_graphic())
        && t.bytes().any(|b| b.is_ascii_digit())
        && t.bytes().any(|b| b.is_ascii_alphabetic())
        && !t.contains("://")
}

/// Settings line for Soloist's state, and whether "ПОВТОРИТЬ" helps.
fn soloist_text(status: &SoloistStatus) -> (String, String, bool) {
    match status {
        SoloistStatus::Off => ("ВЫКЛЮЧЕН".into(), String::new(), false),
        SoloistStatus::Downloading(p) => (
            format!("СКАЧИВАЮ С СЕРВЕРА SPOTIFY · {p}%"),
            String::new(),
            false,
        ),
        SoloistStatus::Starting => ("ЗАПУСКАЮ…".into(), String::new(), false),
        SoloistStatus::Pairing => ("ВХОЖУ В ВАШ АККАУНТ…".into(), String::new(), false),
        SoloistStatus::NeedsPairing { reason } => (
            "НУЖНА ПРИВЯЗКА · ОДИН РАЗ".into(),
            format!(
                "Не получилось войти автоматически ({reason}). Откройте Spotify на телефоне в той же \
                 Wi-Fi сети и выберите устройство «{}» в списке устройств. Больше это не понадобится.",
                tonearm_player::soloist::DEVICE_NAME
            ),
            true,
        ),
        SoloistStatus::Ready { version, days_left } => (
            format!("ГОТОВ · {version} · ОБНОВЛЯЕТСЯ САМ ({days_left} ДН.)"),
            String::new(),
            false,
        ),
        SoloistStatus::Failed(message) => ("ОШИБКА".into(), message.clone(), true),
    }
}

/// Senders for everything the UI can ask for.
#[derive(Clone)]
struct Senders {
    auth: mpsc::Sender<Command>,
    player: mpsc::Sender<PlayerCommand>,
    library: mpsc::Sender<LibraryCommand>,
    connect: mpsc::Sender<ConnectCommand>,
    lastfm: mpsc::Sender<LastFmCommand>,
    /// Spotify Connect: id of the remote device that plays now (`None` = this computer).
    remote: Arc<Mutex<Option<String>>>,
    remote_playing: Arc<AtomicBool>,
    covers: Arc<CoverCache>,
    /// What `config.toml` holds; the settings page edits it.
    settings: Arc<Mutex<Settings>>,
    /// A clipboard watch for the Soloist key is running.
    key_watch: Arc<AtomicBool>,
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
        if new.effects != current.effects {
            self.local(PlayerCommand::Effects(effects_options(&new)));
        }
        if new.player != current.player {
            if new.player.soloist {
                self.start_soloist();
            } else {
                self.local(PlayerCommand::Soloist(None));
            }
        }
        if new.lastfm.scrobble != current.lastfm.scrobble {
            self.lastfm_cmd(LastFmCommand::SetEnabled(new.lastfm.scrobble));
            let on = new.lastfm.scrobble && w.get_lastfm_state() == 3;
            ui_state::with(|st| st.lastfm_on = on);
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
        new.save_later();
    }

    /// Starts Soloist with the saved key, or asks for one.
    fn start_soloist(&self) {
        let s = self.clone();
        self.rt.spawn_blocking(move || {
            let key = tonearm_core::system_store().get(SOLOIST_KEY).ok().flatten();
            match key.and_then(|k| soloist_config(k.expose().to_owned())) {
                Some(cfg) => {
                    s.local(PlayerCommand::Soloist(Some(cfg)));
                    let _ = s
                        .window
                        .upgrade_in_event_loop(|w| w.set_soloist_need_key(false));
                }
                None => {
                    let _ = s.window.upgrade_in_event_loop(|w| {
                        w.set_soloist_need_key(true);
                        w.set_soloist_status("НУЖЕН КЛЮЧ · ОДИН РАЗ".into());
                        w.set_soloist_detail(
                            "Нажмите «Получить ключ»: откроется страница Spotify (нужен Premium). \
                             Там нажмите «Copy», Tonearm сам заберёт ключ из буфера обмена."
                                .into(),
                        );
                    });
                }
            }
        });
    }

    /// Saves a key the user pasted or copied, then starts Soloist with it.
    fn use_soloist_key(&self, key: String) {
        let s = self.clone();
        self.rt.spawn_blocking(move || {
            let store = tonearm_core::system_store();
            if let Err(e) = store.set(SOLOIST_KEY, &tonearm_core::Secret::new(key)) {
                tracing::warn!(error = %e, "cannot save the soloist key");
                let _ = s.window.upgrade_in_event_loop(|w| {
                    w.set_soloist_status("КЛЮЧ НЕ СОХРАНИЛСЯ".into());
                    w.set_soloist_detail("Хранилище паролей системы недоступно.".into());
                });
                return;
            }
            s.start_soloist();
        });
    }

    /// After "ПОЛУЧИТЬ КЛЮЧ": wait (up to three minutes) for the next clipboard change, which is
    /// the user pressing "Copy" on the key page. Only that one change is looked at: whatever is
    /// copied later (a password from a password manager, say) is never read as the key.
    fn watch_clipboard_for_key(&self) {
        if self.key_watch.swap(true, Ordering::SeqCst) {
            return;
        }
        let s = self.clone();
        std::thread::spawn(move || {
            let changed = (|| {
                let mut clipboard = arboard::Clipboard::new().ok()?;
                let before = clipboard.get_text().ok();
                for _ in 0..KEY_WATCH_SECS {
                    std::thread::sleep(Duration::from_secs(1));
                    match clipboard.get_text() {
                        Ok(text) if Some(&text) != before.as_ref() => return Some(text),
                        _ => {}
                    }
                }
                None
            })();
            s.key_watch.store(false, Ordering::SeqCst);
            match changed {
                Some(text) if looks_like_key(&text) => {
                    tracing::info!("soloist key taken from the clipboard");
                    s.use_soloist_key(text.trim().to_owned());
                }
                Some(_) => {
                    tracing::info!("clipboard changed, but not to a key");
                    let _ = s.window.upgrade_in_event_loop(|w| {
                        w.set_soloist_detail(
                            "В буфере обмена не ключ. Скопируйте ключ ещё раз и вставьте его в поле.".into(),
                        );
                    });
                }
                None => {}
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
                    PlayerCommand::Resume => ConnectCommand::Control(RemoteAction::Play),
                    PlayerCommand::Next => ConnectCommand::Control(RemoteAction::Next),
                    PlayerCommand::Prev => ConnectCommand::Control(RemoteAction::Prev),
                    PlayerCommand::Seek { position_ms } => {
                        ConnectCommand::Control(RemoteAction::Seek(position_ms))
                    }
                    PlayerCommand::SetVolume(p) => ConnectCommand::Control(RemoteAction::Volume(p)),
                    PlayerCommand::PlayContext {
                        items,
                        start,
                        position_ms,
                        ..
                    } => ConnectCommand::PlayOn {
                        device_id,
                        uris: items.into_iter().map(|i| i.uri).collect(),
                        offset: start,
                        position_ms,
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

    /// Sends to the local player. While it is busy, a newer volume or seek replaces an older
    /// one anyway, so those may be dropped; anything else (pause, next, a track to play) is
    /// delivered as soon as there is room.
    fn local(&self, cmd: PlayerCommand) {
        match self.player.try_send(cmd) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(
                PlayerCommand::SetVolume(_) | PlayerCommand::Seek { .. },
            )) => tracing::debug!("player busy, volume/seek dropped"),
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                let tx = self.player.clone();
                self.rt.spawn(async move {
                    let _ = tx.send(cmd).await;
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    fn lastfm_cmd(&self, cmd: LastFmCommand) {
        if self.lastfm.try_send(cmd).is_err() {
            tracing::debug!("last.fm busy, command dropped");
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
                Effect::Library(cmd) => {
                    if self.library.try_send(cmd).is_err() {
                        tracing::warn!("library busy");
                    }
                }
                Effect::Save(store) => {
                    if std::env::var_os("TONEARM_UI_PREVIEW").is_none() {
                        store.save_later();
                    }
                }
                Effect::SaveResume(r) => {
                    if std::env::var_os("TONEARM_UI_PREVIEW").is_none() {
                        history::save_resume(Some(*r));
                    }
                }
                Effect::LogPlay(e) => {
                    if std::env::var_os("TONEARM_UI_PREVIEW").is_none() {
                        self.rt.spawn_blocking(move || {
                            if let Some(log) = history::PlayLog::open()
                                && let Err(err) = log.append(&e)
                            {
                                tracing::warn!(error = %err, "cannot write the play log");
                            }
                        });
                    }
                }
                Effect::SaveRules(rules) => {
                    if std::env::var_os("TONEARM_UI_PREVIEW").is_none() {
                        smart::save_later(rules);
                    }
                }
                Effect::LastFm(cmd) => self.lastfm_cmd(cmd),
                Effect::Smart {
                    token,
                    rule,
                    liked,
                    plays,
                } => {
                    let s = self.clone();
                    self.rt.spawn_blocking(move || {
                        let plays = plays.unwrap_or_else(|| {
                            history::PlayLog::open().map_or_else(Vec::new, |l| l.read_all())
                        });
                        let tracks = smart::evaluate(&rule, &liked, &plays, history::now());
                        drop(plays);
                        let weak = s.window.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let fx = ui_state::with(|st| st.on_smart(&w, token, tracks));
                                s.run(fx);
                            }
                        });
                    });
                }
                Effect::Wrapped { year, plays } => {
                    let s = self.clone();
                    self.rt.spawn_blocking(move || {
                        let (years, entries) = match plays {
                            Some(all) => {
                                let mut years: Vec<i32> =
                                    all.iter().map(|e| history::year_of(e.at)).collect();
                                years.sort_unstable_by(|a, b| b.cmp(a));
                                years.dedup();
                                (years, all)
                            }
                            None => {
                                let log = history::PlayLog::open();
                                let years = log.as_ref().map(history::PlayLog::years);
                                let years = years.unwrap_or_default();
                                let pick = if year == 0 {
                                    years.first().copied()
                                } else {
                                    Some(year)
                                };
                                let entries = match (&log, pick) {
                                    (Some(l), Some(y)) => l.plays_in(y),
                                    _ => Vec::new(),
                                };
                                (years, entries)
                            }
                        };
                        let year = if year == 0 {
                            years
                                .first()
                                .copied()
                                .unwrap_or_else(|| history::year_of(history::now()))
                        } else {
                            year
                        };
                        let summary = wrapped::summary(&entries, year);
                        drop(entries);
                        let weak = s.window.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let fx = ui_state::with(|st| st.on_wrapped(&w, &summary, &years));
                                s.run(fx);
                            }
                        });
                    });
                }
            }
        }
    }

    fn load_cover(&self, url: String, size: u32, target: CoverTarget) {
        // Sizes are in logical pixels: decode for the screen's density (sharp on HiDPI), up to
        // 2× so a 4K screen does not cost four times the memory.
        let scale = self
            .window
            .upgrade()
            .map_or(1.0, |w| w.window().scale_factor())
            .clamp(1.0, 2.0);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let size = ((size as f32) * scale).round() as u32;
        let senders = self.clone();
        let covers = Arc::clone(&self.covers);
        let weak = self.window.clone();
        #[cfg(debug_assertions)]
        if let Some(seed) = url.strip_prefix("preview:") {
            let cover = preview_cover(seed, size.min(400));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    ui_state::with(|st| st.on_cover(&w, &target, &cover));
                }
            });
            return;
        }
        self.rt.spawn(async move {
            match covers.get(&url, size).await {
                Ok(cover) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            ui_state::with(|st| st.on_cover(&w, &target, &cover));
                        }
                    });
                }
                Err(e) => {
                    tracing::info!(%e, %url, "cover not loaded");
                    // The player's cover for the playing track failed: try the album's.
                    if let CoverTarget::Deck(uri) = target {
                        let s = senders;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let fx = ui_state::with(|st| st.deck_cover_failed(&w, &uri, &url));
                                s.run(fx);
                            }
                        });
                    }
                }
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
                    w.set_username(username.into());
                    w.set_notice("".into());
                    w.set_offline(false);
                    let fx = ui_state::with(|st| st.show_resume(w));
                    s.run(fx);
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
    apply_player_event(w, s, event);
    mini::sync(w);
}

fn apply_player_event(w: &AppWindow, s: &Senders, event: PlayerEvent) {
    // While a Connect device plays, the deck shows that device, not the paused local player.
    let remote = s.remote.lock().ok().is_some_and(|r| r.is_some());
    if remote
        && matches!(
            event,
            PlayerEvent::State { .. } | PlayerEvent::Track(_) | PlayerEvent::Started(_)
        )
    {
        return;
    }
    match event {
        PlayerEvent::State { state, position_ms } => {
            os_media::on_state(state, position_ms);
            w.set_play_view(ui_state::play_view(state));
            w.set_position_ms(i32::try_from(position_ms).unwrap_or(i32::MAX));
            ui_state::with(|st| st.position_changed(w));
            lyrics_kick(w);
        }
        PlayerEvent::Queue(q) => ui_state::with(|st| st.on_queue(w, &q)),
        PlayerEvent::Started(uri) => {
            let fx = ui_state::with(|st| st.on_started(w, &uri));
            s.run(fx);
        }
        PlayerEvent::Track(track) => {
            os_media::on_track(&track, window_focused(w), s.notifications());
            let fx = ui_state::with(|st| st.on_track(w, &track));
            s.run(fx);
        }
        PlayerEvent::Volume(percent) => {
            os_media::on_volume(percent);
            w.set_volume_level(i32::from(percent));
        }
        PlayerEvent::Error(text) => {
            w.set_notice(text.into());
            w.set_notice_is_error(true);
        }
        PlayerEvent::AudioKeyRefused => {
            w.set_notice(
                "Spotify не выдаёт этому аккаунту ключи для встроенного плеера (так бывает с новыми \
                 аккаунтами). Включите Soloist: Настройки → Плеер."
                    .into(),
            );
            w.set_notice_is_error(true);
        }
        PlayerEvent::Spectrum(frame) => {
            let bars = w.get_spectrum();
            for (i, v) in frame.iter().enumerate() {
                let v = f32::from(*v) / 255.0;
                if bars
                    .row_data(i)
                    .is_some_and(|old| (old - v).abs() > f32::EPSILON)
                {
                    bars.set_row_data(i, v);
                }
            }
        }
        PlayerEvent::Effects(status) => w.set_effects_status(status.unwrap_or_default().into()),
        PlayerEvent::Soloist(status) => {
            let (line, detail, retry) = soloist_text(&status);
            w.set_soloist_status(line.into());
            w.set_soloist_detail(detail.into());
            w.set_soloist_can_retry(retry);
            if matches!(status, SoloistStatus::Ready { .. }) {
                w.set_soloist_need_key(false);
            }
            // A refused key: ask for a new one.
            if let SoloistStatus::Failed(m) = &status
                && m.contains("ключ")
            {
                w.set_soloist_need_key(true);
            }
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
            w.set_remote_name(state.device.name.clone().into());
            s.remote_playing.store(state.is_playing, Ordering::Relaxed);
            w.set_play_view(if state.is_playing {
                PlayView::Playing
            } else {
                PlayView::Paused
            });
            w.set_position_ms(i32::try_from(state.progress_ms).unwrap_or(0));
            ui_state::with(|st| st.position_changed(w));
            if let Some(v) = state.device.volume_percent {
                w.set_volume_level(i32::from(v));
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
        ConnectEvent::HandoffFailed(text) => {
            // The app switched to "remote" before asking; go back to this computer.
            s.set_remote(None);
            s.remote_playing.store(false, Ordering::Relaxed);
            w.set_remote_name("".into());
            s.local(PlayerCommand::Resume);
            w.set_notice(text.into());
            w.set_notice_is_error(true);
        }
        ConnectEvent::Error(text) => {
            w.set_devices_loading(false);
            // The preview has no session, so every Connect call fails; keep its screens clean.
            if std::env::var_os("TONEARM_UI_PREVIEW").is_some() {
                return;
            }
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

fn apply_lastfm(w: &AppWindow, _s: &Senders, event: LastFmEvent) {
    #[cfg(debug_assertions)]
    if std::env::var_os("TONEARM_UI_PREVIEW").is_some() {
        return;
    }
    ui_state::with(|st| st.on_lastfm(w, event));
}

fn apply_library(w: &AppWindow, s: &Senders, event: LibraryEvent) {
    // The preview has no session, so real library calls fail; keep its screens clean.
    if std::env::var_os("TONEARM_UI_PREVIEW").is_some()
        && matches!(
            event,
            LibraryEvent::PageError { .. }
                | LibraryEvent::SearchError { .. }
                | LibraryEvent::Error(_)
        )
    {
        return;
    }
    let fx = ui_state::with(|st| st.on_library(w, event));
    s.run(fx);
    lyrics_kick(w);
}

thread_local! {
    static LYRICS_TIMER: slint::Timer = slint::Timer::default();
}

/// Moves the lyrics highlight on and sets a timer for exactly the next word, so an open lyrics
/// screen costs a redraw per word instead of one per frame.
fn lyrics_kick(w: &AppWindow) {
    let next = ui_state::with(|st| st.lyrics_step(w));
    LYRICS_TIMER.with(|t| match next {
        Some(delay) => {
            let weak = w.as_weak();
            t.start(slint::TimerMode::SingleShot, delay, move || {
                if let Some(w) = weak.upgrade() {
                    lyrics_kick(&w);
                }
            });
        }
        None => t.stop(),
    });
}

/// "Доброе утро" … by the local hour.
fn greeting() -> String {
    use chrono::Timelike;
    match chrono::Local::now().hour() {
        5..=11 => "Доброе утро",
        12..=17 => "Добрый день",
        18..=23 => "Добрый вечер",
        _ => "Доброй ночи",
    }
    .to_owned()
}

/// "Четверг, 19 ноября" in local time.
fn date_label() -> String {
    use chrono::Datelike;
    const MONTHS: [&str; 12] = [
        "января",
        "февраля",
        "марта",
        "апреля",
        "мая",
        "июня",
        "июля",
        "августа",
        "сентября",
        "октября",
        "ноября",
        "декабря",
    ];
    const WEEKDAYS: [&str; 7] = [
        "Понедельник",
        "Вторник",
        "Среда",
        "Четверг",
        "Пятница",
        "Суббота",
        "Воскресенье",
    ];
    let now = chrono::Local::now();
    let month = MONTHS[usize::try_from(now.month0()).unwrap_or(0)];
    let weekday = WEEKDAYS[usize::try_from(now.weekday().num_days_from_monday()).unwrap_or(0)];
    format!("{weekday}, {} {month}", now.day())
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

fn effects_options(s: &Settings) -> EffectsOptions {
    let fx = &s.effects;
    let gains = effects::preset(&fx.eq_preset).map_or_else(
        || {
            let mut g = [0i8; 10];
            for (d, v) in g.iter_mut().zip(&fx.eq_gains) {
                *d = *v;
            }
            g
        },
        |p| p.gains,
    );
    EffectsOptions {
        eq: fx.eq.then_some(gains),
        fade_ms: u32::from(fx.fade_secs) * 1000,
        visualizer: fx.visualizer,
    }
}

/// The equalizer's preset list: the built-in ones, then «Свой» (the user's own bands).
fn eq_preset_index(id: &str) -> usize {
    effects::PRESETS
        .iter()
        .position(|p| p.id == id)
        .unwrap_or(effects::PRESETS.len())
}

fn set_eq_gains(w: &AppWindow, gains: &[i8]) {
    let model: Vec<i32> = gains.iter().map(|&g| i32::from(g)).collect();
    w.set_eq_gains(slint::ModelRc::new(slint::VecModel::from(model)));
}

fn index_of<T: PartialEq>(values: &[T], value: &T, default: usize) -> i32 {
    let i = values.iter().position(|v| v == value).unwrap_or(default);
    i32::try_from(i).unwrap_or(0)
}

/// Opens the themes folder (made on first use, with a short readme) in the file manager.
fn open_themes_folder() {
    let Some(dir) = themes::dir() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_ok() {
        let readme = dir.join("README.txt");
        if !readme.exists() {
            let _ = std::fs::write(&readme, themes::README);
        }
    }
    if let Err(e) = open::that_detached(&dir) {
        tracing::warn!(error = %e, "cannot open the themes folder");
    }
}

/// Settings → the settings page.
fn show_settings(w: &AppWindow, s: &Settings) {
    w.set_set_bitrate(index_of(&BITRATES, &s.audio.bitrate, 2));
    w.set_set_normalisation(i32::from(s.audio.normalisation));
    w.set_set_gapless(i32::from(s.audio.gapless));
    w.set_set_notifications(i32::from(s.ui.notifications));
    w.set_lyrics_mode(i32::from(s.ui.lyrics_words));
    w.set_set_lyrics_backdrop(i32::from(s.ui.lyrics_backdrop.min(2)));
    w.set_set_lyrics_layout(i32::from(s.ui.lyrics_layout.min(1)));
    w.set_set_device(s.audio.device.as_str().into());
    // A hand-edited size that is not one of the steps shows as the nearest larger step.
    let cache = CACHE_SIZES_MB
        .iter()
        .position(|&mb| mb >= s.cache.max_size_mb)
        .unwrap_or(CACHE_SIZES_MB.len() - 1);
    w.set_set_cache(i32::try_from(cache).unwrap_or(2));
    w.set_set_engine(i32::from(s.player.soloist));
    w.set_set_update_check(i32::from(s.update.check));
    w.set_effects_supported(effects::supported());
    let names: Vec<slint::SharedString> = effects::PRESETS
        .iter()
        .map(|p| p.name.into())
        .chain(std::iter::once("Свой".into()))
        .collect();
    w.set_eq_preset_names(slint::ModelRc::new(slint::VecModel::from(names)));
    w.set_set_eq(i32::from(s.effects.eq));
    let preset = eq_preset_index(&s.effects.eq_preset);
    w.set_set_eq_preset(i32::try_from(preset).unwrap_or(0));
    match effects::PRESETS.get(preset) {
        Some(p) => set_eq_gains(w, &p.gains),
        None => set_eq_gains(w, &s.effects.eq_gains),
    }
    w.set_set_fade(index_of(&FADE_SECS, &s.effects.fade_secs, 0));
    w.set_set_visualizer(i32::from(s.effects.visualizer));
    w.set_set_scrobble(i32::from(s.lastfm.scrobble));
    w.set_smart_collapsed(s.ui.smart_collapsed);
}

/// The settings page → Settings (keeps what the page does not show, like the Client ID).
fn read_settings(w: &AppWindow, mut s: Settings) -> Settings {
    let pick = |i: i32, len: usize| usize::try_from(i).unwrap_or(0).min(len - 1);
    s.audio.bitrate = BITRATES[pick(w.get_set_bitrate(), BITRATES.len())];
    s.audio.normalisation = w.get_set_normalisation() != 0;
    s.audio.gapless = w.get_set_gapless() != 0;
    s.audio.device = w.get_set_device().to_string();
    s.ui.notifications = w.get_set_notifications() != 0;
    s.ui.lyrics_words = w.get_lyrics_mode() == 1;
    s.ui.lyrics_backdrop = u8::try_from(w.get_set_lyrics_backdrop())
        .unwrap_or(1)
        .min(2);
    s.ui.lyrics_layout = u8::try_from(w.get_set_lyrics_layout()).unwrap_or(0).min(1);
    s.ui.theme = w.get_theme_id().to_string();
    s.cache.max_size_mb = CACHE_SIZES_MB[pick(w.get_set_cache(), CACHE_SIZES_MB.len())];
    s.player.soloist = w.get_set_engine() == 1;
    s.update.check = w.get_set_update_check() != 0;
    s.effects.eq = w.get_set_eq() != 0;
    let preset = usize::try_from(w.get_set_eq_preset()).unwrap_or(0);
    s.effects.eq_preset = effects::PRESETS
        .get(preset)
        .map_or("custom", |p| p.id)
        .into();
    s.effects.eq_gains = w
        .get_eq_gains()
        .iter()
        .map(|g| i8::try_from(g.clamp(-12, 12)).unwrap_or(0))
        .collect();
    s.effects.fade_secs = FADE_SECS[pick(w.get_set_fade(), FADE_SECS.len())];
    s.effects.visualizer = w.get_set_visualizer() != 0;
    s.lastfm.scrobble = w.get_set_scrobble() != 0;
    s.ui.smart_collapsed = w.get_smart_collapsed();
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
    let (lcmd_tx, lcmd_rx) = mpsc::channel::<LibraryCommand>(64);
    let (lev_tx, lev_rx) = mpsc::channel::<LibraryEvent>(64);
    let (ccmd_tx, ccmd_rx) = mpsc::channel::<ConnectCommand>(16);
    let (cev_tx, cev_rx) = mpsc::channel::<ConnectEvent>(16);
    let (fcmd_tx, fcmd_rx) = mpsc::channel::<LastFmCommand>(64);
    let (fev_tx, fev_rx) = mpsc::channel::<LastFmEvent>(16);
    // The core publishes the connected session; player and library pick it up.
    let (session_tx, session_rx) = watch::channel(None);

    // Remember where the binary is and drop leftovers of the last update.
    tonearm_core::update::init();
    let settings = Settings::load();
    tonearm_core::library::use_own_client_id(
        &settings.spotify.client_id,
        tonearm_core::system_store(),
    );
    let window = AppWindow::new()?;
    ui_state::init(&window);
    ui_state::wrapped_rows(&window);
    show_settings(&window, &settings);
    // TONEARM_THEME=<id> overrides the saved theme (screenshots of every theme).
    let theme = std::env::var("TONEARM_THEME").unwrap_or_else(|_| settings.ui.theme.clone());
    ui_state::with(|st| st.load_themes(&window, &theme));
    window.set_version(env!("CARGO_PKG_VERSION").into());
    window.set_date_label(date_label().into());
    window.set_greeting(greeting().into());

    let senders = Senders {
        auth: cmd_tx.clone(),
        player: pcmd_tx,
        connect: ccmd_tx,
        lastfm: fcmd_tx,
        remote: Arc::new(Mutex::new(None)),
        remote_playing: Arc::new(AtomicBool::new(false)),
        library: lcmd_tx,
        covers: Arc::new(CoverCache::new(mb_to_bytes(settings.cache.max_size_mb))),
        settings: Arc::new(Mutex::new(settings.clone())),
        key_watch: Arc::new(AtomicBool::new(false)),
        rt: runtime.handle().clone(),
        window: window.as_weak(),
    };

    senders.local(PlayerCommand::Configure(audio_options(&settings)));
    senders.local(PlayerCommand::Effects(effects_options(&settings)));
    window.set_spectrum(slint::ModelRc::new(slint::VecModel::from(vec![
        0.0f32;
        effects::spectrum::BARS
    ])));
    let updater = updater::Updater::new(
        runtime.handle().clone(),
        window.as_weak(),
        Arc::clone(&senders.settings),
    );
    if std::env::var_os("TONEARM_UI_PREVIEW").is_none() {
        updater.start_schedule();
    }
    let u = updater.clone();
    window.on_update_check(move || u.check());
    let u = updater.clone();
    window.on_update_install(move || u.install());
    let u = updater.clone();
    window.on_update_open_page(move || u.open_page());
    if settings.player.soloist && std::env::var_os("TONEARM_UI_PREVIEW").is_none() {
        senders.start_soloist();
    }
    let s = senders.clone();
    window.on_soloist_get_key(move || {
        if let Err(e) = open::that_detached(tonearm_player::soloist::KEY_PAGE) {
            tracing::warn!(error = %e, "cannot open the browser");
        }
        if let Some(w) = s.window.upgrade() {
            w.set_soloist_detail(
                "Страница открыта в браузере. Нажмите там «Copy» у ключа: Tonearm заберёт его сам. \
                 Можно и вставить ключ в поле."
                    .into(),
            );
        }
        s.watch_clipboard_for_key();
    });
    let s = senders.clone();
    window.on_soloist_key_entered(move |text| {
        let key = text.trim().to_owned();
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_graphic()) {
            return;
        }
        s.use_soloist_key(key);
    });
    let s = senders.clone();
    window.on_soloist_retry(move || s.local(PlayerCommand::SoloistRetry));
    let s = senders.clone();
    window.on_settings_changed(move || s.settings_changed());
    let s = senders.clone();
    window.on_eq_preset_picked(move |i| {
        let Some(w) = s.window.upgrade() else { return };
        let i = usize::try_from(i).unwrap_or(0);
        w.set_set_eq_preset(i32::try_from(i).unwrap_or(0));
        if let Some(p) = effects::PRESETS.get(i) {
            set_eq_gains(&w, &p.gains);
        } else if let Ok(saved) = s.settings.lock() {
            // «Свой»: the bands as they were last set by hand.
            if saved.effects.eq_preset == "custom" {
                set_eq_gains(&w, &saved.effects.eq_gains);
            }
        }
        s.settings_changed();
    });
    let s = senders.clone();
    window.on_eq_band_committed(move |band, db| {
        let Some(w) = s.window.upgrade() else { return };
        let gains = w.get_eq_gains();
        if let Ok(band) = usize::try_from(band) {
            gains.set_row_data(band, db.clamp(-12, 12));
        }
        w.set_set_eq_preset(i32::try_from(effects::PRESETS.len()).unwrap_or(0));
        s.settings_changed();
    });
    let s = senders.clone();
    window.on_theme_picked(move |id| {
        if let Some(w) = s.window.upgrade() {
            ui_state::with(|st| st.pick_theme(&w, &id));
            s.settings_changed();
        }
    });
    let s = senders.clone();
    window.on_theme_new(move || {
        if let Some(w) = s.window.upgrade() {
            ui_state::with(|st| st.new_theme(&w));
            s.settings_changed();
            open_themes_folder();
        }
    });
    let s = senders.clone();
    window.on_theme_reload(move || {
        if let Some(w) = s.window.upgrade() {
            let id = w.get_theme_id().to_string();
            ui_state::with(|st| st.load_themes(&w, &id));
        }
    });
    window.on_theme_open_folder(open_themes_folder);
    let s = senders.clone();
    window.on_open_playing(move |artist| {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.open_playing(&w, artist));
            s.run(fx);
        }
    });
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
    window.on_logout(move || {
        // Soloist's sign-in and key belong to this account: forget them too.
        s.local(PlayerCommand::Soloist(None));
        s.rt.spawn_blocking(|| {
            let _ = tonearm_core::system_store().delete(SOLOIST_KEY);
            if let Some((root, _)) = soloist_dirs() {
                std::thread::sleep(Duration::from_secs(2)); // let the child exit first
                let _ = std::fs::remove_dir_all(root.join("data"));
            }
        });
        s.auth(Command::Logout);
    });
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
        if let Some(w) = s.window.upgrade() {
            let i = usize::try_from(i).unwrap_or(usize::MAX);
            let fx = ui_state::with(|st| st.play_row(&w, i));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_play_collection(move |kind, uri| {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.play_collection(&w, kind, uri.to_string()));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_radio_current(move || {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.radio_current(&w));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_lyrics_mode_changed(move || {
        s.settings_changed();
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.lyrics_mode_changed(&w));
            s.run(fx);
            lyrics_kick(&w);
        }
    });
    let s = senders.clone();
    window.on_lyrics_opened(move || {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.lyrics_opened(&w));
            s.run(fx);
            lyrics_kick(&w);
        }
    });
    let s = senders.clone();
    window.on_palette_opened(move || {
        if let Some(w) = s.window.upgrade() {
            ui_state::with(|st| st.palette_edited(&w, ""));
        }
    });
    let s = senders.clone();
    window.on_palette_edited(move |text| {
        if let Some(w) = s.window.upgrade() {
            ui_state::with(|st| st.palette_edited(&w, &text));
        }
    });
    let s = senders.clone();
    window.on_palette_activate(move |i| {
        if let Some(w) = s.window.upgrade() {
            let i = usize::try_from(i).unwrap_or(usize::MAX);
            let fx = ui_state::with(|st| st.palette_activate(&w, i));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_page_filter_edited(move |text| {
        if let Some(w) = s.window.upgrade() {
            ui_state::with(|st| st.page_filter(&w, &text));
        }
    });
    let s = senders.clone();
    window.on_page_sort_changed(move |sort| {
        if let Some(w) = s.window.upgrade() {
            ui_state::with(|st| st.page_sort(&w, sort));
        }
    });
    let s = senders.clone();
    window.on_stats_opened(move || {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.stats_opened(&w));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_smart_new(move || {
        if let Some(w) = s.window.upgrade() {
            ui_state::with(|st| st.smart_new(&w));
        }
    });
    let s = senders.clone();
    window.on_smart_edit(move || {
        if let Some(w) = s.window.upgrade() {
            ui_state::with(|st| st.smart_edit(&w));
        }
    });
    let s = senders.clone();
    window.on_smart_save(move || {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.smart_save(&w));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_smart_delete(move || {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.smart_delete(&w));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_wrapped_opened(move |i| {
        if let Some(w) = s.window.upgrade() {
            let fx = ui_state::with(|st| st.wrapped_opened(&w, i));
            s.run(fx);
        }
    });
    let s = senders.clone();
    window.on_wrapped_play(move |i| {
        let fx = ui_state::with(|st| st.wrapped_play(i));
        s.run(fx);
    });
    let s = senders.clone();
    window.on_lastfm_login(move || s.lastfm_cmd(LastFmCommand::Login));
    let s = senders.clone();
    window.on_lastfm_cancel(move || s.lastfm_cmd(LastFmCommand::CancelLogin));
    let s = senders.clone();
    window.on_lastfm_logout(move || s.lastfm_cmd(LastFmCommand::Logout));
    let s = senders.clone();
    window.on_history_play(move |i| {
        let fx = ui_state::with(|st| st.history_play(i));
        s.run(fx);
    });
    let s = senders.clone();
    window.on_queue_move(move |from, to| s.run(ui_state::UiState::queue_move(from, to)));
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
        w.set_remote_name(name.into());
    });
    let s = senders.clone();
    window.on_queue_jump(move |i| {
        s.player(PlayerCommand::JumpTo(
            usize::try_from(i).unwrap_or(usize::MAX),
        ));
    });
    let s = senders.clone();
    window.on_queue_remove(move |i| {
        if let Ok(i) = usize::try_from(i) {
            s.player(PlayerCommand::RemoveFromQueue(i));
        }
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
    window.on_toggle_play(move || match ui_state::with(UiState::take_resume) {
        Some(fx) => s.run(fx),
        None => s.player(PlayerCommand::TogglePlay),
    });
    let weak = window.as_weak();
    let s = senders.clone();
    window.on_seek(move |ratio| {
        if let Some(w) = weak.upgrade() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let ms = (f64::from(ratio.clamp(0.0, 1.0)) * f64::from(w.get_duration_ms())) as u32;
            s.player(PlayerCommand::Seek { position_ms: ms });
            w.set_position_ms(i32::try_from(ms).unwrap_or(0));
            ui_state::with(|st| st.position_changed(&w));
            lyrics_kick(&w);
        }
    });
    let s = senders.clone();
    window.on_volume_changed(move |level| {
        let percent = u8::try_from(level.clamp(0, 100)).unwrap_or(100);
        s.player(PlayerCommand::SetVolume(percent));
    });

    // Once-a-second UI clock: login wait time, offline countdown, history and lyrics.
    let tick_senders = senders.clone();
    let tick = slint::Timer::default();
    let weak = window.as_weak();
    // Informational notices ("Добавлено в «Любимые»") go away by themselves; errors stay.
    let mut notice_seen = (slint::SharedString::new(), 0u32);
    // Back in the window after a while: playlists made or followed elsewhere show up.
    let mut was_focused = true;
    let mut library_at = std::time::Instant::now();
    tick.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        move || {
            if let Some(w) = weak.upgrade() {
                let notice = w.get_notice();
                if notice.is_empty() || w.get_notice_is_error() || notice != notice_seen.0 {
                    notice_seen = (notice, 0);
                } else {
                    notice_seen.1 += 1;
                    if notice_seen.1 >= NOTICE_SECS {
                        w.set_notice("".into());
                    }
                }
                if w.get_auth_view() == AuthView::Waiting {
                    w.set_waited_secs(w.get_waited_secs() + 1);
                }
                let focused = window_focused(&w);
                if focused
                    && !was_focused
                    && w.get_auth_view() == AuthView::LoggedIn
                    && library_at.elapsed() >= LIBRARY_REFRESH
                {
                    library_at = std::time::Instant::now();
                    tick_senders.run(vec![Effect::Library(LibraryCommand::Refresh)]);
                }
                was_focused = focused;
                if w.get_offline() && w.get_retry_in_secs() > 0 {
                    w.set_retry_in_secs(w.get_retry_in_secs() - 1);
                }
                // A remote device is polled every few seconds; move the dial in between.
                let playing = w.get_play_view() == PlayView::Playing;
                if !w.get_remote_name().is_empty() && playing {
                    let next = (w.get_position_ms() + 1000).min(w.get_duration_ms());
                    w.set_position_ms(next);
                    ui_state::with(|st| st.position_changed(&w));
                }
                ui_state::with(|st| st.watch_theme(&w));
                let fx = ui_state::with(|st| st.tick(&w, playing));
                tick_senders.run(fx);
                // Catches play/pause, opening and closing the lyrics.
                lyrics_kick(&w);
                mini::sync(&w);
            }
        },
    );

    let ui = window.as_weak();
    let backend = watched(
        &runtime,
        "вход",
        &ui,
        tonearm_core::run_backend(cmd_rx, ev_tx, session_tx),
    );
    let player = watched(
        &runtime,
        "плеер",
        &ui,
        tonearm_player::run(pcmd_rx, pev_tx, session_rx.clone()),
    );
    let library = watched(
        &runtime,
        "медиатека",
        &ui,
        tonearm_core::library::run(lcmd_rx, lev_tx, session_rx.clone()),
    );
    let connect = watched(
        &runtime,
        "Spotify Connect",
        &ui,
        tonearm_core::connect::run(ccmd_rx, cev_tx, session_rx),
    );
    let lastfm = watched(
        &runtime,
        "Last.fm",
        &ui,
        tonearm_core::lastfm::run(
            settings.lastfm.clone(),
            tonearm_core::system_store(),
            fcmd_rx,
            fev_tx,
        ),
    );
    forward(
        &runtime,
        fev_rx,
        window.as_weak(),
        senders.clone(),
        apply_lastfm,
    );
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
        preview(&window, &senders);
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

    window.on_toggle_mini({
        let weak = window.as_weak();
        move || {
            if let Some(w) = weak.upgrade() {
                mini::toggle(&w);
            }
        }
    });
    window.on_mini_sync({
        let weak = window.as_weak();
        move || {
            if let Some(w) = weak.upgrade() {
                mini::sync(&w);
            }
        }
    });
    // Closing the main window quits, even with the mini player still open.
    window.window().on_close_requested({
        let weak = window.as_weak();
        move || {
            if let Some(w) = weak.upgrade() {
                mini::close(&w);
            }
            slint::CloseRequestResponse::HideWindow
        }
    });

    window.run()?;

    // Queued saves first, so none of them can land after (and over) the final one below.
    tonearm_core::fsutil::flush();
    // Continue where we left off next time: write the history and position now.
    if std::env::var_os("TONEARM_UI_PREVIEW").is_none()
        && let Some(store) = ui_state::with(|st| st.save_now(&window))
    {
        store.save();
    }

    let _ = cmd_tx.blocking_send(Command::Shutdown);
    // Dropping the window and the senders closes the player and library channels.
    drop(window);
    drop(senders);
    drop(tick);
    // All services wind down at the same time; together they get two seconds, not two each.
    runtime.block_on(async {
        let all = async {
            let _ = tokio::join!(backend, player, library, connect, lastfm);
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), all).await;
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
    if updater.restart_requested() {
        tonearm_core::update::restart();
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn preview(w: &AppWindow, senders: &Senders) {
    use tonearm_core::library::{CollectionKind, PageHeader, PlaylistSummary, Tile, TrackItem};
    let screen = std::env::var("TONEARM_UI_PREVIEW").unwrap_or_default();
    w.set_auth_view(match screen.as_str() {
        "login" => AuthView::LoggedOut,
        "waiting" => AuthView::Waiting,
        _ => AuthView::LoggedIn,
    });
    w.set_waited_secs(42);
    w.set_username("LISTENER".into());
    if screen.starts_with("update") {
        w.set_update_known(true);
        w.set_update_version("0.2.0".into());
        w.set_update_progress(0.42);
        w.set_update_message("Подпись файла не совпала, обновление не установлено".into());
        w.set_update_state(match screen.as_str() {
            "update-downloading" => UpdateState::Downloading,
            "update-failed" => UpdateState::Failed,
            _ => UpdateState::Available,
        });
    }
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
        owner: "Аня".into(),
        track_count: 40 + u32::try_from(i).unwrap_or(0) * 17,
        image_url: Some(format!("preview:p{i}")),
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
            image_url: Some(format!("preview:{t}")),
        })
        .collect();
        fx.extend(st.on_library(w, LibraryEvent::Home(tiles)));
        if matches!(screen.as_str(), "playlist" | "queue" | "menu") {
            fx.extend(st.open(w, PageRequest::Playlist("spotify:playlist:0".into()), true));
            fx.extend(st.on_library(
                w,
                LibraryEvent::PageHeader {
                    page_id: 1,
                    header: PageHeader {
                        kind: CollectionKind::Playlist,
                        uri: "spotify:playlist:0".into(),
                        title: "Ночные маршруты".into(),
                        byline: "Аня".into(),
                        byline_uri: None,
                        year: None,
                        total_tracks: Some(7),
                        image_url: Some("preview:p0".into()),
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
                    page_id: 1,
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
            cover_url: Some("preview:deck".into()),
        };
        fx.extend(st.on_track(w, &track));
        if screen == "stats" || screen == "home" || screen.starts_with("smart") {
            fx.extend(st.preview_history(w));
        }
        if screen == "wrapped" {
            st.preview_year();
        }
        if screen == "smart" {
            let req = PageRequest::Playlist(smart::uri("on-repeat"));
            fx.extend(st.open(w, req, true));
        }
        // Feed the OS media controls too, once they are registered (after the event loop starts).
        slint::Timer::single_shot(Duration::from_millis(300), move || {
            os_media::on_track(&track, true, false);
            os_media::on_state(tonearm_player::PlaybackState::Playing, 84_000);
        });
        fx
    });
    senders.run(fx);
    if screen == "library" {
        w.set_page(Page::Library);
    }
    if screen == "effects" {
        w.set_set_engine(1);
        w.set_set_eq(1);
        w.set_set_eq_preset(1);
        set_eq_gains(w, &effects::PRESETS[1].gains);
        w.set_set_fade(2);
        w.set_set_visualizer(1);
        let bars = [
            0.9, 0.8, 0.95, 0.7, 0.55, 0.6, 0.45, 0.5, 0.4, 0.35, 0.3, 0.25, 0.2, 0.12,
        ];
        w.set_spectrum(slint::ModelRc::new(slint::VecModel::from(bars.to_vec())));
    }
    if screen == "settings" || screen == "effects" {
        w.set_page(Page::Settings);
        w.set_username("nightowl".into());
        w.set_cache_used_mb(214);
        w.set_lastfm_state(3);
        w.set_lastfm_user("nightowl".into());
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
            cover_url: None,
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
        preview_search(w, senders, &screen);
    }
    if screen == "stats" {
        w.set_page(Page::Stats);
    }
    if screen == "mini" {
        // After the sample cover has arrived.
        let weak = w.as_weak();
        slint::Timer::single_shot(Duration::from_millis(800), move || {
            if let Some(w) = weak.upgrade() {
                mini::toggle(&w);
            }
        });
    }
    if screen == "wrapped" {
        w.set_page(Page::Wrapped);
    }
    if screen == "smart-editor" {
        ui_state::with(|st| st.smart_new(w));
    }
    if screen.starts_with("lyrics") {
        use tonearm_core::library::lyrics::{LyricLine, Lyrics, fill_timings};
        let lines = [
            "Город спит, и только мы",
            "Едем вдоль реки",
            "Огни над рекой горят для нас одних, пока над городом встаёт рассвет",
            "Горят для нас одних",
            "",
            "Тихо шепчет радио",
            "Старые слова",
            "Огни над рекой",
            "Нас ведут до утра",
        ];
        let mut lines: Vec<LyricLine> = lines
            .iter()
            .enumerate()
            .map(|(i, t)| LyricLine {
                start_ms: Some(u32::try_from(i).unwrap_or(0) * 4_000 + 76_000),
                text: (*t).into(),
                words: Vec::new(),
            })
            .collect();
        // `lyrics-plain`: the same text without times.
        let synced = screen == "lyrics";
        if synced {
            w.set_lyrics_mode(1);
            fill_timings(&mut lines, 240_000, true);
        } else {
            for l in &mut lines {
                l.start_ms = None;
            }
        }
        let lyrics = Lyrics {
            lines,
            synced,
            provider: "LRCLIB".into(),
            ..Lyrics::default()
        };
        w.set_lyrics_open(true);
        let fx = ui_state::with(|st| {
            st.on_library(
                w,
                LibraryEvent::Lyrics {
                    uri: "spotify:track:0".into(),
                    lyrics: Some(lyrics),
                },
            )
        });
        senders.run(fx);
    }
    if screen == "palette" {
        let weak = w.as_weak();
        slint::Timer::single_shot(Duration::from_millis(400), move || {
            if let Some(w) = weak.upgrade() {
                w.invoke_open_palette();
                w.set_palette_query("но".into());
                ui_state::with(|st| st.palette_edited(&w, "но"));
            }
        });
    }
    w.set_position_ms(84_000);
    ui_state::with(|st| st.position_changed(w));
    w.set_play_view(PlayView::Playing);
}

/// Sample cover for the preview: a two-colour gradient with a disc, coloured by the seed.
#[cfg(debug_assertions)]
fn preview_cover(seed: &str, size: u32) -> tonearm_core::covers::Cover {
    let h = seed.bytes().fold(2_166_136_261_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    });
    let pal: [[u8; 3]; 8] = [
        [0xF2, 0x6B, 0x3A],
        [0x3A, 0x86, 0xF2],
        [0x8E, 0x4D, 0xE8],
        [0x2B, 0xB6, 0x8F],
        [0xE8, 0x3F, 0x6F],
        [0xF2, 0xC1, 0x3A],
        [0x1F, 0x4E, 0x79],
        [0xC9, 0x5A, 0xD6],
    ];
    let a = pal[(h % 8) as usize];
    let b = pal[((h >> 8) % 8) as usize];
    let n = size.max(8);
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    for y in 0..n {
        for x in 0..n {
            let t = (x + y) as f32 / (2 * n) as f32;
            let (cx, cy) = (x as f32 - n as f32 * 0.62, y as f32 - n as f32 * 0.42);
            let r = (cx * cx + cy * cy).sqrt() / n as f32;
            let ring = if r < 0.28 { 0.75 } else { 1.0 };
            for c in 0..3 {
                let v = f32::from(a[c]) * (1.0 - t) + f32::from(b[c]) * t;
                px.push((v * ring) as u8);
            }
            px.push(255);
        }
    }
    let accent = tonearm_core::covers::accent(&px);
    tonearm_core::covers::Cover {
        url: seed.into(),
        width: n,
        height: n,
        color: px,
        accent,
    }
}

#[cfg(debug_assertions)]
fn preview_search(w: &AppWindow, senders: &Senders, screen: &str) {
    use tonearm_core::library::{CollectionKind, SearchResults, Tile, TrackItem};
    let tile = |kind, title: &str, sub: &str| Tile {
        uri: format!("spotify:x:{title}"),
        kind,
        title: title.into(),
        subtitle: sub.into(),
        image_url: Some(format!("preview:{title}")),
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
        image_url: Some(format!("preview:{a}")),
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
            .map(|t| tile(CollectionKind::Playlist, t, "Аня"))
            .collect(),
        more: [true, true, false, false],
    };
    w.set_page(Page::Search);
    w.set_search_query("Ночн".into());
    if screen == "search-tracks" {
        w.set_search_tab(1);
    }
    let fx = ui_state::with(|st| {
        drop(st.search_edited(w, "Ночн"));
        st.on_library(
            w,
            LibraryEvent::Search {
                query: "Ночн".into(),
                kind: None,
                offset: 0,
                results,
            },
        )
    });
    senders.run(fx);
}
