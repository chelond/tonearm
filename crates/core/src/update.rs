//! One-button self-update from GitHub Releases.
//!
//! The release feed is GitHub's `releases/latest` JSON (a mirror can serve the same shape, see
//! `[update] feed_url` in `config.toml`). Every release file has a detached ed25519 signature
//! next to it (`<file>.sig`, 64 raw bytes) made by CI; the public key is built into the app, so
//! a swapped file is never installed, whoever hosts it.
//!
//! How the new version gets in place depends on how the app was installed:
//! - a binary the user can write to (Windows exe, Linux tarball): the new file is written next
//!   to it and renamed over it; the running process keeps the old one until it restarts;
//! - a pacman package (`/usr/bin`): the signed `.pkg.tar.zst` from the release is installed
//!   with `pkexec pacman -U`, which asks for the password in a system dialog.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

/// Where to look for new versions unless `config.toml` names a mirror.
pub const DEFAULT_FEED: &str = "https://api.github.com/repos/chelond/tonearm/releases/latest";
/// Release page for people who want to look before updating.
pub const RELEASES_PAGE: &str = "https://github.com/chelond/tonearm/releases/latest";

/// ed25519 public key that release files are signed with (the private key is a CI secret).
const PUBLIC_KEY: [u8; 32] = [
    0x92, 0x25, 0x9d, 0xf2, 0xa1, 0x8f, 0xa6, 0x3a, 0x98, 0xe5, 0xaa, 0xf6, 0x3d, 0xa9, 0x44, 0x22,
    0xcf, 0x46, 0x64, 0x7d, 0x5f, 0xf6, 0x0f, 0x45, 0xbb, 0x29, 0x79, 0x3c, 0xb8, 0x9a, 0xf6, 0xba,
];

/// Release files are a few dozen MB; refuse anything absurd.
const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("сервер обновлений недоступен: {0}")]
    Network(String),
    #[error("в релизе нет файла для этой системы")]
    NoAsset,
    #[error("подпись файла не совпала, обновление не установлено")]
    BadSignature,
    #[error("нет прав на запись в {0}")]
    NotWritable(String),
    #[error("установка отменена")]
    Cancelled,
    #[error("{0}")]
    Install(String),
}

/// How this copy of the app can be replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    /// Write the new binary next to the running one and rename it over.
    Binary,
    /// Installed by pacman: install the new package through pkexec.
    Pacman,
}

/// A newer release, ready to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Without the leading `v`.
    pub version: String,
    /// Release notes (Markdown as written on GitHub).
    pub notes: String,
    pub page: String,
    pub asset_name: String,
    pub asset_url: String,
    pub sig_url: String,
    pub kind: InstallKind,
}

/// A downloaded, verified file waiting to be installed.
#[derive(Debug)]
pub struct Downloaded {
    path: PathBuf,
    kind: InstallKind,
}

#[derive(Deserialize)]
struct FeedRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<FeedAsset>,
}

#[derive(Deserialize)]
struct FeedAsset {
    name: String,
    browser_download_url: String,
}

static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Path of the running binary, read once at startup: after pacman replaces `/usr/bin/tonearm`
/// Linux reports the old one as "… (deleted)", which is no use for the restart.
pub fn current_exe() -> Option<PathBuf> {
    EXE.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
    })
    .clone()
}

/// Call at startup: remembers the binary path and removes what an earlier update left behind.
pub fn init() {
    let Some(exe) = current_exe() else { return };
    for leftover in [old_path(&exe), new_path(&exe)] {
        if leftover.exists() {
            match std::fs::remove_file(&leftover) {
                Ok(()) => tracing::info!(path = %leftover.display(), "removed update leftover"),
                Err(e) => tracing::debug!(error = %e, "cannot remove update leftover yet"),
            }
        }
    }
}

/// How this copy was installed.
#[must_use]
pub fn install_kind() -> InstallKind {
    let packaged = cfg!(target_os = "linux")
        && current_exe()
            .is_some_and(|p| p.starts_with("/usr/") && Path::new("/usr/bin/pacman").exists());
    if packaged {
        InstallKind::Pacman
    } else {
        InstallKind::Binary
    }
}

