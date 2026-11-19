//! Cover art: download, disk cache with a size limit, decode, resize, grayscale.
//!
//! The "Пульт" design shows every cover in grayscale except the playing one, so each cover is
//! decoded once into both variants here and the UI only picks one.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::imageops::FilterType;

/// Decoded cover, RGBA8.
#[derive(Clone)]
pub struct Cover {
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub color: Vec<u8>,
    pub gray: Vec<u8>,
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

/// Only Spotify's image CDN is fetched.
fn allowed(url: &str) -> bool {
    url.starts_with("https://i.scdn.co/")
        || url.starts_with("https://mosaic.scdn.co/")
        || url.starts_with("https://image-cdn-")
        || url.starts_with("https://wrapped-images.spotifycdn.com/")
        || url.starts_with("https://pickasso.spotifycdn.com/")
        || url.starts_with("https://seeded-session-images.scdn.co/")
        || url.starts_with("https://daylist.spotifycdn.com/")
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
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Returns the cover scaled to fit `size`×`size`.
    pub async fn get(&self, url: &str, size: u32) -> Result<Cover, CoverError> {
        if !allowed(url) {
            return Err(CoverError::Network("недопустимый адрес".into()));
        }
        let bytes = self.bytes(url).await?;
        let url = url.to_owned();
        tokio::task::spawn_blocking(move || decode(&url, &bytes, size))
            .await
            .map_err(|_| CoverError::Decode)?
    }

    async fn bytes(&self, url: &str) -> Result<Vec<u8>, CoverError> {
        let path = self.dir.as_ref().map(|d| d.join(file_name(url)));
        if let Some(p) = &path
            && let Ok(data) = tokio::fs::read(p).await
        {
            // Touch so pruning keeps recently used covers.
            let _ = filetime_touch(p);
            return Ok(data);
        }
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| CoverError::Network(e.to_string()))?;
        let data = resp
            .bytes()
            .await
            .map_err(|e| CoverError::Network(e.to_string()))?
            .to_vec();
        if let Some(p) = path
            && tokio::fs::write(&p, &data).await.is_ok()
            && let Some(dir) = self.dir.clone()
        {
            let max = self.limit();
            tokio::task::spawn_blocking(move || prune(&dir, max));
        }
        Ok(data)
    }
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
    let mut gray = color.clone();
    for px in gray.pixels_mut() {
        let [r, g, b, a] = px.0;
        let l = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
        // Slight contrast boost around mid-grey, as in the mockups.
        let l = ((l - 128.0) * 1.1 + 128.0).clamp(0.0, 255.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let l = l as u8;
        px.0 = [l, l, l, a];
    }
    Ok(Cover {
        url: url.to_owned(),
        width,
        height,
        color: color.into_raw(),
        gray: gray.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 40, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn decode_resizes_and_makes_gray() {
        let c = decode("u", &png(640, 640), 64).unwrap();
        assert_eq!((c.width, c.height), (64, 64));
        assert_eq!(c.color.len(), 64 * 64 * 4);
        let [r, g, b, _] = [c.gray[0], c.gray[1], c.gray[2], c.gray[3]];
        assert!(r == g && g == b);
        assert_eq!(&c.color[..3], &[200, 40, 30]);
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
        assert!(!allowed("http://i.scdn.co/image/x"));
        assert!(!allowed("https://example.com/x.jpg"));
    }
}
