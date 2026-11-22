//! The visualizer's bars: a 512-point FFT of mono audio at 16 kHz, folded into log-spaced
//! bands, ~31 frames a second. On Linux the audio comes from `pw-record` on the monitor of
//! Tonearm's sink; it runs only while the visualizer is on and music plays.

/// Bars under the cover.
pub const BARS: usize = 14;
pub(crate) const RATE: u32 = 16_000;
const N: usize = 512;
const LOW_HZ: f32 = 50.0;
const HIGH_HZ: f32 = 7_000.0;
/// Level range shown, dB below full scale.
const FLOOR_DB: f32 = 60.0;
/// Bars fall at most this much per frame (rise is instant).
const FALL: f32 = 0.06;

pub type Frame = [u8; BARS];

/// Turns blocks of samples into bar heights, with the falling smoothing applied.
pub struct Analyzer {
    window: Vec<f32>,
    edges: [usize; BARS + 1],
    levels: [f32; BARS],
    re: Vec<f32>,
    im: Vec<f32>,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    #[must_use]
    pub fn new() -> Self {
        let window = (0..N)
            .map(|i| {
                let x = std::f32::consts::TAU * i as f32 / (N - 1) as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        let bin_hz = RATE as f32 / N as f32;
        let mut edges = [0; BARS + 1];
        for (i, e) in edges.iter_mut().enumerate() {
            let hz = LOW_HZ * (HIGH_HZ / LOW_HZ).powf(i as f32 / BARS as f32);
            *e = ((hz / bin_hz).round() as usize).clamp(1, N / 2);
        }
        // Every band at least one bin wide.
        for i in 1..=BARS {
            if edges[i] <= edges[i - 1] {
                edges[i] = (edges[i - 1] + 1).min(N / 2);
            }
        }
        Self {
            window,
            edges,
            levels: [0.0; BARS],
            re: vec![0.0; N],
            im: vec![0.0; N],
        }
    }

    /// Samples per frame.
    #[must_use]
    pub const fn block() -> usize {
        N
    }

    /// One frame from `N` samples (fewer are padded with silence).
    pub fn frame(&mut self, samples: &[f32]) -> Frame {
        for i in 0..N {
            self.re[i] = samples.get(i).copied().unwrap_or(0.0) * self.window[i];
            self.im[i] = 0.0;
        }
        fft(&mut self.re, &mut self.im);
        let mut out = [0u8; BARS];
        for (b, slot) in out.iter_mut().enumerate() {
            let (lo, hi) = (self.edges[b], self.edges[b + 1].max(self.edges[b] + 1));
            let peak = (lo..hi.min(N / 2))
                .map(|k| self.re[k].hypot(self.im[k]))
                .fold(0.0_f32, f32::max);
            // Full-scale sine through a Hann window peaks at N/4.
            let db = 20.0 * (peak / (N as f32 / 4.0)).max(1e-9).log10();
            let target = ((db + FLOOR_DB) / FLOOR_DB).clamp(0.0, 1.0);
            let level = &mut self.levels[b];
            *level = if target > *level {
                target
            } else {
                (*level - FALL).max(target)
            };
            *slot = (*level * 255.0).round() as u8;
        }
        out
    }
}

/// In-place iterative radix-2 FFT; `re.len()` must be a power of two.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f32::consts::TAU / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0_f32, 0.0_f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let next = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = next;
            }
        }
        len <<= 1;
    }
}

#[cfg(target_os = "linux")]
pub(crate) use capture::Capture;

#[cfg(target_os = "linux")]
mod capture {
    use std::io::Read;
    use std::process::{Child, Command, Stdio};

    use super::{Analyzer, Frame, RATE};

    /// `pw-record` on the monitor of `node`, analysed on its own thread.
    pub struct Capture {
        child: Child,
    }

    impl Capture {
        pub fn start(
            node: &str,
            mut on_frame: impl FnMut(Frame) + Send + 'static,
        ) -> std::io::Result<Self> {
            let child = Command::new("pw-record")
                .args(["--target", node, "-P"])
                .arg("{ stream.capture.sink = true node.name = tonearm_visualizer media.role = Music }")
                .args(["--rate", &RATE.to_string(), "--channels", "1", "--format", "f32"])
                .args(["--latency", "32ms", "-"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?;
            // From here on `Capture` owns the child, so every early return below kills it.
            let mut capture = Self { child };
            let mut stdout = capture
                .child
                .stdout
                .take()
                .ok_or_else(|| std::io::Error::other("no pw-record output"))?;
            std::thread::Builder::new()
                .name("visualizer".into())
                .spawn(move || {
                    let mut analyzer = Analyzer::new();
                    let mut bytes = vec![0u8; Analyzer::block() * 4];
                    let mut samples = vec![0f32; Analyzer::block()];
                    // Ends when pw-record is killed (EOF).
                    while stdout.read_exact(&mut bytes).is_ok() {
                        for (s, b) in samples.iter_mut().zip(bytes.as_chunks::<4>().0) {
                            *s = f32::from_le_bytes(*b);
                        }
                        on_frame(analyzer.frame(&samples));
                    }
                })?;
            Ok(capture)
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, amp: f32) -> Vec<f32> {
        (0..N)
            .map(|i| amp * (std::f32::consts::TAU * hz * i as f32 / RATE as f32).sin())
            .collect()
    }

    #[test]
    fn a_tone_lights_its_own_band() {
        let mut a = Analyzer::new();
        let f = a.frame(&sine(1000.0, 0.5));
        let top = f.iter().enumerate().max_by_key(|(_, v)| **v).unwrap().0;
        let band_hz = |b: usize| LOW_HZ * (HIGH_HZ / LOW_HZ).powf((b as f32 + 0.5) / BARS as f32);
        assert!(
            (band_hz(top) / 1000.0).log2().abs() < 0.6,
            "band {top}: {f:?}"
        );
        assert!(f[0] < f[top] / 2, "low band quiet: {f:?}");
    }

    #[test]
    fn silence_falls_off_smoothly() {
        let mut a = Analyzer::new();
        let loud = a.frame(&sine(200.0, 1.0));
        let after = a.frame(&[]);
        let peak = loud.iter().copied().max().unwrap();
        let peak_after = after.iter().copied().max().unwrap();
        assert!(peak > 200, "{loud:?}");
        assert!(peak_after < peak && peak_after > peak - 30, "{after:?}");
    }
}
