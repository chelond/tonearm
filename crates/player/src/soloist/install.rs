//! Downloads Spotify's Soloist build for this computer straight from Spotify's CDN (it may not
//! be redistributed, so it is never bundled), keeps it in the data directory, and renews it:
//! builds stop working 90 days after their build date.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CDN: &str = "https://soloist-builds.spotifycdn.com";
const ARCHIVE_LIMIT: usize = 80 * 1024 * 1024;
const EXPANDED_LIMIT: u64 = 160 * 1024 * 1024;
pub const DAY: u64 = 86_400;
/// Builds expire this long after their build date.
pub const LIFETIME: u64 = 90 * DAY;
/// Look for a newer build once the current one is this old, at most daily.
const RENEW_AFTER: u64 = 60 * DAY;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("Spotify не выпускает Soloist для этой системы")]
    Unsupported,
    #[error("не удалось скачать Soloist: {0}")]
    Download(String),
    #[error("архив Soloist повреждён: {0}")]
    Archive(String),
    #[error("сборка Soloist устарела, а новой у Spotify пока нет. Проверьте дату на компьютере")]
    Expired,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// The installed build, `current.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    pub version: String,
    /// Build date, Unix seconds (from `soloist --version`).
    pub built: u64,
    /// SHA-256 of the downloaded archive; also the build's folder name.
    pub digest: String,
    /// Last time the CDN was asked for a newer build.
    pub checked: u64,
}

impl Build {
    #[must_use]
    pub fn exe(&self, root: &Path) -> PathBuf {
        root.join(&self.digest).join(executable())
    }

    #[must_use]
    pub fn usable(&self, now: u64) -> bool {
        self.built <= now.saturating_add(DAY) && now < self.built.saturating_add(LIFETIME)
    }

    #[must_use]
    pub fn renew_due(&self, now: u64) -> bool {
        now.saturating_sub(self.built) >= RENEW_AFTER && now.saturating_sub(self.checked) >= DAY
    }

    /// Whole days left before the build stops starting.
    #[must_use]
    pub fn days_left(&self, now: u64) -> u64 {
        self.built.saturating_add(LIFETIME).saturating_sub(now) / DAY
    }
}

#[must_use]
pub fn artifact(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("soloist_release_x86_64.tar.gz"),
        ("linux", "aarch64") => Some("soloist_release_arm64.tar.gz"),
        ("linux", "arm") => Some("soloist_release_arm32.tar.gz"),
        ("macos", "aarch64") => Some("soloist_release_osx_arm64.tar.gz"),
        ("windows", "x86_64") => Some("soloist_release_windows_x86_64.zip"),
        _ => None,
    }
}

#[must_use]
pub fn supported() -> bool {
    artifact(std::env::consts::OS, std::env::consts::ARCH).is_some()
}

#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn executable() -> &'static str {
    if cfg!(windows) {
        "soloist.exe"
    } else {
        "soloist"
    }
}

/// The installed build, if it is still there and not expired.
pub async fn current(root: &Path) -> Option<Build> {
    let bytes = tokio::fs::read(root.join("current.json")).await.ok()?;
    let build: Build = serde_json::from_slice(&bytes).ok()?;
    (build.usable(now()) && tokio::fs::metadata(build.exe(root)).await.is_ok()).then_some(build)
}

async fn remember(root: &Path, build: &Build) -> Result<(), InstallError> {
    let json =
        serde_json::to_vec_pretty(build).map_err(|e| InstallError::Archive(e.to_string()))?;
    let tmp = root.join("current.json.tmp");
    tokio::fs::write(&tmp, json).await?;
    tokio::fs::rename(&tmp, root.join("current.json")).await?;
    Ok(())
}

/// Records that the CDN was asked today, so a failed or unchanged check waits a day.
pub async fn mark_checked(root: &Path, build: &Build) {
    let mut build = build.clone();
    build.checked = now();
    let _ = remember(root, &build).await;
}

/// Downloads the current build from Spotify and makes it `current.json`. `progress` gets 0..=100.
/// Returns `None` when Spotify still serves the build that is installed (`installed`).
pub async fn download(
    root: &Path,
    installed: Option<&Build>,
    progress: impl Fn(u8),
) -> Result<Option<Build>, InstallError> {
    let name =
        artifact(std::env::consts::OS, std::env::consts::ARCH).ok_or(InstallError::Unsupported)?;
    tokio::fs::create_dir_all(root).await?;
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| InstallError::Download(e.to_string()))?;
    let mut resp = http
        .get(format!("{CDN}/{name}"))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| InstallError::Download(e.to_string()))?;
    let total = resp.content_length().unwrap_or(0);
    if total > ARCHIVE_LIMIT as u64 {
        return Err(InstallError::Download("архив слишком большой".into()));
    }
    let mut data = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
    let mut last = u8::MAX;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| InstallError::Download(e.to_string()))?
    {
        data.extend_from_slice(&chunk);
        if data.len() > ARCHIVE_LIMIT {
            return Err(InstallError::Download("архив слишком большой".into()));
        }
        if let Some(pct) = (data.len() as u64 * 100).checked_div(total) {
            let pct = u8::try_from(pct.min(100)).unwrap_or(100);
            if pct != last {
                last = pct;
                progress(pct);
            }
        }
    }
    let digest = hex(ring::digest::digest(&ring::digest::SHA256, &data).as_ref());
    if let Some(b) = installed
        && b.digest == digest
    {
        return Ok(None);
    }

    let dir = root.join(&digest);
    let exe = dir.join(executable());
    let target = exe.clone();
    let is_zip = name.ends_with(".zip");
    tokio::task::spawn_blocking(move || extract(&data, is_zip, &target))
        .await
        .map_err(|e| InstallError::Archive(e.to_string()))??;

    let (version, built) = match probe(&exe).await {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(e);
        }
    };
    let build = Build {
        version,
        built,
        digest,
        checked: now(),
    };
    if !build.usable(now()) {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Err(InstallError::Expired);
    }
    remember(root, &build).await?;
    Ok(Some(build))
}

