//! Cover art: download, disk cache with a size limit, decode, resize, accent colour.
//!
//! Each cover is decoded once at the size it is shown; the accent colour (a vivid, readable
//! colour picked from the picture) tints the UI around the playing track and the open page.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use image::imageops::FilterType;

/// Decoded cover, RGBA8.
#[derive(Clone)]
pub struct Cover {
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub color: Vec<u8>,
    /// Vivid colour from the picture, lightened or darkened to read well on the dark theme.
    pub accent: [u8; 3],
}

impl std::fmt::Debug for Cover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cover({} {}x{})", self.url, self.width, self.height)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoverError {
    #[error("загрузка обложки: {0}")]
    Network(String),
    #[error("обложка повреждена")]
    Decode,
}

pub struct CoverCache {
    dir: Option<PathBuf>,
    /// Changeable at runtime from the settings page.
    max_bytes: std::sync::atomic::AtomicU64,
    http: reqwest::Client,
}

/// Only Spotify's image CDNs are fetched. Spotify serves its own playlists (blends, "This Is",
/// mixes, daylist, Wrapped…) from many hosts under these two domains, so match the domain.
fn allowed(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.contains(['@', ':']) {
        return false;
    }
    [".scdn.co", ".spotifycdn.com", ".spotify.com"]
        .iter()
        .any(|d| host.ends_with(d))
}

/// `spotify:image:<hex>` / `spotify:mosaic:<id>:<id>…` (as some internal endpoints return them)
/// → a CDN url; other urls pass through.
#[must_use]
pub fn normalize_url(url: &str) -> String {
    if let Some(hex) = url.strip_prefix("spotify:image:") {
        return format!("https://i.scdn.co/image/{hex}");
    }
    if let Some(ids) = url.strip_prefix("spotify:mosaic:") {
        let ids: String = ids.split(':').collect();
        return format!("https://mosaic.scdn.co/640/{ids}");
    }
    url.to_owned()
}

