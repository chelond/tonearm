//! Mini player: a small always-on-top window with the cover, the track and the transport.
//!
//! It is created when opened and dropped when closed, so it costs nothing while hidden. The
//! window shows a copy of the main window's player state; its buttons call the main window's
//! callbacks, so playback logic stays in one place.
//!
//! "Above the other windows" is a request the window system may ignore: Windows and X11 honour
//! it; Wayland has no such protocol, so on Hyprland the window is floated and pinned over its IPC
//! socket, and other Wayland compositors need their own "keep above" setting.

use std::cell::RefCell;
use std::time::Duration;

use slint::{ComponentHandle, Global};

use crate::{AppWindow, MiniPlayer, PlayView, Theme};

/// Window title (as in mini.slint); the Hyprland lookup finds the window by it.
#[cfg(target_os = "linux")]
const TITLE: &str = "Tonearm — мини-плеер";

thread_local! {
    static MINI: RefCell<Option<MiniPlayer>> = const { RefCell::new(None) };
}

pub fn toggle(main: &AppWindow) {
    if MINI.with(|m| m.borrow().is_some()) {
        close(main);
    } else {
        open(main);
    }
}

fn open(main: &AppWindow) {
    let mini = match MiniPlayer::new() {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "cannot create the mini player");
            return;
        }
    };
    let weak = main.as_weak();
    let on_main = move |f: fn(&AppWindow)| {
        let weak = weak.clone();
        move || {
            if let Some(w) = weak.upgrade() {
                f(&w);
                sync(&w);
            }
        }
    };
    mini.on_toggle_play(on_main(|w| w.invoke_toggle_play()));
    mini.on_next(on_main(|w| w.invoke_next()));
    mini.on_prev(on_main(|w| w.invoke_prev()));
    mini.on_close_mini(on_main(close));
    mini.on_expand(on_main(|w| {
        close(w);
        crate::raise(w);
    }));
    let weak = main.as_weak();
    mini.on_seek(move |ratio| {
        if let Some(w) = weak.upgrade() {
            w.invoke_seek(ratio);
            sync(&w);
        }
    });
    let me = mini.as_weak();
    mini.on_drag(move || {
        use slint::winit_030::WinitWindowAccessor;
        if let Some(m) = me.upgrade() {
            m.window().with_winit_window(|ww| {
                if let Err(e) = ww.drag_window() {
                    tracing::debug!(error = %e, "window move not supported");
                }
            });
        }
    });
    // The window manager's close (Alt+F4) closes it like the button does.
    let weak = main.as_weak();
    mini.window().on_close_requested(move || {
        if let Some(w) = weak.upgrade() {
            close(&w);
        }
        slint::CloseRequestResponse::HideWindow
    });
    copy_state(main, &mini);
    if let Err(e) = mini.show() {
        tracing::warn!(error = %e, "cannot show the mini player");
        return;
    }
    MINI.with(|m| *m.borrow_mut() = Some(mini));
    main.set_mini_open(true);
    #[cfg(target_os = "linux")]
    hyprland::float_and_pin(TITLE, 380, 104);
}

/// Hides and frees the mini player (also when the main window closes).
pub fn close(main: &AppWindow) {
    main.set_mini_open(false);
    let Some(mini) = MINI.with(|m| m.borrow_mut().take()) else {
        return;
    };
    if let Err(e) = mini.hide() {
        tracing::debug!(error = %e, "cannot hide the mini player");
    }
    // Often called from the mini player's own callback: drop it once that has returned.
    slint::Timer::single_shot(Duration::ZERO, move || drop(mini));
}

/// Copies the main window's player state into the open mini player (cheap: unchanged values
/// are not redrawn).
pub fn sync(main: &AppWindow) {
    MINI.with(|m| {
        if let Some(mini) = m.borrow().as_ref() {
            copy_state(main, mini);
        }
    });
}

fn copy_state(main: &AppWindow, mini: &MiniPlayer) {
    mini.set_track_title(main.get_track_title());
    mini.set_track_artists(main.get_track_artists());
    mini.set_track_image(main.get_track_image());
    mini.set_track_has_image(main.get_track_has_image());
    mini.set_track_backdrop(main.get_track_backdrop());
    mini.set_playing(matches!(
        main.get_play_view(),
        PlayView::Playing | PlayView::Loading
    ));
    mini.set_position_ms(main.get_position_ms());
    mini.set_duration_ms(main.get_duration_ms());
    // Globals belong to each window: carry the theme over.
    let (from, to) = (Theme::get(main), Theme::get(mini));
    to.set_bg(from.get_bg());
    to.set_sidebar(from.get_sidebar());
    to.set_surface(from.get_surface());
    to.set_surface_2(from.get_surface_2());
    to.set_hover(from.get_hover());
    to.set_pressed(from.get_pressed());
    to.set_selected(from.get_selected());
    to.set_line(from.get_line());
    to.set_line_2(from.get_line_2());
    to.set_text_1(from.get_text_1());
    to.set_text_2(from.get_text_2());
    to.set_text_3(from.get_text_3());
    to.set_danger(from.get_danger());
    to.set_light(from.get_light());
    to.set_accent(from.get_accent());
    to.set_page_accent(from.get_page_accent());
    to.set_on_accent(from.get_on_accent());
}

