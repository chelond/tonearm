//! Equalizer, fades between tracks and the spectrum under the cover.
//!
//! Soloist plays straight into the system mixer and has no audio settings of its own. On Linux
//! with PipeWire, Tonearm puts its own virtual output in front of the speakers (a filter-chain
//! sink, [`pipewire`]) and starts Soloist with `--pipewire-device` pointing at it: the
//! equalizer and the fade gain live there and change on the fly, and the visualizer records the
//! sink's monitor. PipeWire does the filtering, so Tonearm itself spends almost nothing on it.
//! Elsewhere (Windows, PulseAudio without PipeWire) these settings are unavailable.

// The sink's config and `Props` text is only used where the sink exists.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(target_os = "linux")]
pub(crate) mod pipewire;
pub mod spectrum;

/// Centre frequencies of the ten bands, Hz.
pub const BANDS_HZ: [u32; 10] = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
/// Band gains are whole dB in `-GAIN_LIMIT..=GAIN_LIMIT`.
pub const GAIN_LIMIT: i8 = 12;
/// Peaking filters an octave wide.
const Q: f32 = 1.41;

pub type Gains = [i8; 10];

pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub gains: Gains,
}

/// Built-in presets; the settings page shows them in this order, "custom" after them.
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "flat",
        name: "Ровно",
        gains: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    },
    Preset {
        id: "bass",
        name: "Басы",
        gains: [6, 5, 4, 2, 0, 0, 0, 0, 0, 0],
    },
    Preset {
        id: "treble",
        name: "Верха",
        gains: [0, 0, 0, 0, 0, 0, 1, 3, 5, 6],
    },
    Preset {
        id: "vocal",
        name: "Голос",
        gains: [-2, -2, -1, 1, 3, 3, 2, 1, 0, -1],
    },
    Preset {
        id: "rock",
        name: "Рок",
        gains: [4, 3, 2, 0, -1, -1, 1, 2, 3, 4],
    },
    Preset {
        id: "electronic",
        name: "Электроника",
        gains: [5, 4, 1, 0, -2, 1, 0, 1, 4, 5],
    },
    Preset {
        id: "acoustic",
        name: "Акустика",
        gains: [3, 3, 2, 1, 1, 1, 2, 2, 2, 1],
    },
    Preset {
        id: "night",
        name: "Тихо",
        gains: [-3, -2, -1, 0, 1, 1, 0, -1, -2, -3],
    },
];

#[must_use]
pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// What the player needs from the `[effects]` settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EffectsOptions {
    /// Band gains; `None` = equalizer off.
    pub eq: Option<Gains>,
    /// Fade out the end of a track and fade in the next one over this long; 0 = off.
    pub fade_ms: u32,
    pub visualizer: bool,
}

impl EffectsOptions {
    /// Something here needs Tonearm's PipeWire sink.
    #[must_use]
    pub fn needs_sink(&self) -> bool {
        self.eq.is_some() || self.fade_ms > 0 || self.visualizer
    }
}