fn file_name(url: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, url.as_bytes());
    digest
        .as_ref()
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl CoverCache {
    /// Cache in the platform cache dir (`~/.cache/tonearm/covers`, `%LOCALAPPDATA%\tonearm\cache\covers`).
    #[must_use]
    pub fn new(max_bytes: u64) -> Self {
        let dir = directories::ProjectDirs::from("", "", "tonearm")
            .map(|d| d.cache_dir().join("covers"))
            .filter(|d| std::fs::create_dir_all(d).is_ok());
        Self::with_dir(dir, max_bytes)
    }

    #[must_use]
    pub fn limit(&self) -> u64 {
        self.max_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// New size limit; prunes right away if the cache is over it.
    pub fn set_limit(&self, max_bytes: u64) {
        self.max_bytes
            .store(max_bytes, std::sync::atomic::Ordering::Relaxed);
        if let Some(dir) = &self.dir {
            prune(dir, max_bytes);
        }
    }

    /// Bytes on disk.
    #[must_use]
    pub fn usage(&self) -> u64 {
        self.dir.as_deref().map_or(0, dir_size)
    }

    pub fn clear(&self) {
        if let Some(dir) = &self.dir {
            prune(dir, 0);
        }
    }

    #[must_use]
    pub fn with_dir(dir: Option<PathBuf>, max_bytes: u64) -> Self {
        Self {
            dir,
            max_bytes: std::sync::atomic::AtomicU64::new(max_bytes),
            http: reqwest::Client::builder()
                // A blocked host hangs on connect: give up quickly and try the next mirror.
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Returns the cover scaled to fit `size`×`size`.
    pub async fn get(&self, url: &str, size: u32) -> Result<Cover, CoverError> {
        let url = normalize_url(url);
        if !allowed(&url) {
            return Err(CoverError::Network(format!("недопустимый адрес {url}")));
        }
        // Mosaics are put together here from the four covers: one host fewer to depend on.
        if let Some(ids) = mosaic_ids(&url) {
            let ids: Vec<String> = ids.iter().map(|id| sized_id(id, size / 2)).collect();
            let parts = futures_util::future::join_all(ids.iter().map(|id| self.image(id))).await;
            let parts: Vec<Vec<u8>> = parts.into_iter().collect::<Result<_, _>>()?;
            return tokio::task::spawn_blocking(move || compose(&url, &parts, size))
                .await
                .map_err(|_| CoverError::Decode)?;
        }
        let bytes = match image_id(&url) {
            Some(id) => self.image(&sized_id(&id, size)).await?,
            None => self.fetch_cached(&url, std::slice::from_ref(&url)).await?,
        };
        tokio::task::spawn_blocking(move || decode(&url, &bytes, size))
            .await
            .map_err(|_| CoverError::Decode)?
    }

    /// An image by its id, from whichever of Spotify's image hosts answers (some networks
    /// block `i.scdn.co` but not the others). The host that worked last is tried first.
    async fn image(&self, id: &str) -> Result<Vec<u8>, CoverError> {
        let first = PREFERRED_HOST.load(Ordering::Relaxed) % IMAGE_HOSTS.len();
        let urls: Vec<String> = (0..IMAGE_HOSTS.len())
            .map(|i| {
                format!(
                    "https://{}/image/{id}",
                    IMAGE_HOSTS[(first + i) % IMAGE_HOSTS.len()]
                )
            })
            .collect();
        // Cached under one name whatever host served it.
        let key = format!("https://i.scdn.co/image/{id}");
        self.fetch_cached(&key, &urls).await
    }

    /// From the disk cache under `key`, else the first of `urls` that answers.
    async fn fetch_cached(&self, key: &str, urls: &[String]) -> Result<Vec<u8>, CoverError> {
        let path = self.dir.as_ref().map(|d| d.join(file_name(key)));
        if let Some(p) = &path
            && let Ok(data) = tokio::fs::read(p).await
        {
            // Touch so pruning keeps recently used covers.
            let _ = filetime_touch(p);
            return Ok(data);
        }
        let mut last = String::new();
        let mut data = None;
        // Every host once, then once more: a blocked host costs one short timeout, and the
        // next requests start from the host that answered.
        'passes: for pass in 0..2 {
            if pass == 1 {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
            for url in urls {
                let host = IMAGE_HOSTS.iter().position(|h| host_of(url) == *h);
                match self.fetch(url).await {
                    Ok(d) => {
                        if let Some(i) = host {
                            PREFERRED_HOST.store(i, Ordering::Relaxed);
                        }
                        data = Some(d);
                        break 'passes;
                    }
                    Err(e) => {
                        tracing::debug!(%e, %url, pass, "cover fetch failed");
                        if let Some(i) = host {
                            let _ = PREFERRED_HOST.compare_exchange(
                                i,
                                (i + 1) % IMAGE_HOSTS.len(),
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );
                        }
                        last = format!("{url}: {e}");
                    }
                }
            }
        }
        let data = data.ok_or(CoverError::Network(last))?;
        if let Some(p) = path
            && tokio::fs::write(&p, &data).await.is_ok()
            && let Some(dir) = self.dir.clone()
        {
            let max = self.limit();
            tokio::task::spawn_blocking(move || prune(&dir, max));
        }
        Ok(data)
    }

    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| e.to_string())?;
        Ok(resp.bytes().await.map_err(|e| e.to_string())?.to_vec())
    }
}

/// Hosts that serve `/image/<id>` for the same ids.
const IMAGE_HOSTS: [&str; 3] = [
    "i.scdn.co",
    "image-cdn-ak.spotifycdn.com",
    "image-cdn-fa.spotifycdn.com",
];
static PREFERRED_HOST: AtomicUsize = AtomicUsize::new(0);

fn host_of(url: &str) -> &str {
    url.strip_prefix("https://")
        .and_then(|r| r.split('/').next())
        .unwrap_or("")
}

/// The id of an image on one of the [`IMAGE_HOSTS`].
fn image_id(url: &str) -> Option<String> {
    let host = host_of(url);
    if !IMAGE_HOSTS.contains(&host) {
        return None;
    }
    let id = url
        .strip_prefix("https://")?
        .strip_prefix(host)?
        .strip_prefix("/image/")?;
    let id = id.split(['?', '#']).next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| id.to_owned())
}

/// Album cover ids name their rendition (`…1e02…` 300 px, `…b273…` 640 px, `…4851…` 64 px):
/// pick the one that matches the size it is shown at, so HiDPI screens get sharp covers and
/// row thumbnails do not download big pictures.
fn sized_id(id: &str, size: u32) -> String {
    const PREFIX: &str = "ab67616d0000";
    const RENDITIONS: [(&str, u32); 3] = [("4851", 64), ("1e02", 300), ("b273", 640)];
    let Some(rest) = id.strip_prefix(PREFIX) else {
        return id.to_owned();
    };
    let (Some(code), Some(tail)) = (rest.get(..4), rest.get(4..)) else {
        return id.to_owned();
    };
    if !RENDITIONS.iter().any(|(c, _)| *c == code) {
        return id.to_owned();
    }
    let want = RENDITIONS
        .iter()
        .find(|(_, px)| *px >= size)
        .unwrap_or(&RENDITIONS[2])
        .0;
    format!("{PREFIX}{want}{tail}")
}

