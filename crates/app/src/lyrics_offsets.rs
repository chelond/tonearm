//! Per-track lyrics timing fix: "the words come 0.5 s early" is remembered for that track.
//! Kept in `lyrics-offsets.json` next to the config.

use std::collections::HashMap;
use std::path::PathBuf;

const FILE: &str = "lyrics-offsets.json";
/// More tracks than this: the new offset is still applied, just not remembered.
const MAX_TRACKS: usize = 5000;
/// ±10 s: anything further off is a different recording, not a timing slip.
pub const LIMIT_MS: i32 = 10_000;

fn file() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "tonearm").map(|d| d.config_dir().join(FILE))
}

#[must_use]
pub fn load() -> HashMap<String, i32> {
    file()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<HashMap<String, i32>>(&b).ok())
        .unwrap_or_default()
}

/// Sets the offset of `uri` (0 removes it) and queues a save. Returns the value in effect.
pub fn set(map: &mut HashMap<String, i32>, uri: &str, ms: i32) -> i32 {
    let ms = ms.clamp(-LIMIT_MS, LIMIT_MS);
    if ms == 0 {
        map.remove(uri);
    } else if map.len() < MAX_TRACKS || map.contains_key(uri) {
        map.insert(uri.to_owned(), ms);
    } else {
        return ms;
    }
    if let Some(p) = file() {
        let snapshot = map.clone();
        tonearm_core::fsutil::write_later(p, tonearm_core::fsutil::Access::Normal, move || {
            serde_json::to_vec(&snapshot).ok()
        });
    }
    ms
}

/// `500` → `"+0,5 с"`, `-1200` → `"−1,2 с"`, `0` → `"0,0 с"` (to a tenth of a second).
#[must_use]
pub fn label(ms: i32) -> String {
    let sign = match ms.signum() {
        1 => "+",
        -1 => "\u{2212}",
        _ => "",
    };
    let tenths = (ms.unsigned_abs() + 50) / 100;
    format!("{sign}{},{} с", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_limits() {
        assert_eq!(label(0), "0,0 с");
        assert_eq!(label(500), "+0,5 с");
        assert_eq!(label(-1200), "\u{2212}1,2 с");
        let mut map = HashMap::new();
        assert_eq!(set(&mut map, "t", 99_999), LIMIT_MS);
        assert_eq!(set(&mut map, "t", 0), 0);
        assert!(map.is_empty());
    }
}