/// Removes build folders other than `keep` (call before starting `keep`, never while an old
/// build may still run).
pub async fn collect(root: &Path, keep: &Build) {
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_build = name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit());
        if is_build && name != keep.digest {
            let _ = tokio::fs::remove_dir_all(entry.path()).await;
        }
    }
}

/// Unpacks only the executable (whatever folder the archive puts it in) to `target`.
fn extract(data: &[u8], is_zip: bool, target: &Path) -> Result<(), InstallError> {
    let want = executable();
    let mut out = Vec::new();
    if is_zip {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data))
            .map_err(|e| InstallError::Archive(e.to_string()))?;
        for i in 0..zip.len() {
            let file = zip
                .by_index(i)
                .map_err(|e| InstallError::Archive(e.to_string()))?;
            let matches = file
                .enclosed_name()
                .and_then(|p| p.file_name().map(|n| n == want))
                .unwrap_or(false);
            if matches && file.is_file() {
                file.take(EXPANDED_LIMIT).read_to_end(&mut out)?;
                break;
            }
        }
    } else {
        let gz = flate2::read::GzDecoder::new(data);
        let mut tar = tar::Archive::new(gz);
        for entry in tar.entries()? {
            let entry = entry?;
            let path = entry.path()?.into_owned();
            let safe = path.components().all(|c| {
                matches!(
                    c,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            });
            if safe
                && entry.header().entry_type().is_file()
                && path.file_name().is_some_and(|n| n == want)
            {
                entry.take(EXPANDED_LIMIT).read_to_end(&mut out)?;
                break;
            }
        }
    }
    if out.is_empty() {
        return Err(InstallError::Archive(format!("нет {want}")));
    }
    let dir = target.parent().unwrap_or(target);
    std::fs::create_dir_all(dir)?;
    std::fs::write(target, out)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// `soloist --version`: "soloist <version> … build <unix seconds> …".
async fn probe(exe: &Path) -> Result<(String, u64), InstallError> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--version").kill_on_drop(true);
    super::process::hide_window(&mut cmd);
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), cmd.output())
        .await
        .map_err(|_| InstallError::Archive("soloist --version не ответил".into()))??;
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    if out.status.code() == Some(super::process::EXIT_EXPIRED) {
        return Err(InstallError::Expired);
    }
    parse_version(&text).ok_or_else(|| {
        InstallError::Archive(format!(
            "неожиданный ответ soloist --version: {}",
            text.lines().next().unwrap_or("")
        ))
    })
}

fn parse_version(text: &str) -> Option<(String, u64)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let start = words.iter().position(|w| *w == "soloist")?;
    let version = (*words.get(start + 1)?).to_owned();
    let at = words.iter().position(|w| *w == "build")?;
    let built = words
        .get(at + 1)?
        .trim_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    Some((version, built))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line() {
        assert_eq!(
            parse_version("soloist 1.4.2 (abc) build 1763500000 linux\n"),
            Some(("1.4.2".into(), 1_763_500_000))
        );
        assert_eq!(parse_version("nope"), None);
    }

    #[test]
    fn expiry_and_renewal() {
        let b = Build {
            version: "1".into(),
            built: 1000 * DAY,
            digest: "0".repeat(64),
            checked: 1000 * DAY,
        };
        assert!(b.usable(1000 * DAY));
        assert!(!b.usable(1090 * DAY));
        assert!(!b.renew_due(1059 * DAY));
        assert!(b.renew_due(1060 * DAY));
        assert_eq!(b.days_left(1080 * DAY), 10);
    }

    #[test]
    fn unpacks_only_the_executable() {
        let dir = std::env::temp_dir().join(format!("tonearm-solo-{}", std::process::id()));
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("x/README.txt", opts).unwrap();
            std::io::Write::write_all(&mut zip, b"readme").unwrap();
            zip.start_file(format!("soloist_release/{}", executable()), opts)
                .unwrap();
            std::io::Write::write_all(&mut zip, b"binary").unwrap();
            zip.finish().unwrap();
        }
        let target = dir.join(executable());
        extract(buf.get_ref(), true, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"binary");

        let mut tgz = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut tgz, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("soloist_release/{}", executable()),
                &b"elf"[..],
            )
            .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        extract(&tgz, false, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"elf");
        let _ = std::fs::remove_dir_all(dir);
    }
}