/// `https://mosaic.scdn.co/<size>/<id><id><id><id>` → the four 40-character image ids.
fn mosaic_ids(url: &str) -> Option<Vec<String>> {
    let rest = url.strip_prefix("https://mosaic.scdn.co/")?;
    let ids = rest.split_once('/')?.1.split(['?', '#']).next()?;
    if ids.len() != 160 || !ids.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        (0..4)
            .map(|i| ids[i * 40..(i + 1) * 40].to_owned())
            .collect(),
    )
}

/// Four covers in a 2×2 square, like Spotify's playlist mosaics.
fn compose(url: &str, parts: &[Vec<u8>], size: u32) -> Result<Cover, CoverError> {
    let half = (size / 2).max(1);
    let mut out = image::RgbaImage::new(half * 2, half * 2);
    for (i, bytes) in parts.iter().enumerate().take(4) {
        let img = image::load_from_memory(bytes).map_err(|_| CoverError::Decode)?;
        let tile = img
            .resize_to_fill(half, half, FilterType::Triangle)
            .to_rgba8();
        let i = u32::try_from(i).unwrap_or(0);
        image::imageops::replace(
            &mut out,
            &tile,
            i64::from(i % 2 * half),
            i64::from(i / 2 * half),
        );
    }
    let (width, height) = out.dimensions();
    let accent = accent(out.as_raw());
    Ok(Cover {
        url: url.to_owned(),
        width,
        height,
        color: out.into_raw(),
        accent,
    })
}