/// These settings work on this system at all (Linux; PipeWire is checked when they start).
#[must_use]
pub const fn supported() -> bool {
    cfg!(target_os = "linux")
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Gain in front of the bands so boosts do not clip: half the largest boost.
fn preamp(eq: Option<&Gains>) -> f32 {
    let boost = eq.map_or(0, |g| g.iter().copied().max().unwrap_or(0).max(0));
    db_to_gain(-f32::from(boost) / 2.0)
}

fn clamp(gains: &Gains) -> Gains {
    gains.map(|g| g.clamp(-GAIN_LIMIT, GAIN_LIMIT))
}

/// `Props` for the filter-chain: every band, plus the output gain (`fade` × preamp).
/// An equalizer that is off is all zeros, which the peaking filters pass through unchanged.
pub(crate) fn props(eq: Option<&Gains>, fade: f32) -> String {
    let gains = eq.map(clamp).unwrap_or_default();
    let mut s = String::from("{ params = [");
    for (i, g) in gains.iter().enumerate() {
        s.push_str(&format!(" \"b{i}:Gain\" {g}.0"));
    }
    let out = (fade.clamp(0.0, 1.0) * preamp(eq)).clamp(0.0, 1.0);
    s.push_str(&format!(" \"out:Gain 1\" {out:.4} ] }}"));
    s
}

/// A standalone `pipewire -c` config with the filter-chain sink `node` in it.
pub(crate) fn sink_config(node: &str, eq: Option<&Gains>) -> String {
    let gains = eq.map(clamp).unwrap_or_default();
    let mut nodes = String::new();
    let mut links = String::new();
    for (i, (hz, g)) in BANDS_HZ.iter().zip(gains).enumerate() {
        nodes.push_str(&format!(
            "          {{ type = builtin name = b{i} label = bq_peaking control = {{ \"Freq\" = {hz}.0 \"Q\" = {Q} \"Gain\" = {g}.0 }} }}\n"
        ));
        let next = if i + 1 < BANDS_HZ.len() {
            format!("b{}:In", i + 1)
        } else {
            "out:In 1".to_owned()
        };
        links.push_str(&format!(
            "          {{ output = \"b{i}:Out\" input = \"{next}\" }}\n"
        ));
    }
    let out = preamp(eq);
    format!(
        r#"# Made by Tonearm at every start; edits are lost.
context.properties = {{ log.level = 1 }}
context.spa-libs = {{
  audio.convert.* = audioconvert/libspa-audioconvert
  support.*       = support/libspa-support
}}
context.modules = [
  {{ name = libpipewire-module-rt flags = [ ifexists nofail ] }}
  {{ name = libpipewire-module-protocol-native }}
  {{ name = libpipewire-module-client-node }}
  {{ name = libpipewire-module-adapter }}
  {{ name = libpipewire-module-filter-chain
    args = {{
      node.description = "Tonearm"
      media.name = "Tonearm"
      filter.graph = {{
        nodes = [
{nodes}          {{ type = builtin name = out label = mixer control = {{ "Gain 1" = {out:.4} }} }}
        ]
        links = [
{links}        ]
      }}
      audio.channels = 2
      audio.position = [ FL FR ]
      capture.props = {{
        node.name = "{node}"
        media.class = Audio/Sink
        priority.session = 0
        priority.driver = 0
      }}
      playback.props = {{
        node.name = "{node}.out"
        node.passive = true
      }}
    }}
  }}
]
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_unique_and_in_range() {
        for (i, p) in PRESETS.iter().enumerate() {
            assert!(PRESETS[..i].iter().all(|q| q.id != p.id), "{}", p.id);
            assert!(p.gains.iter().all(|g| g.abs() <= GAIN_LIMIT));
        }
        assert_eq!(preset("flat").unwrap().gains, [0; 10]);
    }

    #[test]
    fn props_hold_every_band_and_the_output() {
        let p = props(Some(&[6, 0, 0, 0, 0, 0, 0, 0, 0, -20]), 1.0);
        assert!(p.contains("\"b0:Gain\" 6.0"));
        assert!(p.contains("\"b9:Gain\" -12.0"), "clamped: {p}");
        // +6 dB boost → preamp −3 dB.
        assert!(p.contains("\"out:Gain 1\" 0.7079"), "{p}");
        let off = props(None, 0.0);
        assert!(off.contains("\"b3:Gain\" 0.0") && off.contains("\"out:Gain 1\" 0.0000"));
    }

    #[test]
    fn sink_config_chains_the_bands() {
        let c = sink_config("tonearm_eq", None);
        assert!(c.contains("node.name = \"tonearm_eq\""));
        assert!(c.contains("output = \"b9:Out\" input = \"out:In 1\""));
        assert_eq!(c.matches("bq_peaking").count(), 10);
    }

    #[test]
    fn sink_only_when_needed() {
        assert!(!EffectsOptions::default().needs_sink());
        let o = EffectsOptions {
            fade_ms: 3000,
            ..EffectsOptions::default()
        };
        assert!(o.needs_sink());
    }
}
