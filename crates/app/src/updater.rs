//! Glue between the update module of the core and the window: background checks, the
//! "Обновить" button, progress, and the restart once the new version is in place.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;
use tonearm_core::config::Settings;
use tonearm_core::update::{self, Release};

use crate::{AppWindow, UpdateState};

const FIRST_CHECK: Duration = Duration::from_secs(10);
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone)]
pub struct Updater {
    rt: Handle,
    window: slint::Weak<AppWindow>,
    settings: Arc<Mutex<Settings>>,
    release: Arc<Mutex<Option<Release>>>,
    busy: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
}

impl Updater {
    pub fn new(rt: Handle, window: slint::Weak<AppWindow>, settings: Arc<Mutex<Settings>>) -> Self {
        Self {
            rt,
            window,
            settings,
            release: Arc::new(Mutex::new(None)),
            busy: Arc::new(AtomicBool::new(false)),
            restart: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Checks shortly after startup and then once a day, while the setting is on.
    pub fn start_schedule(&self) {
        let me = self.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(FIRST_CHECK).await;
            loop {
                if me.auto_enabled() {
                    me.check_now(false).await;
                }
                tokio::time::sleep(CHECK_EVERY).await;
            }
        });
    }

    fn auto_enabled(&self) -> bool {
        self.settings.lock().is_ok_and(|s| s.update.check)
    }

    fn feed(&self) -> String {
        self.settings
            .lock()
            .map(|s| s.update.feed_url.clone())
            .unwrap_or_default()
    }

    /// "Проверить" on the settings page.
    pub fn check(&self) {
        let me = self.clone();
        self.rt.spawn(async move { me.check_now(true).await });
    }

    async fn check_now(&self, manual: bool) {
        if self.busy.load(Ordering::SeqCst) {
            return;
        }
        if manual {
            self.ui(|w| w.set_update_state(UpdateState::Checking));
        }
        match update::check(&self.feed(), env!("CARGO_PKG_VERSION")).await {
            Ok(Some(release)) => {
                tracing::info!(version = %release.version, file = %release.asset_name, "update available");
                let version = release.version.clone();
                let was = self
                    .release
                    .lock()
                    .ok()
                    .and_then(|mut r| r.replace(release))
                    .map(|r| r.version);
                let fresh = was.as_deref() != Some(version.as_str());
                self.ui(move |w| {
                    w.set_update_version(version.into());
                    w.set_update_known(true);
                    w.set_update_state(UpdateState::Available);
                    // A version the user already closed the card for stays closed.
                    if fresh || manual {
                        w.set_update_hidden(false);
                    }
                });
            }
            Ok(None) => {
                tracing::debug!("no newer version");
                if manual {
                    self.ui(|w| w.set_update_state(UpdateState::Latest));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "update check failed");
                if manual {
                    let message = e.to_string();
                    self.ui(move |w| {
                        w.set_update_message(capitalize(&message).into());
                        w.set_update_state(UpdateState::Failed);
                    });
                }
            }
        }
    }

    /// "Обновить": download, verify, install, restart.
    pub fn install(&self) {
        let Some(release) = self.release.lock().ok().and_then(|r| r.clone()) else {
            return self.check();
        };
        if self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        self.rt.spawn(async move {
            me.ui(|w| {
                w.set_update_progress(0.0);
                w.set_update_state(UpdateState::Downloading);
            });
            let progress_ui = me.window.clone();
            let mut last_step = -1i64;
            let result = update::download(&release, move |done, total| {
                // Repaint at most every 2 %.
                #[allow(clippy::cast_precision_loss)]
                let ratio = if total > 0 {
                    done as f32 / total as f32
                } else {
                    -1.0
                };
                #[allow(clippy::cast_possible_truncation)]
                let step = (ratio * 50.0) as i64;
                if step != last_step {
                    last_step = step;
                    let _ =
                        progress_ui.upgrade_in_event_loop(move |w| w.set_update_progress(ratio));
                }
            })
            .await;
            let result = match result {
                Ok(file) => {
                    me.ui(|w| w.set_update_state(UpdateState::Installing));
                    tokio::task::spawn_blocking(move || update::install(file))
                        .await
                        .unwrap_or_else(|e| Err(update::UpdateError::Install(e.to_string())))
                }
                Err(e) => Err(e),
            };
            me.busy.store(false, Ordering::SeqCst);
            match result {
                Ok(()) => {
                    tracing::info!(version = %release.version, "update installed, restarting");
                    me.restart.store(true, Ordering::SeqCst);
                    let _ = slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "update failed");
                    let message = capitalize(&e.to_string());
                    me.ui(move |w| {
                        w.set_update_message(message.into());
                        w.set_update_state(UpdateState::Failed);
                        w.set_update_hidden(false);
                    });
                }
            }
        });
    }

    /// "Что нового": the release page in the browser.
    pub fn open_page(&self) {
        let page = self
            .release
            .lock()
            .ok()
            .and_then(|r| r.as_ref().map(|r| r.page.clone()))
            .unwrap_or_else(|| update::RELEASES_PAGE.to_owned());
        if let Err(e) = open::that_detached(&page) {
            tracing::warn!(error = %e, "cannot open the release page");
        }
    }

    /// The new version is installed: start it after this process has shut down.
    pub fn restart_requested(&self) -> bool {
        self.restart.load(Ordering::SeqCst)
    }

    fn ui(&self, f: impl FnOnce(&AppWindow) + Send + 'static) {
        let _ = self.window.upgrade_in_event_loop(move |w| f(&w));
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}