fn old_path(exe: &Path) -> PathBuf {
    sibling(exe, "old")
}

fn new_path(exe: &Path) -> PathBuf {
    sibling(exe, "new")
}

/// `tonearm.exe` → `tonearm.<tag>.exe`, `tonearm` → `tonearm.<tag>`.
fn sibling(exe: &Path, tag: &str) -> PathBuf {
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("tonearm");
    let name = match exe.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}.{tag}.{ext}"),
        None => format!("{stem}.{tag}"),
    };
    exe.with_file_name(name)
}

/// `"v1.2.3"` / `"1.2.3"` → `(1, 2, 3)`; a pre-release (`1.2.3-beta.1`) gives the same numbers
/// and `pre = true`.
fn parse_version(s: &str) -> Option<((u64, u64, u64), bool)> {
    let s = s.trim().trim_start_matches('v');
    // `+build` metadata does not count; `-beta.1` makes it a pre-release.
    let s = s.split('+').next().unwrap_or(s);
    let (core, pre) = match s.split_once('-') {
        Some((core, _)) => (core, true),
        None => (s, false),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(((major, minor, patch), pre))
}

/// Whether `candidate` is newer than `current`. Pre-releases are never offered, but a copy that
/// *is* a pre-release gets the final release of the same version (`0.3.0` > `0.3.0-beta.1`).
#[must_use]
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some((_, true)), _) => false,
        (Some((a, false)), Some((b, b_pre))) => a > b || (a == b && b_pre),
        _ => false,
    }
}

/// Name of the release file for this platform and install kind.
fn wanted_asset(name: &str, kind: InstallKind) -> bool {
    let arch = std::env::consts::ARCH;
    match (std::env::consts::OS, kind) {
        ("windows", _) => name == format!("tonearm-windows-{arch}.exe"),
        ("linux", InstallKind::Pacman) => {
            // makepkg may also split off an empty tonearm-debug package: never pick that one.
            name.starts_with("tonearm-")
                && !name.starts_with("tonearm-debug-")
                && name.ends_with(&format!("-{arch}.pkg.tar.zst"))
        }
        ("linux", InstallKind::Binary) => name == format!("tonearm-linux-{arch}"),
        _ => false,
    }
}

fn client(timeout: Option<Duration>) -> Result<reqwest::Client, UpdateError> {
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("tonearm/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10));
    if let Some(t) = timeout {
        b = b.timeout(t);
    }
    b.build().map_err(|e| UpdateError::Network(e.to_string()))
}

fn feed_or_default(feed: &str) -> &str {
    if feed.trim().is_empty() {
        DEFAULT_FEED
    } else {
        feed.trim()
    }
}

/// Asks the feed for the latest release; `Ok(None)` = this is the newest version.
pub async fn check(feed: &str, current: &str) -> Result<Option<Release>, UpdateError> {
    let kind = install_kind();
    let feed = feed_or_default(feed);
    if !https(feed) {
        return Err(UpdateError::Network(
            "адрес обновлений в config.toml должен начинаться с https://".into(),
        ));
    }
    let http = client(Some(Duration::from_secs(20)))?;
    let resp = http
        .get(feed)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(UpdateError::Network(format!(
            "HTTP {}",
            resp.status().as_u16()
        )));
    }
    let release: FeedRelease = resp
        .json()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?;
    pick(release, current, kind)
}

/// A file name with no directory part (`a/../b`, `..`, `C:\x` are refused): the name from the
/// feed becomes a path on disk.
fn plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\', ':'])
        && Path::new(name).file_name().and_then(|n| n.to_str()) == Some(name)
}

/// Downloads come over TLS only, whatever a custom feed says.
fn https(url: &str) -> bool {
    url.starts_with("https://")
}

/// The page handed to the OS opener: only GitHub release pages, never `file://` or app schemes.
fn safe_page(url: Option<String>) -> String {
    url.filter(|u| u.starts_with("https://github.com/"))
        .unwrap_or_else(|| RELEASES_PAGE.to_owned())
}