fn filetime_touch(path: &Path) -> std::io::Result<()> {
    std::fs::File::options()
        .append(true)
        .open(path)?
        .set_modified(SystemTime::now())
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|e| e.metadata().ok())
                .filter(std::fs::Metadata::is_file)
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// Deletes the least recently used files until the directory fits into `max_bytes`.
pub fn prune(dir: &Path, max_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file().then(|| {
                (
                    m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    m.len(),
                    e.path(),
                )
            })
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= max_bytes {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

fn decode(url: &str, bytes: &[u8], size: u32) -> Result<Cover, CoverError> {
    let img = image::load_from_memory(bytes).map_err(|_| CoverError::Decode)?;
    let img = if img.width() > size || img.height() > size {
        img.resize(size, size, FilterType::Triangle)
    } else {
        img
    };
    let color = img.to_rgba8();
    let (width, height) = color.dimensions();
    let accent = accent(color.as_raw());
    Ok(Cover {
        url: url.to_owned(),
        width,
        height,
        color: color.into_raw(),
        accent,
    })
}

/// Picks the accent colour: a saturation-weighted average of the pixels (so a vivid area wins
/// over a large grey one), then brought into a lightness band that reads on a dark background.
#[must_use]
pub fn accent(rgba: &[u8]) -> [u8; 3] {
    let (mut r, mut g, mut b, mut wsum) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for px in rgba.as_chunks::<4>().0.iter().step_by(3) {
        let (pr, pg, pb) = (f32::from(px[0]), f32::from(px[1]), f32::from(px[2]));
        let max = pr.max(pg).max(pb);
        let min = pr.min(pg).min(pb);
        let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
        // Skip near-black and near-white: they say nothing about the cover's colour.
        let w = sat * sat + 0.02;
        let w = if max < 30.0 || min > 235.0 {
            w * 0.1
        } else {
            w
        };
        r += pr * w;
        g += pg * w;
        b += pb * w;
        wsum += w;
    }
    if wsum <= 0.0 {
        return [0xFF, 0x6A, 0x3D];
    }
    let (h, s, _) = rgb_to_hsl(r / wsum, g / wsum, b / wsum);
    // Keep some colour even for washed-out covers; fixed lightness keeps text on it readable.
    let s = (s * 1.25).clamp(0.25, 0.85);
    hsl_to_rgb(h, s, 0.62)
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = f32::midpoint(max, min);
    if (max - min).abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [u8; 3] {
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let ch = |t: f32| {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let v = (v * 255.0).round().clamp(0.0, 255.0) as u8;
        v
    };
    [ch(h + 1.0 / 3.0), ch(h), ch(h - 1.0 / 3.0)]
}

#[cfg(test)]
mod tests {
    #[test]
    fn album_cover_rendition_follows_size() {
        let tail = "2c5b24ecfa39523a75c993c4";
        let id = format!("ab67616d00001e02{tail}");
        assert_eq!(sized_id(&id, 600), format!("ab67616d0000b273{tail}"));
        assert_eq!(sized_id(&id, 200), id);
        assert_eq!(sized_id(&id, 48), format!("ab67616d00004851{tail}"));
        assert_eq!(sized_id(&id, 2000), format!("ab67616d0000b273{tail}"));
        assert_eq!(
            sized_id("ab6761610000e5eb1234", 600),
            "ab6761610000e5eb1234"
        );
    }

    #[test]
    fn image_ids_and_mosaics_are_recognised() {
        let id = "ab67616d0000b2732c5b24ecfa39523a75c993c4";
        assert_eq!(
            image_id(&format!("https://i.scdn.co/image/{id}")).as_deref(),
            Some(id)
        );
        assert_eq!(
            image_id(&format!("https://image-cdn-ak.spotifycdn.com/image/{id}")).as_deref(),
            Some(id)
        );
        assert_eq!(image_id("https://daylist.spotifycdn.com/x.jpg"), None);
        let m = format!("https://mosaic.scdn.co/640/{}", id.repeat(4));
        assert_eq!(mosaic_ids(&m).unwrap(), vec![id.to_owned(); 4]);
        assert!(mosaic_ids("https://mosaic.scdn.co/640/abc").is_none());
    }

    #[test]
    fn mosaic_is_composed_from_four_pictures() {
        let png = |c: [u8; 4]| {
            let img = image::RgbaImage::from_pixel(8, 8, image::Rgba(c));
            let mut out = std::io::Cursor::new(Vec::new());
            img.write_to(&mut out, image::ImageFormat::Png).unwrap();
            out.into_inner()
        };
        let parts = vec![
            png([255, 0, 0, 255]),
            png([0, 255, 0, 255]),
            png([0, 0, 255, 255]),
            png([255, 255, 0, 255]),
        ];
        let c = compose("m", &parts, 20).unwrap();
        assert_eq!((c.width, c.height), (20, 20));
        let px = |x: usize, y: usize| &c.color[(y * 20 + x) * 4..(y * 20 + x) * 4 + 3];
        assert_eq!(px(2, 2), [255, 0, 0]);
        assert_eq!(px(17, 2), [0, 255, 0]);
        assert_eq!(px(2, 17), [0, 0, 255]);
        assert_eq!(px(17, 17), [255, 255, 0]);
    }

    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 40, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn decode_resizes_and_picks_accent() {
        let c = decode("u", &png(640, 640), 64).unwrap();
        assert_eq!((c.width, c.height), (64, 64));
        assert_eq!(c.color.len(), 64 * 64 * 4);
        assert_eq!(&c.color[..3], &[200, 40, 30]);
        // A red cover gives a red accent.
        let [r, g, b] = c.accent;
        assert!(r > g && r > b, "{:?}", c.accent);
    }

    #[test]
    fn accent_of_grey_is_not_black() {
        let grey = vec![128u8; 16 * 16 * 4];
        let [r, g, b] = accent(&grey);
        assert!(u16::from(r) + u16::from(g) + u16::from(b) > 300);
    }

    #[test]
    fn spotify_uris_become_urls() {
        assert_eq!(
            normalize_url("spotify:image:ab12"),
            "https://i.scdn.co/image/ab12"
        );
        assert_eq!(
            normalize_url("spotify:mosaic:a:b:c:d"),
            "https://mosaic.scdn.co/640/abcd"
        );
        assert_eq!(normalize_url("https://x.scdn.co/y"), "https://x.scdn.co/y");
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(decode("u", b"not an image", 64).is_err());
    }

    #[test]
    fn prune_removes_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        for (i, name) in ["old", "mid", "new"].iter().enumerate() {
            let p = dir.path().join(name);
            std::fs::write(&p, vec![0u8; 100]).unwrap();
            let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i as u64);
            std::fs::File::options()
                .append(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        prune(dir.path(), 200);
        assert!(!dir.path().join("old").exists());
        assert!(dir.path().join("mid").exists() && dir.path().join("new").exists());
    }

    #[test]
    fn only_spotify_cdn() {
        assert!(allowed("https://i.scdn.co/image/ab67"));
        assert!(allowed(
            "https://blend-playlist-covers.spotifycdn.com/v2/blend_DEFAULT-x.jpg"
        ));
        assert!(allowed("https://thisis-images.spotifycdn.com/x"));
        assert!(!allowed("http://i.scdn.co/image/x"));
        assert!(!allowed("https://example.com/x.jpg"));
        assert!(!allowed("https://scdn.co.evil.com/x.jpg"));
        assert!(!allowed("https://evil.com@i.scdn.co/x"));
    }
}