/// Hyprland ignores "always on top" (Wayland has no protocol for it), but its IPC socket can
/// float and pin a window: pinned floating windows stay above tiled ones on every workspace.
#[cfg(target_os = "linux")]
mod hyprland {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::Duration;

    pub fn float_and_pin(title: &'static str, width: u32, height: u32) {
        let Some(socket) = socket() else { return };
        let spawned = std::thread::Builder::new()
            .name("hyprland-pin".into())
            .spawn(move || {
                // The compositor lists the window a moment after it is mapped.
                for _ in 0..20 {
                    std::thread::sleep(Duration::from_millis(100));
                    match find(&socket, title) {
                        Ok(Some(c)) => {
                            let a = c.address;
                            let mut cmds = Vec::new();
                            if !c.floating {
                                cmds.push(format!("dispatch setfloating address:{a}"));
                                cmds.push(format!(
                                    "dispatch resizewindowpixel exact {width} {height},address:{a}"
                                ));
                            }
                            if !c.pinned {
                                cmds.push(format!("dispatch pin address:{a}"));
                            }
                            if !cmds.is_empty() {
                                let batch = format!("[[BATCH]]{}", cmds.join(";"));
                                if let Err(e) = request(&socket, &batch) {
                                    tracing::debug!(error = %e, "hyprland: cannot pin");
                                }
                            }
                            return;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::debug!(error = %e, "hyprland: no answer");
                            return;
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::debug!(error = %e, "hyprland: no thread");
        }
    }

    fn socket() -> Option<PathBuf> {
        let sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
        let mut dirs = Vec::new();
        if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR") {
            dirs.push(PathBuf::from(run).join("hypr"));
        }
        // Hyprland before 0.40.
        dirs.push(PathBuf::from("/tmp/hypr"));
        dirs.into_iter()
            .map(|d| d.join(&sig).join(".socket.sock"))
            .find(|p| p.exists())
    }

    fn request(socket: &PathBuf, cmd: &str) -> std::io::Result<String> {
        let mut s = UnixStream::connect(socket)?;
        s.set_read_timeout(Some(Duration::from_secs(1)))?;
        s.write_all(cmd.as_bytes())?;
        let mut out = String::new();
        s.read_to_string(&mut out)?;
        Ok(out)
    }

    #[derive(Debug, PartialEq)]
    pub(super) struct Client {
        pub address: String,
        pub floating: bool,
        pub pinned: bool,
    }

    fn find(socket: &PathBuf, title: &str) -> std::io::Result<Option<Client>> {
        let json = request(socket, "j/clients")?;
        Ok(pick(&json, std::process::id(), title))
    }

    /// Our window in Hyprland's `j/clients` answer.
    pub(super) fn pick(json: &str, pid: u32, title: &str) -> Option<Client> {
        let clients: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
        clients.iter().find_map(|c| {
            (c["pid"].as_u64() == Some(u64::from(pid)) && c["title"].as_str() == Some(title))
                .then(|| Client {
                    address: c["address"].as_str().unwrap_or_default().to_owned(),
                    floating: c["floating"].as_bool().unwrap_or(false),
                    pinned: c["pinned"].as_bool().unwrap_or(false),
                })
                .filter(|c| c.address.starts_with("0x"))
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::hyprland::{Client, pick};

    #[test]
    fn finds_own_window_in_hyprland_clients() {
        let json = r#"[
            {"address":"0x1","pid":7,"title":"Tonearm","floating":false,"pinned":false},
            {"address":"0x2","pid":9,"title":"Tonearm — мини-плеер","floating":true,"pinned":false},
            {"address":"0x3","pid":7,"title":"Tonearm — мини-плеер","floating":true,"pinned":true}
        ]"#;
        assert_eq!(
            pick(json, 7, super::TITLE),
            Some(Client {
                address: "0x3".into(),
                floating: true,
                pinned: true
            })
        );
        assert_eq!(pick(json, 8, super::TITLE), None);
        assert_eq!(pick("not json", 7, super::TITLE), None);
    }
}
