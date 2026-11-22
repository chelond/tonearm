//! Tonearm's PipeWire sink: a `pipewire -c` child running a filter-chain (ten peaking filters
//! and an output gain), changed on the fly with `pw-cli set-param`. Both tools come with
//! PipeWire itself, so nothing extra is needed.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use super::{Gains, props, sink_config};

/// The sink's node name; Soloist gets it as `--pipewire-device`.
pub const NODE: &str = "tonearm_eq";
const CONFIG: &str = "tonearm-sink.conf";
/// Fade steps; small enough that the gain changes sound smooth.
const STEP: Duration = Duration::from_millis(40);

enum Ctl {
    Eq(Option<Gains>),
    /// Move the output gain to `to` (0..=1) over `ms`.
    Fade {
        to: f32,
        ms: u32,
    },
}

pub struct Sink {
    child: Child,
    ctl: mpsc::UnboundedSender<Ctl>,
    config: PathBuf,
}

impl Sink {
    /// Starts the sink and waits until PipeWire lists it. Fails without PipeWire.
    pub async fn start(dir: &Path, eq: Option<Gains>) -> std::io::Result<Self> {
        tokio::fs::create_dir_all(dir).await?;
        let config = dir.join(CONFIG);
        stop_leftover(&config).await;
        tokio::fs::write(&config, sink_config(NODE, eq.as_ref())).await?;
        let mut child = Command::new("pipewire")
            .arg("-c")
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        for _ in 0..40 {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(std::io::Error::other(format!(
                    "pipewire -c stopped: {status}"
                )));
            }
            if node_listed().await {
                let (tx, rx) = mpsc::unbounded_channel();
                tokio::spawn(control(rx, eq));
                tracing::info!("pipewire sink {NODE} is up");
                return Ok(Self {
                    child,
                    ctl: tx,
                    config,
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = child.kill().await;
        Err(std::io::Error::other(
            "PipeWire не создал устройство Tonearm",
        ))
    }

    pub fn set_eq(&self, eq: Option<Gains>) {
        let _ = self.ctl.send(Ctl::Eq(eq));
    }

    pub fn fade(&self, to: f32, ms: u32) {
        let _ = self.ctl.send(Ctl::Fade { to, ms });
    }

    /// `false` once the `pipewire -c` child has stopped (PipeWire restarted, killed…).
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.config);
    }
}

/// Applies changes in order; a newer fade replaces the one in progress.
async fn control(mut rx: mpsc::UnboundedReceiver<Ctl>, mut eq: Option<Gains>) {
    let mut gain = 1.0_f32;
    // (target, change per step)
    let mut fading: Option<(f32, f32)> = None;
    let mut tick = tokio::time::interval(STEP);
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    None => return,
                    Some(Ctl::Eq(new)) => eq = new,
                    Some(Ctl::Fade { to, ms }) => {
                        let to = to.clamp(0.0, 1.0);
                        let steps = (ms as f32 / STEP.as_millis() as f32).max(1.0);
                        fading = Some((to, (to - gain) / steps));
                        continue;
                    }
                }
                set(&props(eq.as_ref(), gain * gain)).await;
            }
            _ = tick.tick(), if fading.is_some() => {
                let Some((to, step)) = fading else { continue };
                gain += step;
                if (step >= 0.0 && gain >= to) || (step < 0.0 && gain <= to) {
                    gain = to;
                    fading = None;
                }
                // Perceived loudness follows the square better than the linear gain.
                set(&props(eq.as_ref(), gain * gain)).await;
            }
        }
    }
}

async fn set(props: &str) {
    // A hung pw-cli must not stall the fade/equalizer loop; it is killed when dropped.
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        Command::new("pw-cli")
            .args(["set-param", NODE, "Props", props])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await;
    match out {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "pw-cli set-param failed"),
        Err(_) => tracing::warn!("pw-cli set-param timed out"),
    }
}

async fn node_listed() -> bool {
    let Ok(out) = Command::new("pw-cli")
        .args(["ls", "Node"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout).contains(&format!("node.name = \"{NODE}\""))
}

/// A sink left by a Tonearm that was killed would make a second `tonearm_eq`: stop it. It is
/// the `pipewire` process whose command line names our config file.
async fn stop_leftover(config: &Path) {
    let Ok(mut dir) = tokio::fs::read_dir("/proc").await else {
        return;
    };
    let want = config.as_os_str().as_encoded_bytes();
    while let Ok(Some(entry)) = dir.next_entry().await {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(cmdline) = tokio::fs::read(entry.path().join("cmdline")).await else {
            continue;
        };
        let mut args = cmdline.split(|&b| b == 0);
        let is_pipewire = args.next().is_some_and(|a| a.ends_with(b"pipewire"));
        if is_pipewire && args.any(|a| a == want) {
            tracing::info!(pid, "stopping a leftover tonearm sink");
            let _ = Command::new("kill").arg(pid).status().await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a running PipeWire: `cargo test -p tonearm-player -- --ignored live_ --test-threads=1`.
    #[tokio::test]
    #[ignore = "needs a running PipeWire"]
    async fn live_sink() {
        let dir = tempfile_dir();
        let sink = Sink::start(&dir, Some([6, 0, 0, 0, 0, 0, 0, 0, 0, -3]))
            .await
            .expect("sink starts");
        sink.set_eq(Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]));
        sink.fade(0.0, 200);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let out = Command::new("pw-cli")
            .args(["enum-params", NODE, "Props"])
            .output()
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let after = |key: &str| {
            let i = text.find(key).unwrap_or_else(|| panic!("{key} in {text}"));
            text[i..]
                .lines()
                .nth(1)
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        assert_eq!(after("\"b9:Gain\""), "Float 10.000000");
        assert_eq!(after("\"out:Gain 1\""), "Float 0.000000");
        drop(sink);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!node_listed().await, "sink gone after drop");
    }

    /// A tone played into the sink shows up in the visualizer's frames.
    #[tokio::test]
    #[ignore = "needs a running PipeWire and a sine.wav (TONEARM_TEST_WAV)"]
    async fn live_visualizer() {
        let wav = std::env::var("TONEARM_TEST_WAV").expect("TONEARM_TEST_WAV");
        let sink = Sink::start(&tempfile_dir(), None)
            .await
            .expect("sink starts");
        let mut play = Command::new("pw-play")
            .args(["--target", NODE, &wav])
            .spawn()
            .unwrap();
        let frames = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let f = std::sync::Arc::clone(&frames);
        let cap =
            crate::effects::spectrum::Capture::start(NODE, move |fr| f.lock().unwrap().push(fr))
                .expect("pw-record starts");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        drop(cap);
        let _ = play.kill().await;
        drop(sink);
        let frames = frames.lock().unwrap();
        assert!(frames.len() > 20, "{} frames", frames.len());
        let loudest = frames.iter().flat_map(|f| f.iter().copied()).max().unwrap();
        assert!(loudest > 150, "{:?}", frames.last());
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tonearm-sink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