fn pick(
    release: FeedRelease,
    current: &str,
    kind: InstallKind,
) -> Result<Option<Release>, UpdateError> {
    if release.draft || release.prerelease || !is_newer(&release.tag_name, current) {
        return Ok(None);
    }
    let asset = release
        .assets
        .iter()
        .find(|a| plain_file_name(&a.name) && wanted_asset(&a.name, kind))
        .ok_or(UpdateError::NoAsset)?;
    let sig_name = format!("{}.sig", asset.name);
    let sig = release
        .assets
        .iter()
        .find(|a| a.name == sig_name)
        .ok_or(UpdateError::NoAsset)?;
    if !https(&asset.browser_download_url) || !https(&sig.browser_download_url) {
        return Err(UpdateError::Network(
            "сервер обновлений дал ссылку не по https".into(),
        ));
    }
    Ok(Some(Release {
        version: release.tag_name.trim_start_matches('v').to_owned(),
        notes: release.body.unwrap_or_default(),
        page: safe_page(release.html_url),
        asset_name: asset.name.clone(),
        asset_url: asset.browser_download_url.clone(),
        sig_url: sig.browser_download_url.clone(),
        kind,
    }))
}

/// What CI signs: this header, then the file. The version and the file name are part of the
/// signed message, so a signed file cannot be served as another version (no downgrade to an
/// older signed build under a newer tag) or as another file (a tarball in place of the exe).
fn signed_message(version: &str, name: &str, data: &[u8]) -> Vec<u8> {
    let header = format!("{SIGNATURE_DOMAIN}\n{version}\n{name}\n");
    let mut msg = Vec::with_capacity(header.len() + data.len());
    msg.extend_from_slice(header.as_bytes());
    msg.extend_from_slice(data);
    msg
}

/// First line of every signed message (also in `.github/workflows/ci.yml`).
const SIGNATURE_DOMAIN: &str = "tonearm-update-v1";

/// Checks the release file against the built-in key, for this version and file name.
fn verify(version: &str, name: &str, data: &[u8], signature: &[u8]) -> Result<(), UpdateError> {
    verify_with(&PUBLIC_KEY, version, name, data, signature)
}

fn verify_with(
    key: &[u8],
    version: &str,
    name: &str,
    data: &[u8],
    signature: &[u8],
) -> Result<(), UpdateError> {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(&signed_message(version, name, data), signature)
        .map_err(|_| UpdateError::BadSignature)
}

/// The downloaded file is what it should be for this install: an ELF or PE binary, or a
/// zstd-compressed pacman package. Checked before anything replaces the running program.
fn looks_installable(kind: InstallKind, data: &[u8]) -> bool {
    match kind {
        InstallKind::Pacman => data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]),
        InstallKind::Binary if cfg!(windows) => data.starts_with(b"MZ"),
        InstallKind::Binary => data.starts_with(b"\x7fELF"),
    }
}

/// Where the download goes: next to the binary (so the final rename stays on one file system)
/// or, for a package, into the temp dir.
fn target_path(release: &Release) -> Result<PathBuf, UpdateError> {
    match release.kind {
        InstallKind::Pacman => {
            // Not the shared /tmp: another local user could pre-create the directory there and
            // swap the package while pkexec waits for the password, and pacman (as root) would
            // install theirs. A 0700 directory in our own cache dir cannot be touched by others.
            let dir = private_update_dir().map_err(|e| UpdateError::Install(e.to_string()))?;
            Ok(dir.join(&release.asset_name))
        }
        InstallKind::Binary => {
            let exe = current_exe()
                .ok_or_else(|| UpdateError::Install("не найден путь к программе".into()))?;
            Ok(new_path(&exe))
        }
    }
}

/// `~/.cache/tonearm/update`, owner-only. It lives in the user's home, where other users cannot
/// create or replace anything, and it is refused if something other than a directory is there.
fn private_update_dir() -> std::io::Result<PathBuf> {
    let dir = directories::ProjectDirs::from("", "", "tonearm")
        .map(|d| d.cache_dir().join("update"))
        .ok_or_else(|| std::io::Error::other("нет каталога кэша"))?;
    std::fs::create_dir_all(&dir)?;
    if !std::fs::symlink_metadata(&dir)?.is_dir() {
        return Err(std::io::Error::other("каталог обновления подменён"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Creates `path` fresh (an old file or a planted link there is removed first, never followed).
fn create_fresh(path: &Path) -> std::io::Result<std::fs::File> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Downloads the release file and its signature, verifies it and writes it to disk.
/// `progress(done, total)` is called as bytes arrive (`total` is 0 when unknown).
pub async fn download(
    release: &Release,
    mut progress: impl FnMut(u64, u64) + Send,
) -> Result<Downloaded, UpdateError> {
    let path = target_path(release)?;
    // Fail before the download if the file cannot be written anyway.
    let file = match create_fresh(&path) {
        Ok(f) => f,
        Err(e) => {
            let dir = path.parent().unwrap_or(&path).display().to_string();
            return Err(if e.kind() == std::io::ErrorKind::PermissionDenied {
                UpdateError::NotWritable(dir)
            } else {
                UpdateError::Install(e.to_string())
            });
        }
    };
    let result = fetch_verified(release, &mut progress).await;
    let data = match result {
        Ok(d) => d,
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    };
    // Write into the file we created (not by path again: nothing can be swapped in between).
    let write = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut file = file;
        file.write_all(&data).and_then(|()| file.sync_all())
    })
    .await
    .map_err(|e| UpdateError::Install(e.to_string()))?;
    if let Err(e) = write {
        let _ = std::fs::remove_file(&path);
        return Err(UpdateError::Install(e.to_string()));
    }
    tracing::info!(file = %path.display(), version = %release.version, "update downloaded and verified");
    Ok(Downloaded {
        path,
        kind: release.kind,
    })
}

async fn fetch_verified(
    release: &Release,
    progress: &mut (impl FnMut(u64, u64) + Send),
) -> Result<Vec<u8>, UpdateError> {
    let http = client(None)?;
    let net = |e: reqwest::Error| UpdateError::Network(e.to_string());
    let sig_resp = http
        .get(&release.sig_url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(net)?;
    // A signature is 64 bytes: do not read an endless body into memory.
    if sig_resp.content_length().is_some_and(|n| n > 1024) {
        return Err(UpdateError::BadSignature);
    }
    let signature = sig_resp.bytes().await.map_err(net)?;
    if signature.len() != 64 {
        return Err(UpdateError::BadSignature);
    }
    let mut resp = http
        .get(&release.asset_url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(net)?;
    let total = resp.content_length().unwrap_or(0);
    if total > MAX_DOWNLOAD {
        return Err(UpdateError::Install(
            "файл обновления слишком большой".into(),
        ));
    }
    let mut data = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
    progress(0, total);
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(60), resp.chunk())
        .await
        .map_err(|_| UpdateError::Network("загрузка остановилась".into()))?
        .map_err(net)?
    {
        data.extend_from_slice(&chunk);
        if data.len() as u64 > MAX_DOWNLOAD {
            return Err(UpdateError::Install(
                "файл обновления слишком большой".into(),
            ));
        }
        progress(data.len() as u64, total);
    }
    verify(&release.version, &release.asset_name, &data, &signature)?;
    if !looks_installable(release.kind, &data) {
        return Err(UpdateError::Install(
            "файл обновления не похож на программу для этой системы".into(),
        ));
    }
    Ok(data)
}

/// Puts the downloaded version in place (blocking; pacman asks for the password here).
/// The running process keeps working; call [`restart`] after it exits.
pub fn install(file: Downloaded) -> Result<(), UpdateError> {
    match file.kind {
        InstallKind::Binary => replace_binary(&file.path),
        InstallKind::Pacman => install_package(&file.path),
    }
}

fn replace_binary(new: &Path) -> Result<(), UpdateError> {
    let exe =
        current_exe().ok_or_else(|| UpdateError::Install("не найден путь к программе".into()))?;
    let io = |e: std::io::Error| UpdateError::Install(e.to_string());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
        // Renaming over a running binary is fine on Unix: the process keeps the old inode.
        std::fs::rename(new, &exe).map_err(io)?;
    }
    #[cfg(windows)]
    {
        // Windows cannot overwrite a running exe, but it can rename it out of the way.
        let old = old_path(&exe);
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old).map_err(io)?;
        if let Err(e) = std::fs::rename(new, &exe) {
            let _ = std::fs::rename(&old, &exe);
            return Err(io(e));
        }
    }
    tracing::info!(exe = %exe.display(), "binary replaced");
    Ok(())
}

fn install_package(pkg: &Path) -> Result<(), UpdateError> {
    let status = std::process::Command::new("pkexec")
        .arg("pacman")
        .args(["-U", "--noconfirm"])
        .arg(pkg)
        .status();
    let _ = std::fs::remove_file(pkg);
    match status {
        Ok(s) if s.success() => {
            tracing::info!("package installed through pacman");
            Ok(())
        }
        // 126: the user closed the password dialog; 127: not authorised.
        Ok(s) if matches!(s.code(), Some(126 | 127)) => Err(UpdateError::Cancelled),
        Ok(s) => Err(UpdateError::Install(format!(
            "pacman завершился с кодом {}",
            s.code().unwrap_or(-1)
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(UpdateError::Install(
            "нет pkexec (пакет polkit), установите пакет вручную через pacman -U".into(),
        )),
        Err(e) => Err(UpdateError::Install(e.to_string())),
    }
}

/// Starts the (new) binary; call it once the old process has shut down its player.
pub fn restart() {
    let Some(exe) = current_exe() else { return };
    match std::process::Command::new(&exe).spawn() {
        Ok(_) => tracing::info!("restarted after the update"),
        Err(e) => tracing::warn!(error = %e, "cannot start the updated app"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(tag: &str, names: &[&str]) -> FeedRelease {
        FeedRelease {
            tag_name: tag.into(),
            body: Some("notes".into()),
            html_url: None,
            draft: false,
            prerelease: false,
            assets: names
                .iter()
                .map(|n| FeedAsset {
                    name: (*n).into(),
                    browser_download_url: format!("https://example.invalid/{n}"),
                })
                .collect(),
        }
    }

    #[test]
    fn feed_file_names_cannot_leave_the_directory() {
        assert!(plain_file_name("tonearm-0.3.0-1-x86_64.pkg.tar.zst"));
        assert!(!plain_file_name("tonearm-../../.bashrc-x86_64.pkg.tar.zst"));
        assert!(!plain_file_name("../tonearm-linux-x86_64"));
        assert!(!plain_file_name("..\\tonearm-windows-x86_64.exe"));
        assert!(!plain_file_name("C:tonearm-windows-x86_64.exe"));
        assert!(!plain_file_name(".hidden"));
        assert!(!plain_file_name(""));
    }

    #[test]
    fn only_https_and_github_pages() {
        assert!(https("https://github.com/x"));
        assert!(!https("http://mirror.local/x"));
        assert_eq!(
            safe_page(Some(
                "https://github.com/chelond/tonearm/releases/tag/v1".into()
            )),
            "https://github.com/chelond/tonearm/releases/tag/v1"
        );
        assert_eq!(safe_page(Some("file:///etc/passwd".into())), RELEASES_PAGE);
        assert_eq!(safe_page(Some("ms-settings:".into())), RELEASES_PAGE);
        assert_eq!(safe_page(None), RELEASES_PAGE);
    }

    #[test]
    fn http_download_links_are_refused() {
        let mut f = feed(
            "v99.0.0",
            &["tonearm-linux-x86_64", "tonearm-linux-x86_64.sig"],
        );
        for a in &mut f.assets {
            a.browser_download_url = a.browser_download_url.replace("https://", "http://");
        }
        if std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64" {
            assert!(pick(f, "0.1.0", InstallKind::Binary).is_err());
        }
    }

    #[test]
    fn create_fresh_does_not_follow_a_planted_link() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let path = dir.path().join("pkg");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        let mut f = create_fresh(&path).unwrap();
        std::io::Write::write_all(&mut f, b"new").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn versions() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("v1.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
        assert!(!is_newer("v0.2.0-rc1", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        // A beta gets the final release of its version, and anything newer.
        assert!(is_newer("v0.3.0", "0.3.0-beta.1"));
        assert!(is_newer("v0.3.1", "0.3.0-beta.1"));
        assert!(!is_newer("v0.2.9", "0.3.0-beta.1"));
        assert!(
            !is_newer("v0.3.0-beta.2", "0.3.0-beta.1"),
            "pre-releases are not offered"
        );
        assert!(
            !is_newer("v0.3.0+build.5", "0.3.0"),
            "build metadata is not a new version"
        );
    }

    #[test]
    fn signature_covers_version_and_file_name() {
        use ring::signature::{Ed25519KeyPair, KeyPair};
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let key = pair.public_key().as_ref();
        let data = b"\x7fELF old but signed";
        let sig = pair.sign(&signed_message("0.2.0", "tonearm-linux-x86_64", data));
        let ok = |v, n| verify_with(key, v, n, data, sig.as_ref()).is_ok();
        assert!(ok("0.2.0", "tonearm-linux-x86_64"));
        assert!(
            !ok("99.0.0", "tonearm-linux-x86_64"),
            "an old build under a new tag"
        );
        assert!(!ok("0.2.0", "tonearm-windows-x86_64.exe"), "another file");
    }

    #[test]
    fn only_a_program_replaces_the_program() {
        assert!(looks_installable(
            InstallKind::Pacman,
            &[0x28, 0xb5, 0x2f, 0xfd, 0]
        ));
        assert!(!looks_installable(InstallKind::Pacman, b"\x1f\x8b tarball"));
        let exe: &[u8] = if cfg!(windows) {
            b"MZ\x90"
        } else {
            b"\x7fELF\x02"
        };
        assert!(looks_installable(InstallKind::Binary, exe));
        assert!(!looks_installable(InstallKind::Binary, b"PK\x03\x04 zip"));
    }

    #[test]
    fn picks_platform_asset_with_signature() {
        let arch = std::env::consts::ARCH;
        let bin = if cfg!(windows) {
            format!("tonearm-windows-{arch}.exe")
        } else {
            format!("tonearm-linux-{arch}")
        };
        let pkg = format!("tonearm-0.2.0-1-{arch}.pkg.tar.zst");
        // The empty debug split package comes first and must be skipped.
        let debug = format!("tonearm-debug-0.2.0-1-{arch}.pkg.tar.zst");
        let names = [
            bin.clone(),
            format!("{bin}.sig"),
            debug.clone(),
            format!("{debug}.sig"),
            pkg.clone(),
            format!("{pkg}.sig"),
        ];
        let names: Vec<&str> = names.iter().map(String::as_str).collect();

        let r = pick(feed("v0.2.0", &names), "0.1.0", InstallKind::Binary)
            .unwrap()
            .unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.asset_name, bin);
        assert!(r.sig_url.ends_with(".sig"));
        assert_eq!(r.page, RELEASES_PAGE);

        if cfg!(target_os = "linux") {
            let r = pick(feed("v0.2.0", &names), "0.1.0", InstallKind::Pacman)
                .unwrap()
                .unwrap();
            assert_eq!(r.asset_name, pkg);
        }
        assert!(
            pick(feed("v0.1.0", &names), "0.1.0", InstallKind::Binary)
                .unwrap()
                .is_none()
        );
        // A file without its signature is never offered.
        assert!(matches!(
            pick(
                feed("v0.2.0", &[bin.as_str()]),
                "0.1.0",
                InstallKind::Binary
            ),
            Err(UpdateError::NoAsset)
        ));
    }

    #[test]
    fn rejects_unsigned_data() {
        assert!(matches!(
            verify("0.3.0", "tonearm-linux-x86_64", b"hello", &[0u8; 64]),
            Err(UpdateError::BadSignature)
        ));
    }

    #[test]
    fn sibling_names() {
        assert_eq!(
            sibling(Path::new("/a/tonearm"), "new"),
            Path::new("/a/tonearm.new")
        );
        assert_eq!(
            sibling(Path::new("C:/x/tonearm.exe"), "old"),
            Path::new("C:/x/tonearm.old.exe")
        );
    }
}
